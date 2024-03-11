use std::convert::{
    TryFrom,
    TryInto,
};

use anyhow::Context;
use common::{
    self,
    runtime::Runtime,
};
use model::{
    self,
    modules::args_validator::ArgsValidator,
};
use serde_json::{
    json,
    Value as JsonValue,
};
use value::ConvexArray;

use crate::{
    environment::IsolateEnvironment,
    execution_scope::ExecutionScope,
    helpers::UdfArgsJson,
};

impl<'a, 'b: 'a, RT: Runtime, E: IsolateEnvironment<RT>> ExecutionScope<'a, 'b, RT, E> {
    #[convex_macro::v8_op]

    pub fn op_validate_args(
        &mut self,
        validator: JsonValue,
        args: UdfArgsJson,
    ) -> anyhow::Result<JsonValue> {
        let JsonValue::String(validator_string) = validator.clone() else {
            return Err(anyhow::anyhow!("export_args result not a string"));
        };

        let args_validator: ArgsValidator =
            match serde_json::from_str::<JsonValue>(&validator_string) {
                Ok(args_json) => match ArgsValidator::try_from(args_json) {
                    Ok(validator) => validator,
                    Err(err) => return Err(err),
                },
                Err(json_error) => {
                    let message =
                        format!("Unable to parse JSON returned from `exportArgs`: {json_error}");
                    return Err(anyhow::anyhow!(message));
                },
            };

        let args_array = args
            .into_arg_vec()
            .into_iter()
            .map(|arg| arg.try_into())
            .collect::<anyhow::Result<Vec<_>>>()
            .and_then(ConvexArray::try_from)
            .map_err(|err| anyhow::anyhow!(format!("{}", err)))?;

        let state = self.state_mut()?;
        let (table_mapping, virtual_table_mapping) = state.environment.get_all_table_mappings()?;
        match args_validator.check_args(&args_array, &table_mapping, &virtual_table_mapping)? {
            Some(js_error) => Ok(json!({
                "valid": false,
                "message": format!("{}", js_error)
            })),
            None => Ok(json!({
                "valid": true,
            })),
        }
    }
}
