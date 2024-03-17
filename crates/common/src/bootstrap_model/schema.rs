use std::str::FromStr;

use errors::ErrorMetadata;
use value::{
    id_v6::DocumentIdV6,
    GenericDocumentId,
    ResolvedDocumentId,
    TableId,
    TableMapping,
};

pub use super::{
    schema_metadata::SchemaMetadata,
    schema_state::SchemaState,
};

pub fn parse_schema_id(
    schema_id: &str,
    table_mapping: &TableMapping,
) -> anyhow::Result<ResolvedDocumentId> {
    // Try parsing as a document ID with TableId first
    match GenericDocumentId::<TableId>::from_str(schema_id) {
        Ok(s) => s.map_table(table_mapping.inject_table_number()),
        Err(_) => {
            // Try parsing as an IDv6 ID
            let id = DocumentIdV6::decode(schema_id)?;
            id.to_resolved(&table_mapping.inject_table_id())
        },
    }
}

pub fn invalid_schema_id(schema_id: &str) -> ErrorMetadata {
    ErrorMetadata::bad_request(
        "InvalidSchemaId",
        format!("Invalid schema id: {}", schema_id),
    )
}
