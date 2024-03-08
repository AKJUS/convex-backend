#![allow(non_snake_case)]

use common::runtime::Runtime;
use errors::ErrorMetadata;
use serde_json::Value as JsonValue;

use super::ActionEnvironment;

impl<RT: Runtime> ActionEnvironment<RT> {
    pub fn syscall_impl(&mut self, name: &str, _args: JsonValue) -> anyhow::Result<JsonValue> {
        match name {
            #[cfg(test)]
            "throwSystemError" => anyhow::bail!("I can't go for that."),
            _ => {
                anyhow::bail!(ErrorMetadata::bad_request(
                    "UnknownOperation",
                    format!("Unknown operation {name}")
                ));
            },
        }
    }
}
