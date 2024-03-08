#![feature(const_mut_refs)]
#![feature(const_type_name)]
#![feature(lazy_cell)]
#![feature(async_closure)]
#![feature(try_blocks)]
#![feature(const_trait_impl)]
#![feature(iterator_try_collect)]
#![feature(type_alias_impl_trait)]
#![feature(let_chains)]
#![feature(never_type)]
#![feature(assert_matches)]
#![feature(impl_trait_in_assoc_type)]
#![feature(arc_unwrap_or_clone)]
#![feature(round_char_boundary)]

mod bundled_js;
pub mod client;
mod concurrency_limiter;
pub mod environment;
mod error;
mod execution_scope;
mod helpers;
mod http;
mod http_action;
mod is_instance_of_error;
pub mod isolate;
pub mod metrics;
mod module_map;
mod ops;
mod request_scope;
mod strings;
mod termination;
#[cfg(test)]
mod tests;
mod timeout;
mod user_error;

#[cfg(any(test, feature = "testing"))]
pub mod test_helpers;

pub use self::{
    bundled_js::{
        NODE_EXECUTOR_FILES,
        NODE_EXECUTOR_SHA256,
    },
    client::{
        ActionCallbacks,
        ActionRequest,
        ActionRequestParams,
        BackendIsolateWorker,
        FunctionResult,
        IsolateClient,
        IsolateConfig,
    },
    concurrency_limiter::ConcurrencyLimiter,
    environment::{
        action::outcome::{
            ActionOutcome,
            HttpActionOutcome,
        },
        auth_config::AuthConfig,
        helpers::{
            module_loader::{
                ModuleLoader,
                TransactionModuleLoader,
            },
            validation::{
                validate_schedule_args,
                ValidatedHttpPath,
                ValidatedUdfPathAndArgs,
            },
            FunctionOutcome,
            JsonPackedValue,
            SyscallStats,
            SyscallTrace,
        },
        udf::{
            outcome::UdfOutcome,
            CONVEX_ORIGIN,
            CONVEX_SITE,
        },
    },
    helpers::{
        deserialize_udf_custom_error,
        deserialize_udf_result,
        format_uncaught_error,
        parse_udf_args,
        serialize_udf_args,
        UdfArgsJson,
    },
    http_action::{
        HttpActionRequest,
        HttpActionRequestHead,
        HttpActionResponse,
        HTTP_ACTION_BODY_LIMIT,
    },
    isolate::IsolateHeapStats,
    metrics::{
        log_source_map_missing,
        log_source_map_token_lookup_failed,
    },
    user_error::{
        FunctionNotFoundError,
        ModuleNotFoundError,
    },
};
