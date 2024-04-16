use std::{
    collections::BTreeMap,
    convert::TryInto,
    sync::LazyLock,
};

use anyhow::Context;
use common::{
    document::{
        ParsedDocument,
        ResolvedDocument,
    },
    interval::{
        BinaryKey,
        Interval,
    },
    maybe_val,
    query::{
        IndexRange,
        IndexRangeExpression,
        Order,
        Query,
    },
    runtime::Runtime,
    types::{
        IndexName,
        ModuleEnvironment,
    },
    value::{
        ConvexValue,
        ResolvedDocumentId,
        VALUE_TOO_LARGE_SHORT_MSG,
    },
};
use database::{
    defaults::system_index,
    unauthorized_error,
    ResolvedQuery,
    SystemMetadataModel,
    Transaction,
};
use errors::{
    ErrorMetadata,
    ErrorMetadataAnyhowExt,
};
use metrics::{
    get_module_metadata_timer,
    get_module_version_timer,
};
use sync_types::CanonicalizedModulePath;
use value::{
    values_to_bytes,
    FieldPath,
    TableName,
};

use self::{
    module_versions::{
        AnalyzedModule,
        ModuleSource,
        ModuleVersion,
        ModuleVersionMetadata,
        SourceMap,
    },
    types::ModuleMetadata,
};
use crate::{
    config::types::ModuleConfig,
    source_packages::types::SourcePackageId,
    SystemIndex,
    SystemTable,
};

pub mod args_validator;
mod metrics;
pub mod module_versions;
pub mod types;

/// Table name for user modules.
pub static MODULES_TABLE: LazyLock<TableName> =
    LazyLock::new(|| "_modules".parse().expect("Invalid built-in module table"));

/// Table name for the versions of a module.
pub static MODULE_VERSIONS_TABLE: LazyLock<TableName> = LazyLock::new(|| {
    "_module_versions"
        .parse()
        .expect("Invalid built-in module table")
});

/// Field pointing to the `ModuleMetadata` document from
/// `ModuleVersionMetadata`.
static MODULE_ID_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "module_id".parse().expect("Invalid built-in field"));
/// Field for a module's version in `ModuleVersionMetadata`.
static VERSION_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "version".parse().expect("Invalid built-in field"));

/// Field for a module's path in `ModuleMetadata`.
static PATH_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "path".parse().expect("Invalid built-in field"));
/// Field for a module's deleted flag in `ModuleMetadata`.
static DELETED_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "deleted".parse().expect("Invalid built-in field"));

pub static MODULE_INDEX_BY_PATH: LazyLock<IndexName> =
    LazyLock::new(|| system_index(&MODULES_TABLE, "by_path"));
pub static MODULE_INDEX_BY_DELETED: LazyLock<IndexName> =
    LazyLock::new(|| system_index(&MODULES_TABLE, "by_deleted"));
pub static MODULE_VERSION_INDEX: LazyLock<IndexName> =
    LazyLock::new(|| system_index(&MODULE_VERSIONS_TABLE, "by_module_and_version"));

pub struct ModulesTable;
impl SystemTable for ModulesTable {
    fn table_name(&self) -> &'static TableName {
        &MODULES_TABLE
    }

    fn indexes(&self) -> Vec<SystemIndex> {
        vec![
            SystemIndex {
                name: MODULE_INDEX_BY_PATH.clone(),
                fields: vec![PATH_FIELD.clone()].try_into().unwrap(),
            },
            SystemIndex {
                name: MODULE_INDEX_BY_DELETED.clone(),
                fields: vec![DELETED_FIELD.clone(), PATH_FIELD.clone()]
                    .try_into()
                    .unwrap(),
            },
        ]
    }

    fn validate_document(&self, document: ResolvedDocument) -> anyhow::Result<()> {
        ParsedDocument::<ModuleMetadata>::try_from(document).map(|_| ())
    }
}
pub struct ModuleVersionsTable;
impl SystemTable for ModuleVersionsTable {
    fn table_name(&self) -> &'static TableName {
        &MODULE_VERSIONS_TABLE
    }

    fn indexes(&self) -> Vec<SystemIndex> {
        vec![SystemIndex {
            name: MODULE_VERSION_INDEX.clone(),
            fields: vec![MODULE_ID_FIELD.clone(), VERSION_FIELD.clone()]
                .try_into()
                .unwrap(),
        }]
    }

    fn validate_document(&self, document: ResolvedDocument) -> anyhow::Result<()> {
        ParsedDocument::<ModuleVersionMetadata>::try_from(document).map(|_| ())
    }
}

pub struct ModuleModel<'a, RT: Runtime> {
    tx: &'a mut Transaction<RT>,
}

impl<'a, RT: Runtime> ModuleModel<'a, RT> {
    pub fn new(tx: &'a mut Transaction<RT>) -> Self {
        Self { tx }
    }

    /// Returns the registered modules metadata, including system modules.
    pub async fn get_all_metadata(
        &mut self,
    ) -> anyhow::Result<Vec<ParsedDocument<ModuleMetadata>>> {
        let index_range = IndexRange {
            index_name: MODULE_INDEX_BY_DELETED.clone(),
            range: vec![IndexRangeExpression::Eq(
                DELETED_FIELD.clone(),
                maybe_val!(false),
            )],
            order: Order::Asc,
        };
        let index_query = Query::index_range(index_range);
        let mut query_stream = ResolvedQuery::new(self.tx, index_query)?;

        let mut modules = Vec::new();
        while let Some(metadata_document) = query_stream.next(self.tx, None).await? {
            let metadata: ParsedDocument<ModuleMetadata> = metadata_document.try_into()?;
            if !metadata.deleted {
                modules.push(metadata);
            }
        }
        Ok(modules)
    }

    /// Returns all registered modules that aren't system modules.
    pub async fn get_application_modules(
        &mut self,
    ) -> anyhow::Result<BTreeMap<CanonicalizedModulePath, ModuleConfig>> {
        let mut modules = BTreeMap::new();
        for metadata in self.get_all_metadata().await? {
            let path = metadata.path.clone();
            if !path.is_system() {
                let module_version = self
                    .get_version(metadata.id(), metadata.latest_version)
                    .await?
                    .into_value();
                let module_config = ModuleConfig {
                    path: path.clone().into(),
                    source: module_version.source,
                    source_map: module_version.source_map,
                    environment: module_version.environment,
                };
                if modules.insert(path.clone(), module_config).is_some() {
                    panic!("Duplicate application module at {:?}", path);
                }
            }
        }
        Ok(modules)
    }

    pub async fn get_version(
        &mut self,
        module_id: ResolvedDocumentId,
        version: ModuleVersion,
    ) -> anyhow::Result<ParsedDocument<ModuleVersionMetadata>> {
        let timer = get_module_version_timer();
        let module_id_value: ConvexValue = module_id.into();
        let index_range = IndexRange {
            index_name: MODULE_VERSION_INDEX.clone(),
            range: vec![
                IndexRangeExpression::Eq(MODULE_ID_FIELD.clone(), module_id_value.into()),
                IndexRangeExpression::Eq(VERSION_FIELD.clone(), ConvexValue::from(version).into()),
            ],
            order: Order::Asc,
        };
        let module_query = Query::index_range(index_range);
        let mut query_stream = ResolvedQuery::new(self.tx, module_query)?;
        let module_version = query_stream
            .expect_at_most_one(self.tx)
            .await?
            .context(format!(
                "Dangling module version reference: {module_id}@{version}"
            ))?
            .try_into()?;
        timer.finish();
        Ok(module_version)
    }

    /// Helper function to get a module at the latest version.
    pub async fn get_metadata(
        &mut self,
        path: CanonicalizedModulePath,
    ) -> anyhow::Result<Option<ParsedDocument<ModuleMetadata>>> {
        let timer = get_module_metadata_timer();
        if path.is_system() && !(self.tx.identity().is_admin() || self.tx.identity().is_system()) {
            anyhow::bail!(unauthorized_error("get_module"))
        }
        let include_deleted = false;
        let module_metadata = match self.module_metadata(path, include_deleted).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        timer.finish();
        Ok(Some(module_metadata))
    }

    /// Put a module's source at a given path.
    pub async fn put(
        &mut self,
        path: CanonicalizedModulePath,
        source: ModuleSource,
        source_package_id: Option<SourcePackageId>,
        source_map: Option<SourceMap>,
        analyze_result: Option<AnalyzedModule>,
        environment: ModuleEnvironment,
    ) -> anyhow::Result<()> {
        if !(self.tx.identity().is_admin() || self.tx.identity().is_system()) {
            anyhow::bail!(unauthorized_error("put_module"));
        }
        if path.is_system() {
            anyhow::bail!("You cannot push a function under '_system/'");
        }
        anyhow::ensure!(
            path.is_deps() || analyze_result.is_some(),
            "AnalyzedModule is required for non-dependency modules"
        );
        // If there was a previously deleted document, it is important to replace
        // it instead of adding a new one, in order to have at most one document
        // for each path.
        let include_deleted = true;
        let (module_id, version) = match self.module_metadata(path.clone(), include_deleted).await?
        {
            Some(module_metadata) => {
                let previous_version = module_metadata.latest_version;
                let latest_version = previous_version + 1;
                let new_metadata = ModuleMetadata {
                    path,
                    latest_version,
                    deleted: false,
                };
                SystemMetadataModel::new(self.tx)
                    .replace(module_metadata.id(), new_metadata.try_into()?)
                    .await?;

                // Delete the old module version since it has no more references.
                let previous_version_id = self
                    .get_version(module_metadata.id(), previous_version)
                    .await?
                    .id();
                SystemMetadataModel::new(self.tx)
                    .delete(previous_version_id)
                    .await?;

                (module_metadata.id(), latest_version)
            },
            None => {
                let version = 0;
                let new_metadata = ModuleMetadata {
                    path,
                    latest_version: version,
                    deleted: false,
                };

                let document_id = SystemMetadataModel::new(self.tx)
                    .insert(&MODULES_TABLE, new_metadata.try_into()?)
                    .await?;
                (document_id, version)
            },
        };
        let new_version = ModuleVersionMetadata {
            module_id: module_id.into(),
            source,
            source_package_id,
            source_map,
            version,
            environment,
            analyze_result,
        }.try_into()
        .map_err(|e: anyhow::Error| e.map_error_metadata(|em| {
            if em.short_msg == VALUE_TOO_LARGE_SHORT_MSG {
                // Remap the ValueTooLargeError message to something more specific
                // to the modules use case.
                let message = format!(
                    "The functions, source maps, and their dependencies in \"convex/\" are too large. See our docs (https://docs.convex.dev/using/writing-convex-functions#using-libraries) for more details. You can also run `npx convex deploy -v` to print out each source file's bundled size.\n{}", em.msg
                );
                ErrorMetadata::bad_request(
                    "ModulesTooLarge",
                    message,
                )
            } else {
                em
            }
        }))?;
        SystemMetadataModel::new(self.tx)
            .insert(&MODULE_VERSIONS_TABLE, new_version)
            .await?;
        Ok(())
    }

    /// Delete a module, making it inaccessible for subsequent transactions.
    pub async fn delete(&mut self, path: CanonicalizedModulePath) -> anyhow::Result<()> {
        if !(self.tx.identity().is_admin() || self.tx.identity().is_system()) {
            anyhow::bail!(unauthorized_error("delete_module"));
        }
        let include_deleted = false;
        if let Some(module_metadata) = self.module_metadata(path, include_deleted).await? {
            let module_id = module_metadata.id();
            SystemMetadataModel::new(self.tx).delete(module_id).await?;

            // Delete the module version since it has no more references.
            let module_version = self
                .get_version(module_id, module_metadata.latest_version)
                .await?;
            SystemMetadataModel::new(self.tx)
                .delete(module_version.id())
                .await?;
        }
        Ok(())
    }

    #[convex_macro::instrument_future]
    async fn module_metadata(
        &mut self,
        path: CanonicalizedModulePath,
        include_deleted: bool,
    ) -> anyhow::Result<Option<ParsedDocument<ModuleMetadata>>> {
        let index_range = IndexRange {
            index_name: MODULE_INDEX_BY_PATH.clone(),
            range: vec![IndexRangeExpression::Eq(
                PATH_FIELD.clone(),
                ConvexValue::try_from(String::from(path))?.into(),
            )],
            order: Order::Asc,
        };
        let module_query = Query::index_range(index_range);
        let mut query_stream = ResolvedQuery::new(self.tx, module_query)?;
        let module_document: ParsedDocument<ModuleMetadata> =
            match query_stream.expect_at_most_one(self.tx).await? {
                Some(v) => v.try_into()?,
                None => return Ok(None),
            };
        if !include_deleted && module_document.deleted {
            return Ok(None);
        }
        Ok(Some(module_document))
    }

    pub fn record_module_version_read_dependency(
        &mut self,
        module_id: ResolvedDocumentId,
        version: ModuleVersion,
    ) -> anyhow::Result<()> {
        let fields = vec![MODULE_ID_FIELD.clone(), VERSION_FIELD.clone()];
        let values = vec![
            Some(ConvexValue::from(module_id)),
            Some(ConvexValue::from(version)),
        ];
        let module_index_name = MODULE_VERSION_INDEX
            .clone()
            .map_table(&self.tx.table_mapping().name_to_id())?
            .into();
        self.tx.record_system_table_cache_hit(
            module_index_name,
            fields.try_into().expect("Must be valid"),
            Interval::prefix(BinaryKey::from(values_to_bytes(&values[..]))),
        );
        Ok(())
    }
}
