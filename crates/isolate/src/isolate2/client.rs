use std::{
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use common::{
    runtime::Runtime,
    types::UdfType,
};
use deno_core::ModuleSpecifier;
use futures::{
    self,
    channel::{
        mpsc,
        oneshot,
    },
};
use serde_json::Value as JsonValue;
use tokio::sync::Semaphore;
use value::{
    ConvexObject,
    ConvexValue,
};

use super::{
    environment::EnvironmentOutcome,
    FunctionId,
    PromiseId,
};

pub enum IsolateThreadRequest {
    RegisterModule {
        name: ModuleSpecifier,
        source: String,
        source_map: Option<String>,
        response: oneshot::Sender<anyhow::Result<Vec<ModuleSpecifier>>>,
    },
    EvaluateModule {
        name: ModuleSpecifier,
        response: oneshot::Sender<anyhow::Result<()>>,
    },
    StartFunction {
        udf_type: UdfType,
        module: ModuleSpecifier,
        name: String,
        args: ConvexObject,
        response: oneshot::Sender<anyhow::Result<(FunctionId, EvaluateResult)>>,
    },
    PollFunction {
        function_id: FunctionId,
        completions: Vec<AsyncSyscallCompletion>,
        response: oneshot::Sender<anyhow::Result<EvaluateResult>>,
    },
}

#[derive(Debug)]
pub enum EvaluateResult {
    Ready(ReadyEvaluateResult),
    Pending {
        async_syscalls: Vec<PendingAsyncSyscall>,
    },
}

#[derive(Debug)]
pub struct ReadyEvaluateResult {
    pub result: ConvexValue,
    pub outcome: EnvironmentOutcome,
}

#[derive(Debug)]
pub struct PendingAsyncSyscall {
    pub promise_id: PromiseId,
    pub name: String,
    pub args: JsonValue,
}

pub struct AsyncSyscallCompletion {
    pub promise_id: PromiseId,
    pub result: anyhow::Result<String>,
}

pub struct IsolateThreadClient<RT: Runtime> {
    rt: RT,
    sender: mpsc::Sender<IsolateThreadRequest>,
    user_time_remaining: Duration,
    semaphore: Arc<Semaphore>,
}

impl<RT: Runtime> IsolateThreadClient<RT> {
    pub fn new(
        rt: RT,
        sender: mpsc::Sender<IsolateThreadRequest>,
        user_timeout: Duration,
        semaphore: Arc<Semaphore>,
    ) -> Self {
        Self {
            rt,
            sender,
            user_time_remaining: user_timeout,
            semaphore,
        }
    }

    pub async fn send<T>(
        &mut self,
        request: IsolateThreadRequest,
        mut rx: oneshot::Receiver<anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        if self.user_time_remaining.is_zero() {
            anyhow::bail!("User time exhausted");
        }

        // Use the semaphore to ensure that a bounded number of isolate
        // threads are executing at any point in time.
        let permit = self.semaphore.clone().acquire_owned().await?;

        // Start the user timer after we acquire the permit.
        let user_start = Instant::now();
        let mut user_timeout = self.rt.wait(self.user_time_remaining);
        self.sender.try_send(request)?;
        let result = futures::select_biased! {
            _ = user_timeout => {
                // XXX: We need to terminate the isolate handle here in
                // case user code is in an infinite loop.
                anyhow::bail!("User time exhausted");
            },
            result = rx => result,
        };

        // Deduct the time spent in the isolate thread from our remaining user time.
        let user_elapsed = user_start.elapsed();
        self.user_time_remaining = self.user_time_remaining.saturating_sub(user_elapsed);

        // Drop the permit once we've received the response, allowing another
        // Tokio thread to talk to its V8 thread.
        drop(permit);

        result?
    }

    pub async fn register_module(
        &mut self,
        name: ModuleSpecifier,
        source: String,
        source_map: Option<String>,
    ) -> anyhow::Result<Vec<ModuleSpecifier>> {
        let (tx, rx) = oneshot::channel();
        self.send(
            IsolateThreadRequest::RegisterModule {
                name,
                source,
                source_map,
                response: tx,
            },
            rx,
        )
        .await
    }

    pub async fn evaluate_module(&mut self, name: ModuleSpecifier) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(
            IsolateThreadRequest::EvaluateModule { name, response: tx },
            rx,
        )
        .await
    }

    pub async fn start_function(
        &mut self,
        udf_type: UdfType,
        module: ModuleSpecifier,
        name: String,
        args: ConvexObject,
    ) -> anyhow::Result<(FunctionId, EvaluateResult)> {
        let (tx, rx) = oneshot::channel();
        self.send(
            IsolateThreadRequest::StartFunction {
                udf_type,
                module,
                name,
                args,
                response: tx,
            },
            rx,
        )
        .await
    }

    pub async fn poll_function(
        &mut self,
        function_id: FunctionId,
        completions: Vec<AsyncSyscallCompletion>,
    ) -> anyhow::Result<EvaluateResult> {
        let (tx, rx) = oneshot::channel();
        self.send(
            IsolateThreadRequest::PollFunction {
                function_id,
                completions,
                response: tx,
            },
            rx,
        )
        .await
    }
}
