use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        Weak,
    },
};

use anyhow::Context;
use async_trait::async_trait;
use common::{
    codel_queue::{
        new_codel_queue_async,
        CoDelQueueSender,
    },
    http::fetch::FetchClient,
    knobs::{
        FUNRUN_ISOLATE_ACTIVE_THREADS,
        ISOLATE_QUEUE_SIZE,
    },
    log_lines::LogLine,
    persistence::{
        NoopRetentionValidator,
        PersistenceReader,
        RetentionValidator,
    },
    query_journal::QueryJournal,
    runtime::{
        shutdown_and_join,
        Runtime,
        SpawnHandle,
    },
    types::{
        ConvexOrigin,
        IndexId,
        RepeatableTimestamp,
        UdfType,
    },
};
use database::{
    shutdown_error,
    BootstrapMetadata,
    Database,
    FollowerRetentionManager,
    SearchIndexManagerSnapshot,
    TableCountSnapshot,
    Transaction,
    TransactionSearchSnapshot,
};
use file_storage::TransactionalFileStorage;
use futures::channel::{
    mpsc,
    oneshot,
};
use isolate::{
    client::{
        initialize_v8,
        EnvironmentData,
        IsolateWorkerHandle,
        Request as IsolateRequest,
        RequestType as IsolateRequestType,
        SharedIsolateScheduler,
        UdfRequest,
    },
    metrics::{
        execute_full_error,
        queue_timer,
    },
    ActionCallbacks,
    ActionRequest,
    ActionRequestParams,
    ConcurrencyLimiter,
    FunctionOutcome,
    IsolateConfig,
    ValidatedUdfPathAndArgs,
};
use keybroker::{
    Identity,
    InstanceSecret,
    KeyBroker,
};
use model::environment_variables::types::{
    EnvVarName,
    EnvVarValue,
};
use parking_lot::{
    Mutex,
    RwLock,
};
use request_context::RequestContext;
use storage::Storage;
use sync_types::Timestamp;
use usage_tracking::{
    FunctionUsageStats,
    FunctionUsageTracker,
};

use super::{
    in_memory_indexes::InMemoryIndexCache,
    isolate_worker::FunctionRunnerIsolateWorker,
    FunctionRunner,
};
use crate::{
    module_cache::{
        FunctionRunnerModuleLoader,
        ModuleCache,
    },
    FunctionFinalTransaction,
    FunctionWrites,
};

const MAX_ISOLATE_WORKERS: usize = 128;

#[async_trait]
pub trait StorageForInstance<RT: Runtime>: Clone + Send + Sync + 'static {
    /// Gets a storage impl for a instance. Agnostic to what kind of storage -
    /// local or s3, or how it was loaded (e.g. passed directly within backend,
    /// loaded from a transaction created in Funrun)
    async fn storage_for_instance(
        &self,
        transaction: &mut Transaction<RT>,
    ) -> anyhow::Result<Arc<dyn Storage>>;
}

#[async_trait]
impl<RT: Runtime> StorageForInstance<RT> for Arc<dyn Storage> {
    async fn storage_for_instance(
        &self,
        _transaction: &mut Transaction<RT>,
    ) -> anyhow::Result<Arc<dyn Storage>> {
        Ok(self.clone())
    }
}

pub struct FunctionRunnerCore<RT: Runtime, S: StorageForInstance<RT>> {
    rt: RT,
    sender: CoDelQueueSender<RT, IsolateRequest<RT>>,
    scheduler: Arc<Mutex<Option<RT::Handle>>>,
    handles: Arc<Mutex<Vec<IsolateWorkerHandle<RT>>>>,
    storage: S,
    index_cache: InMemoryIndexCache<RT>,
    module_cache: ModuleCache<RT>,
}

impl<RT: Runtime, S: StorageForInstance<RT>> Clone for FunctionRunnerCore<RT, S> {
    fn clone(&self) -> Self {
        Self {
            rt: self.rt.clone(),
            sender: self.sender.clone(),
            scheduler: self.scheduler.clone(),
            handles: self.handles.clone(),
            storage: self.storage.clone(),
            index_cache: self.index_cache.clone(),
            module_cache: self.module_cache.clone(),
        }
    }
}

pub async fn validate_run_function_result(
    udf_type: UdfType,
    ts: Timestamp,
    retention_validator: Arc<dyn RetentionValidator>,
) -> anyhow::Result<()> {
    match udf_type {
        // Since queries and mutations have no side effects, we perform the
        // retention check here, when validating the result.
        UdfType::Query | UdfType::Mutation => retention_validator
            .validate_snapshot(ts)
            .await
            .context("Function runner retention check changed"),
        // Since Actions can have side effects, we have to validate their
        // retention while we run them. We can't perform an additional check
        // here since actions can run longer than the retention.
        UdfType::Action | UdfType::HttpAction => Ok(()),
    }
}

impl<RT: Runtime, S: StorageForInstance<RT>> FunctionRunnerCore<RT, S> {
    pub async fn new(rt: RT, storage: S, max_percent_per_client: usize) -> anyhow::Result<Self> {
        Self::_new(rt, storage, max_percent_per_client, MAX_ISOLATE_WORKERS).await
    }

    async fn _new(
        rt: RT,
        storage: S,
        max_percent_per_client: usize,
        max_isolate_workers: usize,
    ) -> anyhow::Result<Self> {
        let concurrency_limit = if *FUNRUN_ISOLATE_ACTIVE_THREADS > 0 {
            ConcurrencyLimiter::new(*FUNRUN_ISOLATE_ACTIVE_THREADS)
        } else {
            ConcurrencyLimiter::unlimited()
        };
        let isolate_config = IsolateConfig::new("funrun", concurrency_limit);

        initialize_v8();
        // TODO: do we need to change the below?
        // NB: We don't call V8::Dispose or V8::ShutdownPlatform since we just assume a
        // single V8 instance per process and don't need to clean up its
        // resources.
        let (sender, receiver) =
            new_codel_queue_async::<_, IsolateRequest<_>>(rt.clone(), *ISOLATE_QUEUE_SIZE);
        let handles = Arc::new(Mutex::new(Vec::new()));
        let handles_clone = handles.clone();
        let rt_clone = rt.clone();
        let scheduler = rt.spawn("shared_isolate_scheduler", async move {
            // The scheduler thread pops a worker from available_workers and
            // pops a request from the CoDelQueueReceiver. Then it sends the request
            // to the worker.
            let isolate_worker = FunctionRunnerIsolateWorker::new(rt_clone.clone(), isolate_config);
            let scheduler = SharedIsolateScheduler::new(
                rt_clone,
                isolate_worker,
                max_isolate_workers,
                handles_clone,
                max_percent_per_client,
            );
            scheduler.run(receiver).await
        });
        let index_cache = InMemoryIndexCache::new(rt.clone());
        let module_cache = ModuleCache::new(rt.clone());
        Ok(Self {
            rt,
            sender,
            scheduler: Arc::new(Mutex::new(Some(scheduler))),
            handles,
            storage,
            index_cache,
            module_cache,
        })
    }

    fn send_request(&self, request: IsolateRequest<RT>) -> anyhow::Result<()> {
        self.sender
            .try_send(request)
            .map_err(|_| execute_full_error())?;
        Ok(())
    }

    async fn receive_response<T>(rx: oneshot::Receiver<T>) -> anyhow::Result<T> {
        // The only reason a oneshot response channel wil be dropped prematurely if the
        // isolate worker is shutting down.
        rx.await.map_err(|_| shutdown_error())
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        {
            let handles: Vec<_> = {
                let mut handles = self.handles.lock();
                for handle in &mut *handles {
                    handle.handle.shutdown();
                }
                handles.drain(..).collect()
            };
            for handle in handles.into_iter() {
                shutdown_and_join(handle.handle).await?;
            }
        }
        if let Some(mut scheduler) = self.scheduler.lock().take() {
            scheduler.shutdown();
        }
        Ok(())
    }

    pub async fn begin_tx(
        &self,
        identity: Identity,
        ts: RepeatableTimestamp,
        existing_writes: FunctionWrites,
        reader: Box<dyn PersistenceReader>,
        instance_name: String,
        in_memory_index_versions: BTreeMap<IndexId, Timestamp>,
        bootstrap_metadata: BootstrapMetadata,
        table_count_snapshot: Arc<dyn TableCountSnapshot>,
        search_index_snapshot: Arc<dyn TransactionSearchSnapshot>,
        retention_validator: Arc<dyn RetentionValidator>,
    ) -> anyhow::Result<Transaction<RT>> {
        let usage_tracker = FunctionUsageTracker::new();
        let transaction_ingredients = self
            .index_cache
            .begin_tx(
                identity.clone(),
                ts,
                existing_writes,
                reader,
                instance_name.clone(),
                in_memory_index_versions,
                bootstrap_metadata,
                table_count_snapshot,
                search_index_snapshot,
                usage_tracker.clone(),
                retention_validator,
            )
            .await?;
        let transaction = transaction_ingredients.clone().try_into()?;
        Ok(transaction)
    }

    // Runs a function given the information for the backend as well as arguments
    // to the function itself.
    // NOTE: The caller of this is responsible of checking retention by calling
    // `validate_function_runner_result`. If the retention check fails, we should
    // ignore any results or errors returned by this method.
    pub async fn run_function_no_retention_check(
        &self,
        instance_name: String,
        instance_secret: InstanceSecret,
        reader: Box<dyn PersistenceReader>,
        convex_origin: ConvexOrigin,
        bootstrap_metadata: BootstrapMetadata,
        table_count_snapshot: Arc<dyn TableCountSnapshot>,
        search_index_snapshot: Arc<dyn TransactionSearchSnapshot>,
        action_callbacks: Arc<dyn ActionCallbacks>,
        fetch_client: Arc<dyn FetchClient>,
        log_line_sender: Option<mpsc::UnboundedSender<LogLine>>,
        path_and_args: ValidatedUdfPathAndArgs,
        udf_type: UdfType,
        identity: Identity,
        ts: RepeatableTimestamp,
        existing_writes: FunctionWrites,
        journal: QueryJournal,
        system_env_vars: BTreeMap<EnvVarName, EnvVarValue>,
        in_memory_index_last_modified: BTreeMap<IndexId, Timestamp>,
        context: RequestContext,
    ) -> anyhow::Result<(
        Option<FunctionFinalTransaction>,
        FunctionOutcome,
        FunctionUsageStats,
    )> {
        let usage_tracker = FunctionUsageTracker::new();
        let retention_validator: Arc<dyn RetentionValidator> = match udf_type {
            // Since queries and mutations are ready only, we can check the retention
            // in at end in `validate_function_runner_result`.
            UdfType::Query | UdfType::Mutation => Arc::new(NoopRetentionValidator {}),
            // For actions, we have to do it inline since they have side effects.
            UdfType::Action | UdfType::HttpAction => {
                Arc::new(FollowerRetentionManager::new(self.rt.clone(), reader.clone()).await?)
            },
        };
        let transaction_ingredients = self
            .index_cache
            .begin_tx(
                identity.clone(),
                ts,
                existing_writes,
                reader,
                instance_name.clone(),
                in_memory_index_last_modified,
                bootstrap_metadata,
                table_count_snapshot,
                search_index_snapshot,
                usage_tracker.clone(),
                retention_validator,
            )
            .await?;
        let mut transaction = transaction_ingredients.clone().try_into()?;
        let storage = self.storage.storage_for_instance(&mut transaction).await?;
        let file_storage = TransactionalFileStorage::new(self.rt.clone(), storage, convex_origin);

        let key_broker = KeyBroker::new(&instance_name, instance_secret)?;
        let environment_data = EnvironmentData {
            key_broker,
            system_env_vars,
            file_storage,
            module_loader: Arc::new(FunctionRunnerModuleLoader {
                instance_name: instance_name.clone(),
                cache: self.module_cache.clone(),
                transaction_ingredients,
            }),
        };

        match udf_type {
            UdfType::Query | UdfType::Mutation => {
                let (tx, rx) = oneshot::channel();
                let request = IsolateRequest::new(
                    instance_name,
                    IsolateRequestType::Udf {
                        request: UdfRequest {
                            path_and_args,
                            udf_type,
                            identity: identity.into(),
                            transaction,
                            journal,
                            context,
                        },
                        environment_data,
                        response: tx,
                        queue_timer: queue_timer(),
                    },
                );
                self.send_request(request)?;
                let (tx, outcome) = Self::receive_response(rx).await??;
                Ok((Some(tx.into()), outcome, usage_tracker.gather_user_stats()))
            },
            UdfType::Action => {
                let (tx, rx) = oneshot::channel();
                let log_line_sender =
                    log_line_sender.context("Missing log line sender for action")?;
                let request = IsolateRequest::new(
                    instance_name,
                    IsolateRequestType::Action {
                        request: ActionRequest {
                            params: ActionRequestParams { path_and_args },
                            transaction,
                            identity,
                            context,
                        },
                        environment_data,
                        response: tx,
                        queue_timer: queue_timer(),
                        action_callbacks,
                        fetch_client,
                        log_line_sender,
                    },
                );
                self.send_request(request)?;
                let outcome = Self::receive_response(rx).await??;
                Ok((
                    None,
                    FunctionOutcome::Action(outcome),
                    usage_tracker.gather_user_stats(),
                ))
            },
            UdfType::HttpAction => {
                anyhow::bail!("Funrun does not support http actions yet")
            },
        }
    }
}

#[derive(Clone)]
pub struct InProcessFunctionRunner<RT: Runtime> {
    server: FunctionRunnerCore<RT, Arc<dyn Storage>>,
    persistence_reader: Box<dyn PersistenceReader>,

    // Static information about the backend.
    instance_name: String,
    instance_secret: InstanceSecret,
    convex_origin: ConvexOrigin,
    database: Database<RT>,
    // Use Weak reference to avoid reference cycle between InProcessFunctionRunner
    // and ApplicationFunctionRunner.
    action_callbacks: Arc<RwLock<Option<Weak<dyn ActionCallbacks>>>>,
    fetch_client: Arc<dyn FetchClient>,
}

impl<RT: Runtime> InProcessFunctionRunner<RT> {
    pub async fn new(
        instance_name: String,
        instance_secret: InstanceSecret,
        convex_origin: ConvexOrigin,
        rt: RT,
        persistence_reader: Box<dyn PersistenceReader>,
        storage: Arc<dyn Storage>,
        database: Database<RT>,
        fetch_client: Arc<dyn FetchClient>,
    ) -> anyhow::Result<Self> {
        // InProcessFunrun is single tenant and thus can use the full capacity.
        let max_percent_per_client = 100;
        let server = FunctionRunnerCore::new(rt, storage, max_percent_per_client).await?;
        Ok(Self {
            server,
            persistence_reader,
            instance_name,
            instance_secret,
            convex_origin,
            database,
            action_callbacks: Arc::new(RwLock::new(None)),
            fetch_client,
        })
    }
}

#[async_trait]
impl<RT: Runtime> FunctionRunner<RT> for InProcessFunctionRunner<RT> {
    async fn run_function(
        &self,
        path_and_args: ValidatedUdfPathAndArgs,
        udf_type: UdfType,
        identity: Identity,
        ts: RepeatableTimestamp,
        existing_writes: FunctionWrites,
        journal: QueryJournal,
        log_line_sender: Option<mpsc::UnboundedSender<LogLine>>,
        system_env_vars: BTreeMap<EnvVarName, EnvVarValue>,
        in_memory_index_last_modified: BTreeMap<IndexId, Timestamp>,
        context: RequestContext,
    ) -> anyhow::Result<(
        Option<FunctionFinalTransaction>,
        FunctionOutcome,
        FunctionUsageStats,
    )> {
        let snapshot = self.database.snapshot(ts)?;
        let table_count_snapshot = Arc::new(snapshot.table_summaries);
        let search_index_snapshot = Arc::new(SearchIndexManagerSnapshot::new(
            snapshot.index_registry,
            snapshot.search_indexes,
            self.database.searcher.clone(),
            self.database.search_storage.clone(),
        ));
        let action_callbacks = self
            .action_callbacks
            .read()
            .clone()
            .context("Action callbacks not set")?
            .upgrade()
            .context(shutdown_error())?;

        // NOTE: We run the function without checking retention until after the
        // function execution. It is important that we do not surface any errors
        // or results until after we call `validate_run_function_result` below.
        let result = self
            .server
            .run_function_no_retention_check(
                self.instance_name.clone(),
                self.instance_secret,
                self.persistence_reader.box_clone(),
                self.convex_origin.clone(),
                self.database.bootstrap_metadata.clone(),
                table_count_snapshot,
                search_index_snapshot,
                action_callbacks,
                self.fetch_client.clone(),
                log_line_sender,
                path_and_args,
                udf_type,
                identity,
                ts,
                existing_writes,
                journal,
                system_env_vars,
                in_memory_index_last_modified,
                context,
            )
            .await;
        validate_run_function_result(udf_type, *ts, self.database.retention_validator()).await?;
        result
    }

    /// This fn should be called on startup. All `run_function` calls will fail
    /// if actions callbacks are not set.
    fn set_action_callbacks(&self, action_callbacks: Arc<dyn ActionCallbacks>) {
        *self.action_callbacks.write() = Some(Arc::downgrade(&action_callbacks));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use common::pause::PauseController;
    use database::test_helpers::DbFixtures;
    use errors::ErrorMetadataAnyhowExt;
    use futures::channel::oneshot;
    use isolate::{
        client::{
            initialize_v8,
            NO_AVAILABLE_WORKERS,
            PAUSE_REQUEST,
        },
        test_helpers::bogus_udf_request,
    };
    use model::initialize_application_system_tables;
    use runtime::testing::TestRuntime;
    use storage::{
        LocalDirStorage,
        Storage,
    };

    use crate::server::FunctionRunnerCore;
    #[convex_macro::test_runtime]
    async fn test_scheduler_workers_limit_requests(rt: TestRuntime) -> anyhow::Result<()> {
        initialize_v8();
        let storage = Arc::new(LocalDirStorage::new(rt.clone())?) as Arc<dyn Storage>;
        let function_runner_core = FunctionRunnerCore::_new(rt.clone(), storage, 100, 1).await?;
        let (mut pause1, pause_client1) = PauseController::new([PAUSE_REQUEST]);
        let DbFixtures { db, .. } = DbFixtures::new(&rt).await?;
        let client1 = "client1";
        let (sender, _rx1) = oneshot::channel();
        let request = bogus_udf_request(&db, client1, Some(pause_client1), sender).await?;
        function_runner_core.send_request(request)?;
        // Pausing a request while being executed should make the next request be
        // rejected because there are no available workers.
        pause1.wait_for_blocked(PAUSE_REQUEST).await;
        let (sender, rx2) = oneshot::channel();
        let request2 = bogus_udf_request(&db, client1, None, sender).await?;
        function_runner_core.send_request(request2)?;
        let response =
            FunctionRunnerCore::<TestRuntime, Arc<dyn Storage>>::receive_response(rx2).await?;
        let err = response.unwrap_err();
        assert!(err.is_overloaded());
        assert!(err.to_string().contains(NO_AVAILABLE_WORKERS));
        Ok(())
    }

    #[convex_macro::test_runtime]
    async fn test_scheduler_does_not_throttle_different_clients(
        rt: TestRuntime,
    ) -> anyhow::Result<()> {
        initialize_v8();
        let storage = Arc::new(LocalDirStorage::new(rt.clone())?) as Arc<dyn Storage>;
        let function_runner_core = FunctionRunnerCore::_new(rt.clone(), storage, 50, 2).await?;
        let (mut pause1, pause_client1) = PauseController::new([PAUSE_REQUEST]);
        let DbFixtures { db, .. } = DbFixtures::new(&rt).await?;
        initialize_application_system_tables(&db).await?;
        let client1 = "client1";
        let (sender, _rx1) = oneshot::channel();
        let request = bogus_udf_request(&db, client1, Some(pause_client1), sender).await?;
        function_runner_core.send_request(request)?;
        // Pausing a request should not affect the next one because we have 2 workers
        // and 2 requests from different clients.
        pause1.wait_for_blocked(PAUSE_REQUEST).await;
        let (sender, rx2) = oneshot::channel();
        let client2 = "client2";
        let request2 = bogus_udf_request(&db, client2, None, sender).await?;
        function_runner_core.send_request(request2)?;
        FunctionRunnerCore::<TestRuntime, Arc<dyn Storage>>::receive_response(rx2).await??;
        Ok(())
    }

    #[convex_macro::test_runtime]
    async fn test_scheduler_throttles_same_client(rt: TestRuntime) -> anyhow::Result<()> {
        initialize_v8();
        let storage = Arc::new(LocalDirStorage::new(rt.clone())?) as Arc<dyn Storage>;
        let function_runner_core = FunctionRunnerCore::_new(rt.clone(), storage, 50, 2).await?;
        let (mut pause1, pause_client1) = PauseController::new([PAUSE_REQUEST]);
        let DbFixtures { db, .. } = DbFixtures::new(&rt).await?;
        initialize_application_system_tables(&db).await?;
        let client = "client";
        let (sender, _rx1) = oneshot::channel();
        let request = bogus_udf_request(&db, client, Some(pause_client1), sender).await?;
        function_runner_core.send_request(request)?;
        // Pausing the first request and sending a second should make the second fail
        // because there's only one worker left and it is reserved for other clients.
        pause1.wait_for_blocked(PAUSE_REQUEST).await;
        let (sender, rx2) = oneshot::channel();
        let request2 = bogus_udf_request(&db, client, None, sender).await?;
        function_runner_core.send_request(request2)?;
        let response =
            FunctionRunnerCore::<TestRuntime, Arc<dyn Storage>>::receive_response(rx2).await?;
        let err = response.unwrap_err();
        assert!(err.is_overloaded());
        assert!(err.to_string().contains(NO_AVAILABLE_WORKERS));
        Ok(())
    }
}
