use serde_json::Value as JsonValue;
use value::heap_size::HeapSize;

use crate::{
    errors::JsError,
    execution_context::ExecutionContext,
    identity::InertIdentity,
    runtime::{
        Runtime,
        UnixTimestamp,
    },
    types::{
        ModuleEnvironment,
        UdfType,
    },
};

/// Public worker for the LogManager.
pub trait LogSender: Send + Sync {
    fn send_logs(&self, logs: Vec<LogEvent>);
    fn shutdown(&self) -> anyhow::Result<()>;
}

pub struct NoopLogSender;

impl LogSender for NoopLogSender {
    fn send_logs(&self, _logs: Vec<LogEvent>) {}

    fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Structured log
#[derive(Debug, Clone)]
pub struct LogEvent {
    /// Classifies the category of a LogEvent which defines a schematized
    /// structure on logs. We don't currently enforce these schemas, but
    /// could in the future. For example, for `console.log`, all logs are
    /// under the `_console` topic and follow the following schema:
    /// `{ message: v.string() }`, excluding system fields.
    pub topic: LogTopic,
    /// Rough timestamp of when this event was created, for the user's benefit.
    /// We provide no guarantees on the consistency of this timestamp across
    /// topics and log sources - it's best-effort.
    /// This timestamp is serialized to milliseconds.
    pub timestamp: UnixTimestamp,
    /// The log source which generated this log event. This is NOT a user-facing
    /// field and is just used for common fields shared by different log
    /// topics.
    pub source: EventSource,
    /// We use a serde_json::Map to preserve insertion order and still use the
    /// json! macro
    pub payload: serde_json::Map<String, JsonValue>,
}

/// Structured log
impl LogEvent {
    pub fn default_for_verification<RT: Runtime>(runtime: &RT) -> anyhow::Result<Self> {
        let mut payload = serde_json::Map::new();
        payload.insert("message".to_string(), "Convex connection test".into());
        Ok(Self {
            topic: LogTopic::Verification,
            source: EventSource::System,
            payload,
            timestamp: runtime.unix_timestamp(),
        })
    }

    pub fn construct_exception(
        err: &JsError,
        timestamp: UnixTimestamp,
        source: EventSource,
        udf_server_version: Option<&str>,
        identity: &InertIdentity,
    ) -> anyhow::Result<Self> {
        let frames: Option<Vec<String>> = err
            .frames
            .as_ref()
            .map(|frames| frames.0.iter().map(|frame| frame.to_string()).collect());
        let JsonValue::Object(payload) = serde_json::json!({
            "message": err.message,
            "frames": frames,
            "udfServerVersion": udf_server_version,
            "userIdentifier": identity.user_identifier(),
        }) else {
            anyhow::bail!("could not create JSON object for LogEvent Exception");
        };
        let event = Self {
            topic: LogTopic::Exception,
            timestamp,
            source,
            payload,
        };
        Ok(event)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn sample_exception<RT: Runtime>(runtime: &RT) -> anyhow::Result<Self> {
        use sync_types::UserIdentifier;

        let source = EventSource::Function(FunctionEventSource {
            context: ExecutionContext::new_for_test(),
            path: "test".to_string(),
            udf_type: UdfType::Action,
            module_environment: ModuleEnvironment::Isolate,
            cached: None,
        });
        Self::construct_exception(
            &JsError::from_frames_for_test("test_message", vec!["test_frame_1", "test_frame_2"])?,
            runtime.unix_timestamp(),
            source,
            Some("1.5.1"),
            &InertIdentity::User(UserIdentifier("test|user".to_string())),
        )
    }
}

impl TryFrom<LogEvent> for serde_json::Map<String, JsonValue> {
    type Error = anyhow::Error;

    fn try_from(event: LogEvent) -> Result<Self, Self::Error> {
        let mut fields = serde_json::Map::new();
        // Global system fields
        fields.insert("_topic".to_string(), event.topic.try_into()?);
        let ms = event.timestamp.as_ms_since_epoch()?;
        fields.insert("_timestamp".to_string(), ms.into());
        // Source system fields
        match event.source {
            EventSource::Function(f) => {
                fields.insert("_functionPath".to_string(), f.path.into());
                fields.insert(
                    "_functionType".to_string(),
                    serde_json::to_value(f.udf_type)?,
                );
                fields.insert("_functionCached".to_string(), f.cached.into());
            },
            EventSource::System => {},
        }
        // Inline user payload
        for (k, v) in event.payload {
            fields.insert(k, v);
        }

        Ok(fields)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub enum LogTopic {
    /// Topic for logs generated by `console.*` events. This is considered a
    /// `SystemLogTopic` since the topic is generated by the backend.
    Console,
    /// Topic for verification logs. These are issued on sink startup and are
    /// used to test that the backend can authenticate with the sink.
    Verification,
    /// Topic that records UDF executions and provides information on the
    /// execution.
    UdfExecutionRecord,
    /// Topic for deployment audit logs. These are issued when developers
    /// interact with a deployment.
    DeploymentAuditLog,
    /// Topic for exceptions. These happen when a UDF raises an exception from
    /// JS
    Exception,
    /// User-specified topics which are emitted via the client-side UDF
    /// capability See here for more details: https://www.notion.so/Log-Streaming-in-Convex-19a1dfadd6924c33b29b2796b0f5b2e2
    User(String),
}

impl TryFrom<LogTopic> for JsonValue {
    type Error = anyhow::Error;

    fn try_from(value: LogTopic) -> Result<Self, Self::Error> {
        let topic = match value {
            LogTopic::Console => "_console".to_string(),
            LogTopic::Verification => "_verification".to_string(),
            LogTopic::UdfExecutionRecord => "_execution_record".to_string(),
            LogTopic::DeploymentAuditLog => "_audit_log".to_string(),
            LogTopic::Exception => "_exception".to_string(),
            LogTopic::User(s) => s,
        };
        Ok(JsonValue::String(topic))
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub enum EventSource {
    Function(FunctionEventSource),
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub struct FunctionEventSource {
    pub context: ExecutionContext,
    pub path: String,
    pub udf_type: UdfType,
    pub module_environment: ModuleEnvironment,
    // Only queries can be cached, so this is only Some for queries. This is important
    // information to transmit to the client to distinguish from logs users explicitly created
    // and logs that we created for by redoing a query when its readset changes.
    pub cached: Option<bool>,
}

impl HeapSize for FunctionEventSource {
    fn heap_size(&self) -> usize {
        self.path.heap_size() + self.udf_type.heap_size() + self.cached.heap_size()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{
        json,
        Value as JsonValue,
    };

    use crate::{
        execution_context::ExecutionContext,
        log_streaming::{
            EventSource,
            FunctionEventSource,
            LogEvent,
            LogTopic,
        },
        runtime::UnixTimestamp,
        types::{
            ModuleEnvironment,
            UdfType,
        },
    };

    #[test]
    fn test_serialization_of_console_log_event() -> anyhow::Result<()> {
        let JsonValue::Object(payload) = json!({
            "message": "my test log",
        }) else {
            unreachable!()
        };
        let event = LogEvent {
            topic: LogTopic::Console,
            timestamp: UnixTimestamp::from_millis(1000),
            source: EventSource::Function(FunctionEventSource {
                context: ExecutionContext::new_for_test(),
                path: "test:test".to_string(),
                udf_type: UdfType::Query,
                module_environment: ModuleEnvironment::Isolate,
                cached: Some(true),
            }),
            payload,
        };

        // Test serialization
        let fields: serde_json::Map<String, JsonValue> = event.try_into()?;
        let value = serde_json::to_value(&fields)?;
        assert_eq!(
            value,
            json!({
                "_topic": "_console",
                "_timestamp": 1000,
                "_functionPath": "test:test",
                "_functionType": "query",
                "_functionCached": true,
                "message": "my test log",
            })
        );
        Ok(())
    }

    #[test]
    fn test_serialization_of_user_log_event() -> anyhow::Result<()> {
        let JsonValue::Object(payload) = json!({
            "message": "my test log",
            "abc": "aaa",
        }) else {
            unreachable!()
        };
        let event = LogEvent {
            topic: LogTopic::User("myTopic".to_string()),
            timestamp: UnixTimestamp::from_millis(1000),
            source: EventSource::Function(FunctionEventSource {
                context: ExecutionContext::new_for_test(),
                path: "test:test".to_string(),
                udf_type: UdfType::Query,
                module_environment: ModuleEnvironment::Isolate,
                cached: Some(true),
            }),
            payload,
        };

        // Test serialization
        let fields: serde_json::Map<String, JsonValue> = event.try_into()?;
        let value = serde_json::to_value(&fields)?;
        assert_eq!(
            value,
            json!({
                "_topic": "myTopic",
                "_timestamp": 1000,
                "_functionPath": "test:test",
                "_functionType": "query",
                "_functionCached": true,
                "message": "my test log",
                "abc": "aaa",
            })
        );
        Ok(())
    }
}
