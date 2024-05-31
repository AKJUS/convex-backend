use async_trait::async_trait;
use common::{
    components::{
        ComponentFunctionPath,
        ComponentPath,
    },
    pause::PauseClient,
    runtime::Runtime,
    types::{
        AllowedVisibility,
        FunctionCaller,
        RepeatableTimestamp,
    },
    RequestId,
};
use database::{
    LogReader,
    ReadSet,
    Subscription,
    Token,
};
use futures::{
    future::BoxFuture,
    FutureExt,
};
use keybroker::Identity;
use model::session_requests::types::SessionRequestIdentifier;
use serde_json::Value as JsonValue;
use sync_types::{
    AuthenticationToken,
    SerializedQueryJournal,
    Timestamp,
    UdfPath,
};

use crate::{
    Application,
    RedactedActionError,
    RedactedActionReturn,
    RedactedMutationError,
    RedactedMutationReturn,
    RedactedQueryReturn,
};

#[cfg_attr(
    any(test, feature = "testing"),
    derive(proptest_derive::Arbitrary, Debug, Clone, PartialEq)
)]
pub enum ExecuteQueryTimestamp {
    // Execute the query at the latest timestamp.
    Latest,
    // Execute the query at a given timestamp.
    At(Timestamp),
}

// A trait that abstracts the backend API. It all state and validation logic
// so http routes can be kept thin and stateless. The implementor is also
// responsible for routing the request to the appropriate backend in the hosted
// version of Convex.
#[async_trait]
pub trait ApplicationApi: Send + Sync {
    async fn authenticate(
        &self,
        host: &str,
        request_id: RequestId,
        auth_token: AuthenticationToken,
    ) -> anyhow::Result<Identity>;

    async fn execute_public_query(
        &self,
        host: &str,
        request_id: RequestId,
        identity: Identity,
        path: UdfPath,
        args: Vec<JsonValue>,
        caller: FunctionCaller,
        ts: ExecuteQueryTimestamp,
        journal: Option<SerializedQueryJournal>,
    ) -> anyhow::Result<RedactedQueryReturn>;

    async fn execute_public_mutation(
        &self,
        host: &str,
        request_id: RequestId,
        identity: Identity,
        path: UdfPath,
        args: Vec<JsonValue>,
        caller: FunctionCaller,
        // Identifier used to make this mutation idempotent.
        mutation_identifier: Option<SessionRequestIdentifier>,
    ) -> anyhow::Result<Result<RedactedMutationReturn, RedactedMutationError>>;

    async fn execute_public_action(
        &self,
        host: &str,
        request_id: RequestId,
        identity: Identity,
        path: UdfPath,
        args: Vec<JsonValue>,
        caller: FunctionCaller,
    ) -> anyhow::Result<Result<RedactedActionReturn, RedactedActionError>>;

    async fn latest_timestamp(
        &self,
        host: &str,
        request_id: RequestId,
    ) -> anyhow::Result<RepeatableTimestamp>;

    async fn subscribe(&self, token: Token) -> anyhow::Result<Box<dyn SubscriptionTrait>>;
}

// Implements ApplicationApi via Application.
#[async_trait]
impl<RT: Runtime> ApplicationApi for Application<RT> {
    async fn authenticate(
        &self,
        _host: &str,
        _request_id: RequestId,
        auth_token: AuthenticationToken,
    ) -> anyhow::Result<Identity> {
        let validate_time = self.runtime().system_time();
        self.authenticate(auth_token, validate_time).await
    }

    async fn execute_public_query(
        &self,
        _host: &str,
        request_id: RequestId,
        identity: Identity,
        udf_path: UdfPath,
        args: Vec<JsonValue>,
        caller: FunctionCaller,
        ts: ExecuteQueryTimestamp,
        journal: Option<SerializedQueryJournal>,
    ) -> anyhow::Result<RedactedQueryReturn> {
        anyhow::ensure!(
            caller.allowed_visibility() == AllowedVisibility::PublicOnly,
            "This method should not be used by internal callers."
        );

        let ts = match ts {
            ExecuteQueryTimestamp::Latest => *self.now_ts_for_reads(),
            ExecuteQueryTimestamp::At(ts) => ts,
        };
        let path = ComponentFunctionPath {
            component: ComponentPath::root(),
            udf_path,
        };
        self.read_only_udf_at_ts(request_id, path, args, identity, ts, journal, caller)
            .await
    }

    async fn execute_public_mutation(
        &self,
        _host: &str,
        request_id: RequestId,
        identity: Identity,
        udf_path: UdfPath,
        args: Vec<JsonValue>,
        caller: FunctionCaller,
        // Identifier used to make this mutation idempotent.
        mutation_identifier: Option<SessionRequestIdentifier>,
    ) -> anyhow::Result<Result<RedactedMutationReturn, RedactedMutationError>> {
        anyhow::ensure!(
            caller.allowed_visibility() == AllowedVisibility::PublicOnly,
            "This method should not be used by internal callers."
        );

        let path = ComponentFunctionPath {
            component: ComponentPath::root(),
            udf_path,
        };
        self.mutation_udf(
            request_id,
            path,
            args,
            identity,
            mutation_identifier,
            caller,
            PauseClient::new(),
        )
        .await
    }

    async fn execute_public_action(
        &self,
        _host: &str,
        request_id: RequestId,
        identity: Identity,
        udf_path: UdfPath,
        args: Vec<JsonValue>,
        caller: FunctionCaller,
    ) -> anyhow::Result<Result<RedactedActionReturn, RedactedActionError>> {
        anyhow::ensure!(
            caller.allowed_visibility() == AllowedVisibility::PublicOnly,
            "This method should not be used by internal callers."
        );

        let path = ComponentFunctionPath {
            component: ComponentPath::root(),
            udf_path,
        };
        self.action_udf(request_id, path, args, identity, caller)
            .await
    }

    async fn latest_timestamp(
        &self,
        _host: &str,
        _request_id: RequestId,
    ) -> anyhow::Result<RepeatableTimestamp> {
        Ok(self.now_ts_for_reads())
    }

    async fn subscribe(&self, token: Token) -> anyhow::Result<Box<dyn SubscriptionTrait>> {
        let inner = self.subscribe(token.clone()).await?;
        Ok(Box::new(ApplicationSubscription {
            initial_ts: token.ts(),
            end_ts: token.ts(),
            reads: token.into_reads(),
            inner,
            log: self.database.log().clone(),
        }))
    }
}

#[async_trait]
pub trait SubscriptionTrait: Send + Sync {
    fn wait_for_invalidation(&self) -> BoxFuture<'static, anyhow::Result<()>>;

    // Returns true if the subscription validity can be extended to new_ts. Note
    // that extend_validity might return false even if the subscription can be
    // extended, but will never return true if it can't.
    async fn extend_validity(&mut self, new_ts: Timestamp) -> anyhow::Result<bool>;
}

struct ApplicationSubscription {
    inner: Subscription,
    log: LogReader,

    reads: ReadSet,
    // The initial timestamp the subscription was created at. This is known
    // to be valid.
    initial_ts: Timestamp,
    // The last timestamp the subscription is known to be valid for.
    // NOTE that the inner subscription might be valid to a higher timestamp,
    // but end_ts is not automatically updated.
    end_ts: Timestamp,
}

#[async_trait]
impl SubscriptionTrait for ApplicationSubscription {
    fn wait_for_invalidation(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        self.inner.wait_for_invalidation().map(Ok).boxed()
    }

    async fn extend_validity(&mut self, new_ts: Timestamp) -> anyhow::Result<bool> {
        if new_ts < self.initial_ts {
            // new_ts is before the initial subscription timestamp.
            return Ok(false);
        }

        if new_ts <= self.end_ts {
            // We have already validated the subscription past new_ts.
            return Ok(true);
        }

        // The inner subscription is periodically updated by the subscription
        // worker.
        let Some(current_ts) = self.inner.current_ts() else {
            // Subscription is no longer valid. We could check validity from end_ts
            // to new_ts, but this is likely to fail and is potentially unbounded amount of
            // work, so we return false here. This is valid per the function contract.
            return Ok(false);
        };
        self.end_ts = self.end_ts.max(current_ts);

        let current_token = Token::new(self.reads.clone(), self.end_ts);
        let Some(_new_token) = self.log.refresh_token(current_token, new_ts)? else {
            // Subscription validity can't be extended. Note that returning false
            // here also doesn't mean there is a conflict.
            return Ok(false);
        };
        self.end_ts = self.end_ts.max(new_ts);

        return Ok(true);
    }
}
