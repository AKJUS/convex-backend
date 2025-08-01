use std::{
    collections::BTreeMap,
    ffi,
    ptr,
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use common::{
    knobs::{
        FUNRUN_INITIAL_PERMIT_TIMEOUT,
        ISOLATE_MAX_ARRAY_BUFFER_TOTAL_SIZE,
        ISOLATE_MAX_USER_HEAP_SIZE,
    },
    runtime::Runtime,
};
use deno_core::v8;
use derive_more::{
    Add,
    AddAssign,
};
use errors::ErrorMetadata;
use fastrace::{
    local::LocalSpan,
    Event,
};
use humansize::{
    FormatSize,
    BINARY,
};
use value::heap_size::WithHeapSize;

use crate::{
    array_buffer_allocator::ArrayBufferMemoryLimit,
    concurrency_limiter::ConcurrencyLimiter,
    environment::IsolateEnvironment,
    helpers::pump_message_loop,
    metrics::{
        create_isolate_timer,
        log_heap_statistics,
    },
    request_scope::RequestState,
    strings,
    termination::{
        IsolateHandle,
        TerminationReason,
    },
    timeout::Timeout,
};

pub const CONVEX_SCHEME: &str = "convex";
pub const SYSTEM_PREFIX: &str = "_system/";
pub const SETUP_URL: &str = "convex:/_system/setup.js";

/// Thin wrapper over `v8::Isolate` that includes our Convex-specific
/// configuration.
pub struct Isolate<RT: Runtime> {
    rt: RT,
    v8_isolate: v8::OwnedIsolate,
    handle: IsolateHandle,
    // Typically, the user timeout is configured based on environment. This
    // allows us to set an upper bound to it that we use for tests.
    max_user_timeout: Option<Duration>,
    // The heap limit callback takes ownership of this `Box` allocation, which
    // we reclaim after removing the callback.
    heap_ctx_ptr: *mut HeapContext,
    limiter: ConcurrencyLimiter,
    array_buffer_memory_limit: Arc<ArrayBufferMemoryLimit>,

    created: tokio::time::Instant,
}

#[derive(thiserror::Error, Debug)]
pub enum IsolateNotClean {
    #[error("Isolate failed with system error.")]
    SystemError,
    #[error("Isolate failed with uncatchable developer error.")]
    UncatchableDeveloperError,
    #[error("Isolate failed with unhandled promise rejection.")]
    UnhandledPromiseRejection,
    #[error("Isolate hit user timeout")]
    UserTimeout,
    #[error("Isolate hit system timeout")]
    SystemTimeout,
    #[error("Isolate ran out of memory")]
    OutOfMemory,

    #[error(
        "Possible memory leak: not enough room for user heap. Total available size {0} out of {1}."
    )]
    TooMuchMemoryCarryOver(String, String),
    #[error("Possible memory leak: {0} contexts have not been garbage collected.")]
    DetachedContext(usize),
}

impl IsolateNotClean {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::SystemError => "system_error",
            Self::UncatchableDeveloperError => "uncatchable_developer_error",
            Self::UnhandledPromiseRejection => "unhandled_promise_rejection",
            Self::UserTimeout => "user_timeout",
            Self::SystemTimeout => "system_timeout",
            Self::OutOfMemory => "out_of_memory",
            Self::TooMuchMemoryCarryOver(..) => "memory_carry_over",
            Self::DetachedContext(_) => "detached_context",
        }
    }
}

#[derive(Debug, Default, Copy, Clone, Add, AddAssign)]
pub struct IsolateHeapStats {
    pub v8_total_heap_size: usize,
    pub v8_total_heap_size_executable: usize,
    pub v8_total_physical_size: usize,
    pub v8_used_heap_size: usize,

    pub v8_external_memory_bytes: usize,

    // Malloced memory from v8. This is the heap size. See
    // https://stackoverflow.com/questions/69418109/why-is-malloced-memory-lower-than-used-heap-size-in-node-js.
    pub v8_malloced_memory: usize,

    // Heap used for syscalls and similar related to processing the request.
    pub environment_heap_size: usize,

    pub blobs_heap_size: usize,
    pub streams_heap_size: usize,
    pub array_buffer_size: usize,
}

impl IsolateHeapStats {
    pub fn new(
        stats: v8::HeapStatistics,
        blobs_heap_size: usize,
        streams_heap_size: usize,
        array_buffer_size: usize,
    ) -> Self {
        Self {
            v8_total_heap_size: stats.total_heap_size(),
            v8_total_heap_size_executable: stats.total_heap_size_executable(),
            v8_total_physical_size: stats.total_physical_size(),
            v8_used_heap_size: stats.used_heap_size(),
            v8_malloced_memory: stats.malloced_memory(),
            v8_external_memory_bytes: stats.external_memory(),
            environment_heap_size: 0,
            blobs_heap_size,
            streams_heap_size,
            array_buffer_size,
        }
    }

    pub fn env_heap_size(&self) -> usize {
        self.environment_heap_size + self.blobs_heap_size + self.streams_heap_size
    }
}

impl<RT: Runtime> Isolate<RT> {
    pub fn new(rt: RT, max_user_timeout: Option<Duration>, limiter: ConcurrencyLimiter) -> Self {
        let _timer = create_isolate_timer();
        let (array_buffer_memory_limit, array_buffer_allocator) =
            crate::array_buffer_allocator::limited_array_buffer_allocator(
                *ISOLATE_MAX_ARRAY_BUFFER_TOTAL_SIZE,
            );
        let mut v8_isolate = crate::udf_runtime::create_isolate_with_udf_runtime(
            v8::CreateParams::default().array_buffer_allocator(array_buffer_allocator),
        );

        // Tells V8 to capture current stack trace when uncaught exception occurs and
        // report it to the message listeners. The option is off by default.
        v8_isolate.set_capture_stack_trace_for_uncaught_exceptions(
            true, // capture
            10,   // frame_limit
        );

        // This specifies the callback called by the upcoming import.meta language
        // feature to retrieve host-defined meta data for a module.
        v8_isolate.set_host_initialize_import_meta_object_callback(Self::import_meta_callback);

        // This specifies the callback called by the upcoming dynamic import() language
        // feature to load modules.
        v8_isolate.set_host_import_module_dynamically_callback(Self::error_dynamic_import_callback);

        // Disallow synchronous `Atomics.wait`.
        v8_isolate.set_allow_atomics_wait(false);

        v8_isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);

        let handle = IsolateHandle::new(v8_isolate.thread_safe_handle());

        // Pass ownership of the HeapContext struct to the heap limit callback, which
        // we'll take back in the `Isolate`'s destructor.
        let heap_context = Box::new(HeapContext {
            handle: handle.clone(),
        });
        let heap_ctx_ptr = Box::into_raw(heap_context);
        v8_isolate.add_near_heap_limit_callback(
            near_heap_limit_callback,
            heap_ctx_ptr as *mut ffi::c_void,
        );

        assert!(v8_isolate.set_slot(handle.clone()));
        assert!(v8_isolate.set_slot(array_buffer_memory_limit.clone()));

        Self {
            created: rt.monotonic_now(),
            rt,
            v8_isolate,
            handle,
            heap_ctx_ptr,
            max_user_timeout,
            limiter,
            array_buffer_memory_limit,
        }
    }

    extern "C" fn import_meta_callback(
        context: v8::Local<v8::Context>,
        _module: v8::Local<v8::Module>,
        _meta: v8::Local<v8::Object>,
    ) {
        let scope = &mut unsafe { v8::CallbackScope::new(context) };
        let message = strings::import_meta_unsupported
            .create(scope)
            .expect("Failed to create exception string");
        let exception = v8::Exception::type_error(scope, message);
        scope.throw_exception(exception);
    }

    pub fn error_dynamic_import_callback<'s>(
        scope: &mut v8::HandleScope<'s>,
        _host_defined_options: v8::Local<'s, v8::Data>,
        _resource_name: v8::Local<'s, v8::Value>,
        _specifier: v8::Local<'s, v8::String>,
        _import_assertions: v8::Local<'s, v8::FixedArray>,
    ) -> Option<v8::Local<'s, v8::Promise>> {
        let message = strings::dynamic_import_unsupported
            .create(scope)
            .expect("Failed to crate exception string");
        let exception = v8::Exception::type_error(scope, message);

        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        resolver.reject(scope, exception);
        Some(promise)
    }

    // Heap stats for an isolate that has no associated state or environment.
    pub fn heap_stats(&mut self) -> IsolateHeapStats {
        let stats = self.v8_isolate.get_heap_statistics();
        IsolateHeapStats::new(stats, 0, 0, self.array_buffer_memory_limit.used())
    }

    pub fn check_isolate_clean(&mut self) -> Result<(), IsolateNotClean> {
        // The microtask queue should be empty.
        // v8 doesn't expose whether it's empty, so we empty it ourselves.
        // TODO(CX-2874) use a different microtask queue for each context.
        self.v8_isolate.perform_microtask_checkpoint();
        pump_message_loop(&mut self.v8_isolate);

        // Isolate has not been terminated by heap overflow, system error, or timeout.
        if let Some(not_clean) = self.handle.is_not_clean() {
            return Err(not_clean);
        }
        // The heap should have enough memory available.
        let stats = self.v8_isolate.get_heap_statistics();
        log_heap_statistics(&stats);
        if stats.total_available_size() < *ISOLATE_MAX_USER_HEAP_SIZE {
            self.handle.terminate(TerminationReason::OutOfMemory);
            return Err(IsolateNotClean::TooMuchMemoryCarryOver(
                stats.total_available_size().format_size(BINARY),
                stats.heap_size_limit().format_size(BINARY),
            ));
        }
        if stats.number_of_detached_contexts() > 0 {
            return Err(IsolateNotClean::DetachedContext(
                stats.number_of_detached_contexts(),
            ));
        }

        Ok(())
    }

    pub async fn start_request<E: IsolateEnvironment<RT>>(
        &mut self,
        client_id: Arc<String>,
        environment: E,
    ) -> anyhow::Result<(IsolateHandle, RequestState<RT, E>)> {
        // Double check that the isolate is clean.
        // It's unexpected to encounter this error, since we are supposed to
        // have already checked after the last request finished, but in practice
        // it does happen - so make this error retryable.
        self.check_isolate_clean()
            .context(ErrorMetadata::rejected_before_execution(
                "IsolateNotClean",
                "Selected isolate was not clean",
            ))?;
        // Acquire a concurrency permit without counting it against the timeout.
        let permit = tokio::select! {
            biased;
            permit = self.limiter.acquire(client_id) => permit,
            // Do not apply a timeout for subfunctions that can't be retried
            () = self.rt.wait(*FUNRUN_INITIAL_PERMIT_TIMEOUT),
                    if !environment.is_nested_function() => {
                anyhow::bail!(ErrorMetadata::rejected_before_execution(
                    "InitialPermitTimeoutError",
                    "Couldn't acquire a permit on this funrun",
                ));
            }
        };
        let context_handle = self.handle.new_context_created();
        let mut user_timeout = environment.user_timeout();
        if let Some(max_user_timeout) = self.max_user_timeout {
            // We apply the minimum between the timeout from the environment
            // and the max_user_timeout, that is set from tets.
            user_timeout = user_timeout.min(max_user_timeout);
        }
        let timeout = Timeout::new(
            self.rt.clone(),
            context_handle,
            Some(user_timeout),
            Some(environment.system_timeout()),
        );
        let state = RequestState {
            rt: self.rt.clone(),
            environment,
            timeout,
            permit: Some(permit),
            blob_parts: WithHeapSize::default(),
            streams: WithHeapSize::default(),
            stream_listeners: WithHeapSize::default(),
            request_stream_state: None,
            console_timers: WithHeapSize::default(),
            text_decoders: BTreeMap::new(),
        };
        Ok((self.handle.clone(), state))
    }

    pub fn handle_scope(&mut self) -> v8::HandleScope<()> {
        v8::HandleScope::new(&mut self.v8_isolate)
    }

    pub fn created(&self) -> &tokio::time::Instant {
        &self.created
    }
}

impl<RT: Runtime> Drop for Isolate<RT> {
    fn drop(&mut self) {
        if !self.heap_ctx_ptr.is_null() {
            // First remove the callback, so V8 can no longer invoke it.
            self.v8_isolate
                .remove_near_heap_limit_callback(near_heap_limit_callback, 0);

            // Now that the callback is gone, we can free its context.
            let heap_ctx = unsafe { Box::from_raw(self.heap_ctx_ptr) };
            drop(heap_ctx);

            self.heap_ctx_ptr = ptr::null_mut();
        }

        // XXX: our version of rusty_v8 is missing a call to
        // NotifyIsolateShutdown, so the isolate's foreground task runner is
        // going to leak. Before that happens, let's pump the message loop to at
        // least drain all the tasks from it.
        pump_message_loop(&mut self.v8_isolate);
    }
}

struct HeapContext {
    handle: IsolateHandle,
}

extern "C" fn near_heap_limit_callback(
    data: *mut ffi::c_void,
    current_heap_limit: usize,
    initial_heap_limit: usize,
) -> usize {
    LocalSpan::add_event(Event::new("isolate_out_of_memory").with_properties(|| {
        [
            ("current_heap_limit", current_heap_limit.to_string()),
            ("initial_heap_limit", initial_heap_limit.to_string()),
        ]
    }));
    let heap_ctx = unsafe { &mut *(data as *mut HeapContext) };
    heap_ctx.handle.terminate(TerminationReason::OutOfMemory);

    // Raise the heap limit a lot to avoid a hard OOM.
    // This is unfortunate but there is some C++ code in V8 that will abort the
    // process if it fails to allocate. We're about to terminate the isolate
    // anyway so any allocation will be very short-lived.
    current_heap_limit * 4
}
