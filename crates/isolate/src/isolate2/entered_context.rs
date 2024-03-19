use std::mem;

use anyhow::anyhow;
use common::errors::JsError;
use deno_core::{
    v8::{
        self,
    },
    ModuleSpecifier,
};
use errors::ErrorMetadata;
use futures::future::Either;
use serde_json::Value as JsonValue;

use super::{
    client::{
        AsyncSyscallCompletion,
        PendingAsyncSyscall,
    },
    context_state::ContextState,
};
use crate::{
    helpers::{
        self,
        to_rust_string,
    },
    isolate2::context::Context,
};

// 'scope can either be 'session or 'callback
pub struct EnteredContext<'enter, 'scope: 'enter> {
    scope: &'enter mut v8::HandleScope<'scope>,
    context: v8::Local<'scope, v8::Context>,
}

impl<'enter, 'scope: 'enter> EnteredContext<'enter, 'scope> {
    pub fn new(
        scope: &'enter mut v8::HandleScope<'scope>,
        context: v8::Local<'scope, v8::Context>,
    ) -> Self {
        Self { scope, context }
    }

    pub fn from_callback(scope: &'enter mut v8::HandleScope<'scope>) -> Self {
        let context = scope.get_current_context();
        Self { scope, context }
    }

    pub fn context_state_mut(&mut self) -> anyhow::Result<&mut ContextState> {
        self.context
            .get_slot_mut::<ContextState>(self.scope)
            .ok_or_else(|| anyhow::anyhow!("ContextState not found in context"))
    }

    pub fn context_state(&mut self) -> anyhow::Result<&ContextState> {
        self.context
            .get_slot::<ContextState>(self.scope)
            .ok_or_else(|| anyhow::anyhow!("ContextState not found in context"))
    }

    // NB: This can be called from the top-level (i.e. entering from the context
    // into user code) but also from within a callback (e.g. following an object
    // property in an op handler).
    pub fn execute_user_code<R>(
        &mut self,
        f: impl FnOnce(&mut v8::HandleScope<'scope>) -> R,
    ) -> anyhow::Result<R> {
        let mut tc_scope = v8::TryCatch::new(self.scope);
        let r = f(&mut tc_scope);
        if let Some(e) = tc_scope.exception() {
            drop(tc_scope);
            return Err(self.format_traceback(e)?.into());
        }
        drop(tc_scope);
        // XXX: check terminating error here. (call to unsupported syscall)
        if self.scope.is_execution_terminating() {
            anyhow::bail!("Execution terminated");
        }
        // Executing just about any user code can lead to an unhandled promise
        // rejection (e.g. calling `Promise.reject`). However, it's important
        // to only fail the session when we receive control... XXX explain more.
        let promise_rejection = {
            let context_state = self.context_state_mut()?;

            // Only use the first unhandled promise rejection.
            let rejection = context_state.unhandled_promise_rejections.drain().next();
            context_state.unhandled_promise_rejections.clear();
            rejection
        };
        if let Some((_promise, error_global)) = promise_rejection {
            let error = v8::Local::new(self.scope, error_global);
            let err = self.format_traceback(error)?;

            // XXX: how do we want to plumb this to the termination stuff?
            anyhow::bail!("Unhandled promise rejection: {err:?}");
        }
        Ok(r)
    }

    pub fn register_module(
        &mut self,
        url: &ModuleSpecifier,
        source: &str,
    ) -> anyhow::Result<Vec<ModuleSpecifier>> {
        {
            let context_state = self.context_state_mut()?;
            anyhow::ensure!(
                !context_state.module_map.contains_module(url),
                "Module already registered"
            );
        }
        let name_str = v8::String::new(self.scope, url.as_str())
            .ok_or_else(|| anyhow!("Failed to create name string"))?;
        let source_str = v8::String::new(self.scope, source)
            .ok_or_else(|| anyhow!("Failed to create source string"))?;

        let origin = helpers::module_origin(self.scope, name_str);
        let v8_source = v8::script_compiler::Source::new(source_str, Some(&origin));

        let module = self
            .execute_user_code(|s| v8::script_compiler::compile_module(s, v8_source))?
            .ok_or_else(|| anyhow!("Unexpected module compilation error"))?;

        anyhow::ensure!(module.get_status() == v8::ModuleStatus::Uninstantiated);
        let mut import_specifiers: Vec<ModuleSpecifier> = vec![];
        let module_requests = module.get_module_requests();
        for i in 0..module_requests.length() {
            let module_request: v8::Local<v8::ModuleRequest> = module_requests
                .get(self.scope, i)
                .ok_or_else(|| anyhow!("Module request {} out of bounds", i))?
                .try_into()?;
            let import_specifier =
                helpers::to_rust_string(self.scope, &module_request.get_specifier())?;
            let module_specifier = deno_core::resolve_import(&import_specifier, url.as_str())?;
            import_specifiers.push(module_specifier);
        }
        let module = v8::Global::new(self.scope, module);
        let unresolved_imports = {
            let context_state = self.context_state_mut()?;
            import_specifiers.retain(|s| !context_state.module_map.contains_module(s));
            context_state.module_map.register(url.clone(), module)?;
            import_specifiers
        };
        Ok(unresolved_imports)
    }

    pub fn evaluate_module(&mut self, url: &ModuleSpecifier) -> anyhow::Result<()> {
        let module_global = {
            let context_state = self.context_state()?;
            context_state
                .module_map
                .modules
                .get(url)
                .ok_or_else(|| anyhow!("Module not registered"))?
                .clone()
        };
        let module = v8::Local::new(self.scope, module_global);
        match module.get_status() {
            v8::ModuleStatus::Uninstantiated => (),
            s => anyhow::bail!("Module {url} is in invalid state: {s:?}"),
        }

        let instantiation_result = self.execute_user_code(|s| {
            module.instantiate_module(s, Context::module_resolve_callback)
        })?;

        if matches!(instantiation_result, Some(false) | None) {
            anyhow::bail!("Unexpected successful instantiate result: {instantiation_result:?}");
        }
        anyhow::ensure!(module.get_status() == v8::ModuleStatus::Instantiated);

        let evaluation_result = self
            .execute_user_code(|s| module.evaluate(s))?
            .ok_or_else(|| anyhow!("Missing result from successful module evaluation"))?;

        let status = module.get_status();
        anyhow::ensure!(
            status == v8::ModuleStatus::Evaluated || status == v8::ModuleStatus::Errored
        );
        let promise = v8::Local::<v8::Promise>::try_from(evaluation_result)
            .map_err(|e| anyhow!("Module evaluation did not return a promise: {:?}", e))?;
        match promise.state() {
            v8::PromiseState::Pending => {
                anyhow::bail!(JsError::from_message(
                    "Top-level awaits in source files are unsupported".to_string()
                ))
            },
            v8::PromiseState::Fulfilled => {
                anyhow::ensure!(status == v8::ModuleStatus::Evaluated);
            },
            v8::PromiseState::Rejected => {
                let e = promise.result(self.scope);
                return Err(self.format_traceback(e)?.into());
            },
        }
        Ok(())
    }

    pub fn resolve_module(
        &mut self,
        specifier: v8::Local<'scope, v8::String>,
        referrer: v8::Local<'scope, v8::Module>,
    ) -> Option<v8::Local<'scope, v8::Module>> {
        let r: anyhow::Result<_> = try {
            let referrer_global = v8::Global::new(self.scope, referrer);
            let specifier_str = helpers::to_rust_string(self.scope, &specifier)?;
            let context_state = self.context_state()?;
            let referrer_name = context_state
                .module_map
                .by_v8_module
                .get(&referrer_global)
                .ok_or_else(|| anyhow!("Module not registered"))?
                .to_string();
            let resolved_specifier = deno_core::resolve_import(&specifier_str, &referrer_name)?;
            let module = context_state
                .module_map
                .modules
                .get(&resolved_specifier)
                .ok_or_else(|| anyhow!("Couldn't find {resolved_specifier}"))?
                .clone();
            v8::Local::new(self.scope, module)
        };
        match r {
            Ok(m) => Some(m),
            Err(e) => {
                // XXX: This should be a system error!
                helpers::throw_type_error(self.scope, format!("{:?}", e));
                None
            },
        }
    }

    pub fn start_dynamic_import(
        &mut self,
        resource_name: v8::Local<'scope, v8::Value>,
        specifier: v8::Local<'scope, v8::String>,
    ) -> anyhow::Result<v8::Local<'scope, v8::Promise>> {
        let promise_resolver = v8::PromiseResolver::new(self.scope)
            .ok_or_else(|| anyhow::anyhow!("Failed to create v8::PromiseResolver"))?;

        let promise = promise_resolver.get_promise(self.scope);
        let resolver = v8::Global::new(self.scope, promise_resolver);

        let resource_name: v8::Local<v8::String> = resource_name.try_into()?;
        let referrer_name = helpers::to_rust_string(self.scope, &resource_name)?;
        let specifier_str = helpers::to_rust_string(self.scope, &specifier)?;

        let resolved_specifier = deno_core::resolve_import(&specifier_str, &referrer_name)
            .map_err(|e| ErrorMetadata::bad_request("InvalidImport", e.to_string()))?;

        self.context_state_mut()?
            .pending_dynamic_imports
            .push((resolved_specifier, resolver));

        Ok(promise)
    }

    pub fn start_evaluate_function(
        &mut self,
        url: &ModuleSpecifier,
        name: &str,
    ) -> anyhow::Result<Either<String, (v8::Global<v8::Promise>, Vec<PendingAsyncSyscall>)>> {
        let module_global = {
            let context_state = self.context_state()?;
            context_state
                .module_map
                .modules
                .get(url)
                .ok_or_else(|| anyhow!("Module not registered"))?
                .clone()
        };
        let module = v8::Local::new(self.scope, module_global);
        match module.get_status() {
            v8::ModuleStatus::Evaluated => (),
            s => anyhow::bail!("Module is in invalid state: {s:?}"),
        }

        let exports = module.get_module_namespace();
        let exports = v8::Local::new(self.scope, exports);
        let exports = exports
            .to_object(self.scope)
            .ok_or_else(|| anyhow!("Module exports not an object"))?;

        let name_str = v8::String::new(self.scope, name)
            .ok_or_else(|| anyhow::anyhow!("Failed to create name string"))?;
        let function: v8::Local<v8::Function> = exports
            .get(self.scope, name_str.into())
            .ok_or_else(|| anyhow::anyhow!("Function {name} not found in module"))?
            .try_into()?;

        let global = self.scope.get_current_context().global(self.scope);
        let promise: v8::Local<v8::Promise> = self
            .execute_user_code(|s| function.call(s, global.into(), &[]))?
            .ok_or_else(|| anyhow!("Function {name} did not return a result"))?
            .try_into()?;

        match promise.state() {
            v8::PromiseState::Pending => {
                let promise = v8::Global::new(self.scope, promise);
                let pending = mem::take(&mut self.context_state_mut()?.pending_async_syscalls);
                Ok(Either::Right((promise, pending)))
            },
            v8::PromiseState::Fulfilled => {
                let result: v8::Local<v8::String> = promise.result(self.scope).try_into()?;
                let result = helpers::to_rust_string(self.scope, &result)?;
                Ok(Either::Left(result))
            },
            v8::PromiseState::Rejected => {
                todo!()
            },
        }
    }

    pub fn poll_function(
        &mut self,
        completions: Vec<AsyncSyscallCompletion>,
        promise: &v8::Global<v8::Promise>,
    ) -> anyhow::Result<Either<String, (v8::Global<v8::Promise>, Vec<PendingAsyncSyscall>)>> {
        let completed = {
            let context_state = self.context_state_mut()?;
            let mut completed = vec![];
            for completion in completions {
                let resolver = context_state
                    .promise_resolvers
                    .remove(&completion.promise_id)
                    .ok_or_else(|| anyhow!("Promise resolver not found"))?;
                completed.push((resolver, completion.result));
            }
            completed
        };
        for (resolver, result) in completed {
            let resolver = v8::Local::new(self.scope, resolver);
            match result {
                Ok(v) => {
                    let s = serde_json::to_string(&v)?;
                    let v = v8::String::new(self.scope, &s)
                        .ok_or_else(|| anyhow!("Failed to create result string"))?;
                    resolver.resolve(self.scope, v.into());
                },
                Err(e) => {
                    let message = v8::String::new(self.scope, &e.message)
                        .ok_or_else(|| anyhow!("Failed to create error message string"))?;
                    let exception = v8::Exception::error(self.scope, message);
                    resolver.reject(self.scope, exception);
                },
            };
        }

        self.execute_user_code(|s| s.perform_microtask_checkpoint())?;

        let promise = v8::Local::new(self.scope, promise);
        match promise.state() {
            v8::PromiseState::Pending => {
                let promise = v8::Global::new(self.scope, promise);
                let pending = mem::take(&mut self.context_state_mut()?.pending_async_syscalls);
                Ok(Either::Right((promise, pending)))
            },
            v8::PromiseState::Fulfilled => {
                let result: v8::Local<v8::String> = promise.result(self.scope).try_into()?;
                let result = helpers::to_rust_string(self.scope, &result)?;
                Ok(Either::Left(result))
            },
            v8::PromiseState::Rejected => {
                todo!()
            },
        }
    }

    pub fn syscall(
        &mut self,
        args: v8::FunctionCallbackArguments,
    ) -> anyhow::Result<v8::Local<'scope, v8::Value>> {
        if args.length() != 2 {
            // There's not really an expected developer mistake that would lead to them
            // calling Convex.syscall incorrectly -- the bug must be in our
            // convex/server code. Treat this as a system error.
            anyhow::bail!("syscall(name, arg_object) takes two arguments");
        }
        let name: v8::Local<v8::String> = args.get(0).try_into()?;
        let name = to_rust_string(self.scope, &name)?;

        let args_v8: v8::Local<v8::String> = args.get(1).try_into()?;
        let args_s = to_rust_string(self.scope, &args_v8)?;
        let args_v: JsonValue = serde_json::from_str(&args_s).map_err(|e| {
            anyhow::anyhow!(ErrorMetadata::bad_request(
                "SyscallArgsInvalidJson",
                format!("Received invalid json: {e}"),
            ))
        })?;

        let result = self
            .context_state_mut()?
            .environment
            .syscall(&name, args_v)?;

        let value_s = serde_json::to_string(&result)?;
        let value_v8 = v8::String::new(self.scope, &value_s[..])
            .ok_or_else(|| anyhow!("Failed to create result string"))?;

        Ok(value_v8.into())
    }

    pub fn start_async_syscall(
        &mut self,
        args: v8::FunctionCallbackArguments,
    ) -> anyhow::Result<v8::Local<'scope, v8::Promise>> {
        if args.length() != 2 {
            // There's not really an expected developer mistake that would lead to them
            // calling Convex.asyncSyscall incorrectly -- the bug must be in our
            // convex/server code. Treat this as a system error.
            anyhow::bail!("asyncSyscall(name, arg_object) takes two arguments");
        }
        let name: v8::Local<v8::String> = args.get(0).try_into()?;
        let name = to_rust_string(self.scope, &name)?;

        let args_v8: v8::Local<v8::String> = args.get(1).try_into()?;
        let args_s = to_rust_string(self.scope, &args_v8)?;
        let args_v: JsonValue = serde_json::from_str(&args_s).map_err(|e| {
            anyhow::anyhow!(ErrorMetadata::bad_request(
                "SyscallArgsInvalidJson",
                format!("Received invalid json: {e}"),
            ))
        })?;

        let promise_resolver = v8::PromiseResolver::new(self.scope)
            .ok_or_else(|| anyhow::anyhow!("Failed to create v8::PromiseResolver"))?;

        let promise = promise_resolver.get_promise(self.scope);
        let resolver = v8::Global::new(self.scope, promise_resolver);
        {
            let context_state = self.context_state_mut()?;

            let promise_id = context_state.next_promise_id;
            context_state.next_promise_id += 1;

            let pending_async_syscall = PendingAsyncSyscall {
                promise_id,
                name,
                args: args_v,
            };
            context_state
                .pending_async_syscalls
                .push(pending_async_syscall);

            context_state.promise_resolvers.insert(promise_id, resolver);
        };
        Ok(promise)
    }

    pub fn push_unhandled_promise_rejection(
        &mut self,
        message: v8::PromiseRejectMessage,
    ) -> anyhow::Result<()> {
        match message.get_event() {
            v8::PromiseRejectEvent::PromiseRejectWithNoHandler => {
                // See comment on PendingUnhandledPromiseRejections.
                // A promise rejection is necessary but not sufficient for an
                // 'unhandledRejection' event, which throws in our runtime.
                // Save the promise and check back in on it once the microtask
                // queue has drained. If it remains unhandled then, throw.
                let Some(e) = message.get_value() else {
                    tracing::warn!("Message missing from call to promise_reject_callback");
                    return Ok(());
                };
                let error_global = v8::Global::new(self.scope, e);
                let promise_global = v8::Global::new(self.scope, message.get_promise());
                self.context_state_mut()?
                    .unhandled_promise_rejections
                    .insert(promise_global, error_global);
            },
            v8::PromiseRejectEvent::PromiseHandlerAddedAfterReject => {
                tracing::warn!("Promise handler added after reject");
                // If this promise was previously a candidate for an
                // 'unhandledRejection' event, disqualify it by removing it
                // from `pending_unhandled_promise_rejections`.
                let promise_global = v8::Global::new(self.scope, message.get_promise());
                self.context_state_mut()?
                    .unhandled_promise_rejections
                    .remove(&promise_global);
                // log_promise_handler_added_after_reject();
            },
            v8::PromiseRejectEvent::PromiseRejectAfterResolved => {
                tracing::warn!("Promise rejected after resolved");
            },
            v8::PromiseRejectEvent::PromiseResolveAfterResolved => {
                tracing::warn!("Promise resolved after resolved");
            },
        }
        Ok(())
    }

    pub fn format_traceback(&mut self, exception: v8::Local<v8::Value>) -> anyhow::Result<JsError> {
        // XXX: check if terminated
        // XXX: collect unsourcemapped error here and let the tokio thread do
        // sourcemapping if needed.
        let message = v8::Exception::create_message(self.scope, exception);
        let message = message.get(self.scope);
        let message = to_rust_string(self.scope, &message)?;
        Ok(JsError::from_message(message))
    }
}
