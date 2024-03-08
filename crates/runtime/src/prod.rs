//! Production implementation of the Runtime trait.

use std::{
    collections::HashMap,
    future::Future,
    marker::Send,
    ops::{
        Add,
        Sub,
    },
    pin::Pin,
    sync::LazyLock,
    thread,
    time::{
        Instant,
        SystemTime,
    },
};

use ::metrics::CONVEX_METRICS_REGISTRY;
use async_trait::async_trait;
use common::{
    heap_size::HeapSize,
    knobs::RUNTIME_WORKER_THREADS,
    runtime::{
        JoinError,
        Nanos,
        Runtime,
        RuntimeInstant,
        SpawnHandle,
    },
};
use futures::{
    channel::oneshot,
    future::FusedFuture,
    FutureExt,
    TryFutureExt,
};
use parking_lot::Mutex;
use rand::rngs::ThreadRng;
use tokio::{
    runtime::{
        Builder,
        Handle as TokioRuntimeHandle,
        Runtime as TokioRuntime,
    },
    time::{
        sleep,
        Duration,
    },
};
use tokio_metrics_collector::TaskMonitor;

static INSTANT_EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Set a consistent thread stack size regardless of environment. This is
/// 2x Rust's default: https://doc.rust-lang.org/nightly/std/thread/index.html#stack-size
pub const STACK_SIZE: usize = 4 * 1024 * 1024;

pub struct FutureHandle {
    handle: tokio::task::JoinHandle<()>,
}

impl SpawnHandle for FutureHandle {
    type Future = Pin<Box<dyn Future<Output = Result<(), JoinError>>>>;

    fn shutdown(&mut self) {
        self.handle.abort();
    }

    fn into_join_future(self) -> Self::Future {
        self.handle.map_err(|e| e.into()).boxed()
    }
}

pub struct ThreadHandle {
    cancel: Option<oneshot::Sender<()>>,
    done: oneshot::Receiver<bool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SpawnHandle for ThreadHandle {
    type Future = Pin<Box<dyn Future<Output = Result<(), JoinError>>>>;

    fn shutdown(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }

    fn into_join_future(mut self) -> Self::Future {
        let future = async move {
            // If the future exited cleanly, use its result.
            if let Ok(was_canceled) = self.done.await {
                return if !was_canceled {
                    Ok(())
                } else {
                    Err(JoinError::Canceled)
                };
            }
            let join_r = self.handle.take().expect("Future completed twice?").join();
            // Otherwise look at the result from `std::thread` to see if it panicked.
            let join_err = join_r.expect_err("Future didn't exit cleanly but didn't panic?");
            Err(JoinError::Panicked(anyhow::anyhow!("{:?}", join_err)))
        };
        future.boxed()
    }
}

impl ThreadHandle {
    fn spawn<Fut, F>(tokio_handle: TokioRuntimeHandle, f: F) -> Self
    where
        Fut: Future<Output = ()>,
        F: FnOnce() -> Fut + Send + 'static,
    {
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let thread_handle = thread::spawn(move || {
            let _guard = tokio_handle.enter();
            let thread_body = async move {
                let future = f();
                let was_canceled = futures::select! {
                    _ = cancel_rx => true,
                    _ = future.fuse() => false,
                };
                let _ = done_tx.send(was_canceled);
            };
            tokio_handle.block_on(thread_body);
        });
        ThreadHandle {
            handle: Some(thread_handle),
            cancel: Some(cancel_tx),
            done: done_rx,
        }
    }
}

/// Runtime for running in production that sleeps for wallclock time, doesn't
/// mock out any functionality, etc.
#[derive(Clone)]
pub struct ProdRuntime {
    rt: TokioRuntimeHandle,
}

impl ProdRuntime {
    pub fn init_tokio() -> anyhow::Result<TokioRuntime> {
        assert!(
            TokioRuntimeHandle::try_current().is_err(),
            "Tried to create a `ProdRuntime` from within a Tokio context. Are you using \
             `#[tokio::main]` or `#[tokio::test]`?"
        );
        let mut tokio_builder = Builder::new_multi_thread();
        tokio_builder.thread_stack_size(STACK_SIZE);
        if *RUNTIME_WORKER_THREADS > 0 {
            tokio_builder.worker_threads(*RUNTIME_WORKER_THREADS);
        }
        let tokio_rt = tokio_builder.enable_all().build()?;
        Ok(tokio_rt)
    }

    pub fn task_monitor(name: &'static str) -> TaskMonitor {
        GLOBAL_TASK_MANAGER.lock().get(name)
    }

    /// Create a new tokio-based runtime.
    /// Expected usage:
    /// ```rust
    /// use runtime::prod::ProdRuntime;
    /// fn main() -> anyhow::Result<()> {
    ///     let tokio = ProdRuntime::init_tokio()?;
    ///     let rt = ProdRuntime::new(&tokio);
    ///     rt.block_on(async {});
    ///     Ok(())
    /// }
    /// ```
    /// The `tokio_rt` should live for the duration of `main`.
    /// At the end of `main` its `Drop` will run and join all spawned futures,
    /// which should include all references to the handle `ProdRuntime`.
    /// If `ProdRuntime` is used after the associated `TokioRuntime` has been
    /// dropped, it will panic.
    pub fn new(tokio_rt: &TokioRuntime) -> Self {
        let handle = tokio_rt.handle().clone();

        Self { rt: handle }
    }

    pub fn block_on<F: Future>(&self, name: &'static str, f: F) -> F::Output {
        let monitor = GLOBAL_TASK_MANAGER.lock().get(name);
        self.rt.block_on(monitor.instrument(f))
    }
}

#[async_trait]
impl Runtime for ProdRuntime {
    type Handle = FutureHandle;
    type Instant = ProdInstant;
    type Rng = ThreadRng;
    type ThreadHandle = ThreadHandle;

    fn wait(&self, duration: Duration) -> Pin<Box<dyn FusedFuture<Output = ()> + Send + 'static>> {
        Box::pin(sleep(duration).fuse())
    }

    fn spawn(
        &self,
        name: &'static str,
        f: impl Future<Output = ()> + Send + 'static,
    ) -> FutureHandle {
        let monitor = GLOBAL_TASK_MANAGER.lock().get(name);
        let handle = self.rt.spawn(monitor.instrument(f));
        FutureHandle { handle }
    }

    fn spawn_thread<Fut: Future<Output = ()>, F: FnOnce() -> Fut + Send + 'static>(
        &self,
        f: F,
    ) -> ThreadHandle {
        ThreadHandle::spawn(self.rt.clone(), f)
    }

    fn system_time(&self) -> SystemTime {
        SystemTime::now()
    }

    fn monotonic_now(&self) -> ProdInstant {
        // Guarantee that all `ProdInstant`s handed out are after `SYNC_EPOCH`.
        LazyLock::force(&INSTANT_EPOCH);
        ProdInstant(Instant::now())
    }

    fn with_rng<R>(&self, f: impl FnOnce(&mut Self::Rng) -> R) -> R {
        // `rand`'s default RNG is designed to be cryptographically secure:
        // > The PRNG algorithm in StdRng is chosen to be efficient on the current
        // platform, to be > statistically strong and unpredictable (meaning a
        // cryptographically secure PRNG). (Source: https://docs.rs/rand/latest/rand/rngs/struct.StdRng.html)
        let mut rng = rand::thread_rng();
        f(&mut rng)
    }
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct ProdInstant(Instant);

impl Sub for ProdInstant {
    type Output = Duration;

    fn sub(self, rhs: Self) -> Duration {
        self.0 - rhs.0
    }
}

impl Add<Duration> for ProdInstant {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self {
        Self(self.0 + rhs)
    }
}

impl RuntimeInstant for ProdInstant {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    fn as_nanos(&self) -> Nanos {
        let nanos_u128 = self
            .0
            .checked_duration_since(*INSTANT_EPOCH)
            .expect("Created an ProdInstant before INSTANT_EPOCH?")
            .as_nanos();
        let nanos_u64 =
            u64::try_from(nanos_u128).expect("Program duration lasted longer than 584 years?");
        Nanos::new(nanos_u64)
    }
}

impl HeapSize for ProdInstant {
    #[inline]
    fn heap_size(&self) -> usize {
        0
    }
}

static GLOBAL_TASK_MANAGER: LazyLock<Mutex<TaskManager>> = LazyLock::new(|| {
    let task_collector = tokio_metrics_collector::default_task_collector();
    CONVEX_METRICS_REGISTRY
        .register(Box::new(task_collector))
        .unwrap();

    let manager = TaskManager {
        monitors: HashMap::new(),
    };
    Mutex::new(manager)
});

struct TaskManager {
    monitors: HashMap<&'static str, TaskMonitor>,
}

impl TaskManager {
    fn get(&mut self, name: &'static str) -> TaskMonitor {
        if let Some(monitor) = self.monitors.get(name) {
            return monitor.clone();
        }
        let monitor = TaskMonitor::new();
        self.monitors.insert(name, monitor.clone());
        tokio_metrics_collector::default_task_collector()
            .add(name, monitor.clone())
            .expect("Duplicate task label?");
        monitor
    }
}
