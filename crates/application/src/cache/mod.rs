use std::{
    cmp,
    collections::BTreeMap,
    mem,
    sync::{
        Arc,
        LazyLock,
    },
    time::Duration,
};

use async_broadcast::{
    broadcast,
    Receiver,
    Sender,
};
use common::{
    self,
    execution_context::ExecutionContext,
    identity::IdentityCacheKey,
    knobs::{
        DATABASE_UDF_SYSTEM_TIMEOUT,
        DATABASE_UDF_USER_TIMEOUT,
        UDF_CACHE_MAX_SIZE,
    },
    query_journal::QueryJournal,
    runtime::{
        Runtime,
        RuntimeInstant,
    },
    types::{
        AllowedVisibility,
        FunctionCaller,
        TableName,
        TableStats,
        Timestamp,
        UdfType,
    },
    value::ConvexArray,
    RequestId,
};
use database::{
    Database,
    Token,
};
use errors::ErrorMetadataAnyhowExt;
use futures::{
    select_biased,
    FutureExt,
};
use isolate::{
    FunctionOutcome,
    ModuleLoader,
    UdfOutcome,
    ValidatedUdfPathAndArgs,
};
use keybroker::Identity;
use lru::LruCache;
use metrics::{
    get_timer,
    log_cache_size,
    log_drop_cache_result_too_old,
    log_perform_go,
    log_perform_wait_peer_timeout,
    log_perform_wait_self_timeout,
    log_plan_go,
    log_plan_peer_timeout,
    log_plan_ready,
    log_plan_wait,
    log_success,
    log_validate_refresh_failed,
    log_validate_system_time_in_the_future,
    log_validate_system_time_too_old,
    log_validate_ts_too_old,
    succeed_get_timer,
    GoReason,
};
use parking_lot::Mutex;
use sync_types::CanonicalizedUdfPath;
use usage_tracking::FunctionUsageTracker;
use value::heap_size::HeapSize;

use crate::{
    application_function_runner::FunctionRouter,
    function_log::FunctionExecutionLog,
    redaction::{
        RedactedJsError,
        RedactedLogLines,
    },
    QueryReturn,
};

mod metrics;

// Maximum age of results to tolerate if they're time-dependent.
pub const MAX_CACHE_AGE: Duration = Duration::from_secs(5);

static TOTAL_QUERY_TIMEOUT: LazyLock<Duration> =
    LazyLock::new(|| *DATABASE_UDF_USER_TIMEOUT + *DATABASE_UDF_SYSTEM_TIMEOUT);

pub struct CacheManager<RT: Runtime> {
    rt: RT,
    database: Database<RT>,
    function_router: FunctionRouter<RT>,
    udf_execution: FunctionExecutionLog<RT>,
    module_loader: Arc<dyn ModuleLoader<RT>>,

    inner: Arc<Mutex<Inner<RT>>>,
}

impl<RT: Runtime> Clone for CacheManager<RT> {
    fn clone(&self) -> Self {
        Self {
            rt: self.rt.clone(),
            database: self.database.clone(),
            function_router: self.function_router.clone(),
            udf_execution: self.udf_execution.clone(),
            module_loader: self.module_loader.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl<RT: Runtime> HeapSize for CacheManager<RT> {
    fn heap_size(&self) -> usize {
        self.inner.lock().size
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CacheKey {
    udf_path: CanonicalizedUdfPath,
    args: ConvexArray,
    identity: IdentityCacheKey,
    journal: QueryJournal,
    allowed_visibility: AllowedVisibility,
}

impl CacheKey {
    /// Approximate size in-memory of the CacheEntry structure, including stack
    /// and heap allocated memory.
    fn size(&self) -> usize {
        mem::size_of::<Self>()
            + self.udf_path.heap_size()
            + self.args.heap_size()
            + self.identity.heap_size()
            + self.journal.heap_size()
    }
}

enum CacheEntry<RT: Runtime> {
    Ready(CacheResult),
    Waiting {
        id: u64,
        started: RT::Instant,
        receiver: Receiver<CacheResult>,
        // The UDF is being executed at this timestamp.
        ts: Timestamp,
    },
}

impl<RT: Runtime> CacheEntry<RT> {
    /// Approximate size in-memory of the CacheEntry structure, including stack
    /// and heap allocated memory.
    fn size(&self) -> usize {
        mem::size_of::<Self>()
            + match self {
                CacheEntry::Ready(ref result) => result.heap_size(),
                // This is an under count since there might be something in the receiver.
                // However, this is kind of hard to measure, and we expect this to
                // be the exception, not the rule.
                CacheEntry::Waiting { .. } => 0,
            }
    }
}

#[derive(Clone)]
struct CacheResult {
    outcome: UdfOutcome,
    original_ts: Timestamp,
    token: Token,
}

impl HeapSize for CacheResult {
    fn heap_size(&self) -> usize {
        self.outcome.heap_size() + self.original_ts.heap_size() + self.token.heap_size()
    }
}

impl<RT: Runtime> CacheManager<RT> {
    pub fn new(
        rt: RT,
        database: Database<RT>,
        function_router: FunctionRouter<RT>,
        udf_execution: FunctionExecutionLog<RT>,
        module_loader: Arc<dyn ModuleLoader<RT>>,
    ) -> Self {
        let inner = Inner {
            cache: LruCache::unbounded(),
            size: 0,
            next_waiting_id: 0,
        };
        Self {
            rt,
            database,
            function_router,
            udf_execution,
            module_loader,
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Execute a UDF with the given arguments and identity at a particular
    /// timestamp. This function internally handles LRU caching these
    /// function executions and ensuring that served cache values are
    /// consistent as of the given timestamp.
    pub async fn get(
        &self,
        request_id: RequestId,
        name: CanonicalizedUdfPath,
        args: ConvexArray,
        identity: Identity,
        ts: Timestamp,
        journal: Option<QueryJournal>,
        allowed_visibility: AllowedVisibility,
        caller: FunctionCaller,
        block_logging: bool,
        usage_tracker: FunctionUsageTracker,
    ) -> anyhow::Result<QueryReturn> {
        let timer = get_timer();
        let result = self
            ._get(
                request_id,
                name,
                args,
                identity,
                ts,
                journal,
                allowed_visibility,
                caller,
                block_logging,
                usage_tracker,
            )
            .await;
        match &result {
            Ok((_, is_cache_hit)) => {
                succeed_get_timer(timer, *is_cache_hit);
            },
            Err(e) => {
                timer.finish_with(e.metric_status_tag_value());
            },
        }
        Ok(result?.0)
    }

    async fn _get(
        &self,
        request_id: RequestId,
        name: CanonicalizedUdfPath,
        args: ConvexArray,
        identity: Identity,
        ts: Timestamp,
        journal: Option<QueryJournal>,
        allowed_visibility: AllowedVisibility,
        caller: FunctionCaller,
        block_logging: bool,
        usage_tracker: FunctionUsageTracker,
    ) -> anyhow::Result<(QueryReturn, bool)> {
        let start = self.rt.monotonic_now();
        let identity_cache_key = identity.cache_key();
        let key = CacheKey {
            udf_path: name.clone(),
            args: args.clone(),
            identity: identity_cache_key,
            journal: journal.unwrap_or_else(QueryJournal::new),
            allowed_visibility,
        };
        let context = ExecutionContext::new(request_id, &caller);

        let mut num_attempts = 0;
        'top: loop {
            num_attempts += 1;
            let now = self.rt.monotonic_now();

            // Bound the total time of the caching algorithm in cases we wait to
            // long before we start executing. If there is slowdown, we prefer to
            // fast fail instead of adding additional load to the system.
            let elapsed = now.clone() - start.clone();
            anyhow::ensure!(
                elapsed <= *TOTAL_QUERY_TIMEOUT,
                "Query execution time out: {elapsed:?}",
            );

            // Step 1: Decide what we're going to do this iteration: use a cached value,
            // wait on someone else to run a UDF, or run the UDF ourselves.
            let maybe_op = self.plan_cache_op(
                &key,
                start.clone(),
                now.clone(),
                &identity,
                ts,
                context.clone(),
            );
            let op: CacheOp = match maybe_op {
                Some(op) => op,
                None => continue 'top,
            };

            // Step 2: Perform our cache operation, potentially running the UDF.
            let is_cache_hit = match op {
                // Serving from cache.
                CacheOp::Ready { .. } => true,
                // We either use a result from the cache or we continue from 'top
                // after Step 2 without logging.
                CacheOp::Wait { .. } => true,
                // We are executing ourselves.
                CacheOp::Go { .. } => false,
            };
            let waiting_entry_id = match op {
                CacheOp::Go {
                    waiting_entry_id, ..
                } => waiting_entry_id,
                _ => None,
            };
            let (result, table_stats) =
                match self.perform_cache_op(&key, op, usage_tracker.clone()).await {
                    Ok(Some(r)) => r,
                    Ok(None) => continue 'top,
                    Err(err) => {
                        if let Some(waiting_entry_id) = waiting_entry_id {
                            self.inner.lock().remove_waiting(&key, waiting_entry_id);
                        }
                        return Err(err);
                    },
                };

            // Step 3: Validate that the cache result we got is good enough. Is our desired
            // timestamp in its validity interval? If it looked at system time, is it not
            // too old?
            let cache_result = match self.validate_cache_result(&key, ts, result).await {
                Ok(Some(r)) => r,
                Ok(None) => continue 'top,
                Err(err) => {
                    if let Some(waiting_entry_id) = waiting_entry_id {
                        self.inner.lock().remove_waiting(&key, waiting_entry_id);
                    }
                    return Err(err);
                },
            };

            // Step 4: Rewrite the value into the cache. This method will discard the new
            // value if the UDF failed or if a newer (i.e. higher `original_ts`)
            // value is in the cache.
            if cache_result.outcome.result.is_ok() {
                // We do not cache JSErrors
                self.inner
                    .lock()
                    .put_ready(key.clone(), cache_result.clone());
            } else if let Some(waiting_entry_id) = waiting_entry_id {
                self.inner.lock().remove_waiting(&key, waiting_entry_id);
            }

            // Step 5: Log some stuff and return.
            log_success(num_attempts);
            self.udf_execution.log_query(
                cache_result.outcome.clone(),
                table_stats,
                is_cache_hit,
                start.elapsed(),
                caller,
                usage_tracker,
                context.clone(),
            );

            let log_lines = RedactedLogLines::from_log_lines(
                cache_result.outcome.log_lines.clone(),
                block_logging,
            );
            let result = QueryReturn {
                result: match cache_result.outcome.result {
                    Ok(r) => Ok(r.unpack()),
                    Err(e) => Err(RedactedJsError::from_js_error(
                        e,
                        block_logging,
                        context.request_id,
                    )),
                },
                log_lines,
                token: cache_result.token,
                ts,
                journal: cache_result.outcome.journal,
            };
            return Ok((result, is_cache_hit));
        }
    }

    fn plan_cache_op(
        &self,
        key: &CacheKey,
        start: RT::Instant,
        now: RT::Instant,
        identity: &Identity,
        ts: Timestamp,
        context: ExecutionContext,
    ) -> Option<CacheOp> {
        let go = |sender: Option<(Sender<_>, u64)>| {
            let (sender, waiting_entry_id) = match sender {
                Some((sender, waiting_entry_id)) => (sender, Some(waiting_entry_id)),
                None => {
                    // No one should wait for this, so it's okay to drop the
                    // receiver. And the sender ignores errors.
                    let (sender, _) = broadcast(1);
                    (sender, None)
                },
            };
            CacheOp::Go {
                waiting_entry_id,
                sender,
                name: key.udf_path.clone(),
                args: key.args.clone(),
                identity: identity.clone(),
                ts,
                journal: key.journal.clone(),
                allowed_visibility: key.allowed_visibility.clone(),
                context,
            }
        };
        let mut inner = self.inner.lock();
        let op = match inner.cache.get(key) {
            Some(CacheEntry::Ready(r)) => {
                if ts < r.original_ts {
                    // If another request has already executed this UDF at a
                    // newer timestamp, we can't use the cache. Re-execute
                    // in this case.
                    tracing::debug!("Cache value too new for {:?}", key);
                    log_plan_go(GoReason::PeerTimestampTooNew);
                    go(None)
                } else {
                    tracing::debug!("Cache value ready for {:?}", key);
                    log_plan_ready();
                    CacheOp::Ready { result: r.clone() }
                }
            },
            Some(CacheEntry::Waiting {
                id,
                started: peer_started,
                receiver,
                ts: peer_ts,
            }) => {
                let entry_id = *id;
                if *peer_ts > ts {
                    log_plan_go(GoReason::PeerTimestampTooNew);
                    return Some(go(None));
                }
                // We don't serialize sampling `now` under the cache lock, and since it can
                // occur on different threads, we're not guaranteed that
                // `peer_started < now`. So, if the peer started *after* us,
                // consider its `peer_elapsed` time to be zero.
                let peer_started = cmp::min(now.clone(), peer_started.clone());
                let peer_elapsed = now.clone() - peer_started;
                if peer_elapsed >= *TOTAL_QUERY_TIMEOUT {
                    tracing::debug!(
                        "Peer timed out ({:?}), removing cache entry and retrying",
                        peer_elapsed
                    );
                    inner.remove_waiting(key, entry_id);
                    log_plan_peer_timeout();
                    return None;
                }
                let get_elapsed = now - start;
                let remaining = *TOTAL_QUERY_TIMEOUT - cmp::max(peer_elapsed, get_elapsed);
                tracing::debug!("Waiting for peer to compute {:?}", key);
                log_plan_wait();
                CacheOp::Wait {
                    waiting_entry_id: *id,
                    receiver: receiver.clone(),
                    remaining,
                }
            },
            None => {
                tracing::debug!("No cache value for {:?}, executing UDF...", key);
                let (sender, executor_id) = inner.put_waiting(key.clone(), now, ts);
                log_plan_go(GoReason::NoCacheResult);
                go(Some((sender, executor_id)))
            },
        };
        Some(op)
    }

    async fn perform_cache_op(
        &self,
        key: &CacheKey,
        op: CacheOp,
        usage_tracker: FunctionUsageTracker,
    ) -> anyhow::Result<Option<(CacheResult, BTreeMap<TableName, TableStats>)>> {
        let r = match op {
            CacheOp::Ready { result } => {
                if result.outcome.result.is_err() {
                    panic!(
                        "Developer error: Cache contained failed execution for {:?}",
                        key
                    )
                }
                (result, BTreeMap::new())
            },
            CacheOp::Wait {
                waiting_entry_id,
                mut receiver,
                remaining,
            } => {
                let result = select_biased! {
                    maybe_result = receiver.recv().fuse() => match maybe_result {
                        Err(..) => {
                            // The peer working on the cache entry went away (perhaps due to an
                            // error), so remove its entry and retry.
                            self.inner.lock().remove_waiting(key, waiting_entry_id);
                            log_perform_wait_peer_timeout();
                            return Ok(None)
                        },
                        Ok(r) => r,
                    },
                    // We checked in `plan_cache_op` that the peer hadn't timed out with respect to
                    // its start time, so timing out here means that we're on a retry and ran out of
                    // our time ourselves. Therefore, don't remove the cache entry in this
                    // condition.
                    _ = self.rt.wait(remaining).fuse() => {
                        log_perform_wait_self_timeout();
                        return Ok(None)
                    },
                };
                if result.outcome.result.is_err() {
                    panic!(
                        "Developer error: CacheOp::Go sent failed execution for {:?}",
                        key
                    )
                }
                (result, BTreeMap::new())
            },
            CacheOp::Go {
                waiting_entry_id: _,
                sender,
                name,
                args,
                identity,
                ts,
                journal,
                allowed_visibility,
                context,
            } => {
                let mut tx = self
                    .database
                    .begin_with_ts(identity.clone(), ts, usage_tracker)
                    .await?;
                // We are validating UDF visibility here as opposed to earlier so the validation
                // checks are transactional with running the query. This is safe because we will
                // never serve a result based of a stale visibility check since the data read as
                // part of the visibility check is part of the ReadSet for this query.
                let validate_result = ValidatedUdfPathAndArgs::new(
                    allowed_visibility,
                    &mut tx,
                    name.clone(),
                    args.clone(),
                    UdfType::Query,
                    self.module_loader.clone(),
                )
                .await?;
                let (mut tx, outcome) = match validate_result {
                    Err(js_err) => {
                        let query_outcome = UdfOutcome::from_error(
                            js_err,
                            name.clone(),
                            args,
                            identity.into(),
                            self.rt.clone(),
                            None,
                        );
                        (tx, FunctionOutcome::Query(query_outcome))
                    },
                    Ok(path_and_args) => {
                        self.function_router
                            .execute_query_or_mutation(
                                tx,
                                path_and_args.clone(),
                                UdfType::Query,
                                journal,
                                context,
                            )
                            .await?
                    },
                };
                let FunctionOutcome::Query(query_outcome) = outcome else {
                    anyhow::bail!("Received non-query outcome when executing a query")
                };
                let ts = tx.begin_timestamp();
                let table_stats = tx.take_stats();
                let token = tx.into_token()?;
                let result = CacheResult {
                    outcome: query_outcome,
                    original_ts: *ts,
                    token,
                };
                if result.outcome.result.is_ok() {
                    let _: Result<_, _> = sender.try_broadcast(result.clone());
                }
                log_perform_go(result.outcome.result.is_ok());
                (result, table_stats)
            },
        };
        Ok(Some(r))
    }

    async fn validate_cache_result(
        &self,
        key: &CacheKey,
        ts: Timestamp,
        mut result: CacheResult,
    ) -> anyhow::Result<Option<CacheResult>> {
        if ts < result.original_ts {
            // If the cached value is newer than the requested timestamp,
            // we have to re-execute the UDF.
            log_validate_ts_too_old();
            return Ok(None);
        }
        result.token = match self.database.refresh_token(result.token, ts).await? {
            Some(t) => t,
            None => {
                tracing::debug!(
                    "Couldn't refresh cache entry from {} to {}, retrying...",
                    result.original_ts,
                    ts
                );
                self.inner.lock().remove_ready(key, result.original_ts);
                log_validate_refresh_failed();
                return Ok(None);
            },
        };
        if result.outcome.observed_time {
            let sys_now = self.rt.unix_timestamp();
            let cached_time = result.outcome.unix_timestamp;
            match sys_now.checked_sub(cached_time) {
                Some(entry_age) if entry_age > MAX_CACHE_AGE => {
                    tracing::debug!(
                        "Log entry for {:?} used system time and is too old ({:?}), retrying...",
                        key,
                        entry_age
                    );
                    self.inner.lock().remove_ready(key, result.original_ts);
                    log_validate_system_time_too_old();
                    return Ok(None);
                },
                None => {
                    tracing::warn!(
                        "Cached value's timestamp {:?} is in the future (now: {:?})?",
                        cached_time,
                        sys_now,
                    );
                    self.inner.lock().remove_ready(key, result.original_ts);
                    log_validate_system_time_in_the_future();
                    return Ok(None);
                },
                Some(..) => (),
            }
        }
        Ok(Some(result))
    }
}

struct Inner<RT: Runtime> {
    cache: LruCache<CacheKey, CacheEntry<RT>>,
    size: usize,

    next_waiting_id: u64,
}

impl<RT: Runtime> Inner<RT> {
    // Remove only a `CacheEntry::Ready` from the cache, predicated on its
    // `executor_id` matching.
    fn remove_waiting(&mut self, key: &CacheKey, entry_id: u64) {
        match self.cache.get(key) {
            Some(CacheEntry::Waiting { id, .. }) if *id == entry_id => {
                self.size -= key.size() + self.cache.pop(key).unwrap().size();
            },
            _ => (),
        }
        log_cache_size(self.size)
    }

    // Remove only a `CacheEntry::Ready` from the cache, predicated on its
    // `original_ts` matching.
    fn remove_ready(&mut self, key: &CacheKey, original_ts: Timestamp) {
        match self.cache.get(key) {
            Some(CacheEntry::Ready(ref result)) if result.original_ts == original_ts => {
                self.size -= key.size() + self.cache.pop(key).unwrap().size();
            },
            _ => (),
        }
        log_cache_size(self.size);
    }

    fn put_waiting(
        &mut self,
        key: CacheKey,
        now: RT::Instant,
        ts: Timestamp,
    ) -> (Sender<CacheResult>, u64) {
        let id = self.next_waiting_id;
        self.next_waiting_id += 1;

        let (sender, receiver) = broadcast(1);

        let key_size = key.size();
        let new_entry = CacheEntry::Waiting {
            id,
            receiver,
            started: now,
            ts,
        };
        let new_size = key_size + new_entry.size();
        let old_size = self
            .cache
            .put(key, new_entry)
            .map(|value| key_size + value.size())
            .unwrap_or(0);

        self.size -= old_size;
        self.size += new_size;

        self.enforce_size_limit();
        (sender, id)
    }

    // Put a `CacheEntry::Ready` into the cache, potentially dropping it if there's
    // already a value with a higher `original_ts`.
    fn put_ready(&mut self, key: CacheKey, result: CacheResult) {
        match self.cache.get_mut(&key) {
            Some(entry @ CacheEntry::Waiting { .. }) => {
                let new_entry = CacheEntry::Ready(result);
                self.size -= entry.size();
                self.size += new_entry.size();
                *entry = new_entry;
            },
            Some(CacheEntry::Ready(ref mut existing_result)) => {
                if existing_result.original_ts < result.original_ts
                    || (existing_result.original_ts == result.original_ts
                        && existing_result.token.ts() < result.token.ts())
                {
                    self.size -= existing_result.heap_size();
                    self.size += result.heap_size();
                    *existing_result = result;
                } else {
                    tracing::debug!(
                        "dropping cache result for {key:?} because result timestamp {} <= cached \
                         timestamp {}",
                        result.original_ts,
                        existing_result.original_ts
                    );
                    log_drop_cache_result_too_old();
                }
            },
            None => {
                let new_entry = CacheEntry::Ready(result);
                self.size += key.size() + new_entry.size();
                self.cache.put(key, new_entry);
            },
        }
        self.enforce_size_limit();
    }

    /// Pop records until the cache is under the given size.
    fn enforce_size_limit(&mut self) {
        while self.size > *UDF_CACHE_MAX_SIZE {
            let (popped_key, popped_entry) = self
                .cache
                .pop_lru()
                .expect("Cache is too large without any items?");
            self.size -= popped_key.size() + popped_entry.size();
        }
        log_cache_size(self.size)
    }
}

enum CacheOp {
    Ready {
        result: CacheResult,
    },
    Wait {
        waiting_entry_id: u64,
        receiver: Receiver<CacheResult>,
        remaining: Duration,
    },
    Go {
        waiting_entry_id: Option<u64>,
        sender: Sender<CacheResult>,
        name: CanonicalizedUdfPath,
        args: ConvexArray,
        identity: Identity,
        ts: Timestamp,
        journal: QueryJournal,
        allowed_visibility: AllowedVisibility,
        context: ExecutionContext,
    },
}
