use std::time::Duration;

use common::{
    assert_obj,
    errors::JsError,
    runtime::Runtime,
    testing::TestPersistence,
    version::Version,
};
use futures::{
    stream,
    StreamExt,
};
use headers::HeaderMap;
use http::Method;
use keybroker::Identity;
use model::scheduled_jobs::{
    types::ScheduledJobState,
    virtual_table::PublicScheduledJob,
};
use must_let::must_let;
use runtime::{
    prod::ProdRuntime,
    testing::TestRuntime,
};
use serde_json::{
    json,
    Value as JsonValue,
};
use url::Url;
use value::ConvexValue;

use crate::{
    concurrency_limiter::ConcurrencyLimiter,
    http_action::HttpActionRequest,
    test_helpers::{
        UdfTest,
        UdfTestConfig,
    },
    tests::assert_contains,
    HttpActionRequestHead,
    IsolateConfig,
};

pub fn http_request(path: &str) -> HttpActionRequest {
    HttpActionRequest {
        head: HttpActionRequestHead {
            headers: HeaderMap::new(),
            url: Url::parse(&format!("http://127.0.0.1:8001/{}", path)).unwrap(),
            method: Method::GET,
        },
        body: None,
    }
}

pub fn http_post_request(path: &str, body: Vec<u8>) -> HttpActionRequest {
    HttpActionRequest {
        head: HttpActionRequestHead {
            headers: HeaderMap::new(),
            url: Url::parse(&format!("http://127.0.0.1:8001/{}", path)).unwrap(),
            method: Method::POST,
        },
        body: Some(stream::once(async move { Ok(body.into()) }).boxed()),
    }
}

async fn http_action_udf_test_timeout<RT: Runtime>(
    rt: RT,
    timeout: Option<Duration>,
) -> anyhow::Result<UdfTest<RT, TestPersistence>> {
    UdfTest::default_with_config(
        UdfTestConfig {
            isolate_config: IsolateConfig::new_with_max_user_timeout(
                "http_action_test",
                // we need at least 2 threads since HTTP actions will request and block
                // on the execution of other UDFs
                2,
                timeout,
                ConcurrencyLimiter::unlimited(),
            ),
            udf_server_version: Version::parse("1000.0.0")?,
        },
        rt,
    )
    .await
}

pub async fn http_action_udf_test(
    rt: TestRuntime,
) -> anyhow::Result<UdfTest<TestRuntime, TestPersistence>> {
    http_action_udf_test_timeout(rt, None).await
}

#[convex_macro::test_runtime]
async fn test_http_basic(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            http_post_request("basic", "hi".as_bytes().to_vec()),
            Identity::system(),
        )
        .await?;

    must_let!(let Some(value) = outcome.result?.body().clone());
    let expected = json!({
        "requestBody": "hi",
        "countBefore": 0,
        "countAfter": 1,
        "actionResult": 2,
        "isBigInt": true
    });
    let actual: JsonValue = serde_json::from_slice(&value)?;
    assert_eq!(actual, expected);
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_response_stream(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            http_request("stream_response"),
            Identity::system(),
        )
        .await?;

    must_let!(let Some(value) = outcome.result?.body().clone());
    assert_eq!(std::str::from_utf8(&value)?, "<html></html>");
    Ok(())
}

#[convex_macro::prod_rt_test]
async fn test_http_dangling_response_stream(rt: ProdRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test_timeout(rt, Some(Duration::from_secs(1))).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            http_request("stream_dangling_response"),
            Identity::system(),
        )
        .await?;

    let e = outcome.result.unwrap_err();
    assert_contains(&e, "Function execution timed out");
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_slow(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test_timeout(rt, Some(Duration::from_secs(1))).await?;

    let (outcome, log_lines) = t
        .raw_http_action("http_action", http_request("slow"), Identity::system())
        .await?;

    assert!(outcome.result.is_ok());

    let mut log_lines = log_lines;
    let last_line = log_lines.pop().unwrap().to_pretty_string();
    assert_contains(&last_line, "[WARN] Function execution took a long time");
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_echo(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            http_post_request("echo", "hi".as_bytes().to_vec()),
            Identity::system(),
        )
        .await?;

    must_let!(let Some(value) = outcome.result?.body().clone());
    assert_eq!(std::str::from_utf8(&value)?, "hi");
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_scheduler(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action("http_action", http_request("schedule"), Identity::system())
        .await?;

    must_let!(let Some(_) = outcome.result?.body().clone());

    let result = t.query("scheduler:getScheduledJobs", assert_obj!()).await?;
    must_let!(let ConvexValue::Array(scheduled_jobs) = result);
    assert_eq!(scheduled_jobs.len(), 1);
    must_let!(let ConvexValue::Object(job_obj) = scheduled_jobs[0].clone());

    let job = PublicScheduledJob::try_from(job_obj)?;
    assert_eq!(job.state, ScheduledJobState::Pending);

    // End time of the HTTP action + 2 seconds, which should be a little after when
    // the job was scheduled for
    let expected_ts = (outcome.unix_timestamp + Duration::from_secs(2)).as_secs_f64() * 1000.0;
    assert!((job.scheduled_time - expected_ts).abs() < 500.0);
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_error_in_run(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            http_request("errorInRun"),
            Identity::system(),
        )
        .await?;
    must_let!(let JsError { message, .. } = outcome.result.unwrap_err());
    assert!(message.contains("Oh no! Called erroring query"));
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_no_router(rt: TestRuntime) -> anyhow::Result<()> {
    let t = UdfTest::default(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_no_default",
            http_request("no routes here"),
            Identity::system(),
        )
        .await?;

    must_let!(let JsError { message, .. } = outcome.result.unwrap_err());
    assert!(message.contains("Couldn't find default export in"));
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_bad_router(rt: TestRuntime) -> anyhow::Result<()> {
    let t = UdfTest::default(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_object_default",
            http_request("no routes here"),
            Identity::system(),
        )
        .await?;

    must_let!(let JsError { message, .. } = outcome.result.unwrap_err());
    assert!(message.contains("The default export of `convex/http.js` is not a Router"));
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_error_in_run_catch(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action("http", http_request("errorInRunCatch"), Identity::system())
        .await?;

    assert!(outcome.result?.body().is_some());
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_error_in_endpoint(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            http_request("errorInEndpoint"),
            Identity::system(),
        )
        .await?;
    must_let!(let JsError { message, .. } = outcome.result.unwrap_err());
    assert!(message.contains("Oh no!"));
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_env_var(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            http_request("convexCloudSystemVar"),
            Identity::system(),
        )
        .await?;
    must_let!(let Some(value) = outcome.result?.body().clone());
    assert_eq!(String::from_utf8(value)?, "https://carnitas.convex.cloud");

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            http_request("convexSiteSystemVar"),
            Identity::system(),
        )
        .await?;
    must_let!(let Some(value) = outcome.result?.body().clone());
    assert_eq!(String::from_utf8(value)?, "https://carnitas.convex.site");
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_action_response_size_too_large(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, _log_lines) = t
        .raw_http_action(
            "http_action",
            // Ask for 23MiB
            http_post_request("largeResponse", "23".as_bytes().to_vec()),
            Identity::system(),
        )
        .await?;
    let error = outcome.result.unwrap_err();
    assert_contains(
        &error,
        "InternalServerError: HTTP actions support responses up to 20 MiB (returned response was \
         23 MiB bytes)",
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_http_action_response_size_large(rt: TestRuntime) -> anyhow::Result<()> {
    let t = http_action_udf_test(rt).await?;

    let (outcome, log_lines) = t
        .raw_http_action(
            "http_action",
            // Ask for 23MiB
            http_post_request("largeResponse", "19".as_bytes().to_vec()),
            Identity::system(),
        )
        .await?;
    assert!(outcome.result.is_ok());
    let mut log_lines = log_lines;
    let last_line = log_lines.pop().unwrap().to_pretty_string();
    assert_contains(
        &last_line,
        "[WARN] Large response returned from an HTTP action ",
    );
    Ok(())
}
