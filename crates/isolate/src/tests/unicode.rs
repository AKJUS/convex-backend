use errors::ErrorMetadataAnyhowExt;
use itertools::Itertools;
use runtime::testing::TestRuntime;
use value::assert_obj;

use crate::test_helpers::UdfTest;

#[convex_macro::test_runtime]
async fn test_partial_escape_sequence_return(rt: TestRuntime) -> anyhow::Result<()> {
    let t = UdfTest::default(rt).await?;
    let err = t
        .query("unicode:partialEscapeSequenceReturn", assert_obj!())
        .await
        .unwrap_err();
    assert_eq!(err.short_msg(), "FunctionReturnInvalidJson");
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_partial_escape_sequence_on_insert(rt: TestRuntime) -> anyhow::Result<()> {
    let t = UdfTest::default(rt).await?;
    let err = t
        .mutation_js_error("unicode:partialEscapeSequenceDbInsert", assert_obj!())
        .await?;
    assert!(err.message.contains("Received invalid json"));
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_partial_escape_sequence_console_log(rt: TestRuntime) -> anyhow::Result<()> {
    let t = UdfTest::default(rt).await?;
    let log_lines = t
        .query_log_lines("unicode:partialEscapeSequenceConsoleLog", assert_obj!())
        .await?;
    // ::deno_core::serde_v8::from_v8 does a replacement character for invalid utf8
    assert_eq!(
        log_lines
            .into_iter()
            .map(|l| l.to_pretty_string())
            .collect_vec(),
        vec!["[LOG] '�...'".to_string()]
    );
    Ok(())
}
