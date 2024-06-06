use std::sync::Arc;

use async_lru::async_lru::AsyncLru;
use async_trait::async_trait;
use common::{
    document::ParsedDocument,
    knobs::{
        FUNRUN_MODULE_CACHE_SIZE,
        FUNRUN_MODULE_MAX_CONCURRENCY,
    },
    runtime::Runtime,
};
use database::Transaction;
use futures::FutureExt;
use isolate::environment::helpers::module_loader::get_module_and_prefetch;
use model::{
    config::module_loader::ModuleLoader,
    modules::{
        module_versions::{
            FullModuleSource,
            ModuleVersion,
        },
        types::ModuleMetadata,
        ModuleModel,
        MODULE_VERSIONS_TABLE,
    },
};
use storage::Storage;
use value::ResolvedDocumentId;

use crate::in_memory_indexes::TransactionIngredients;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ModuleCacheKey {
    instance_name: String,
    module_id: ResolvedDocumentId,
    module_version: ModuleVersion,
}

#[derive(Clone)]
pub(crate) struct ModuleCache<RT: Runtime>(AsyncLru<RT, ModuleCacheKey, FullModuleSource>);

impl<RT: Runtime> ModuleCache<RT> {
    pub(crate) fn new(rt: RT) -> Self {
        Self(AsyncLru::new(
            rt,
            *FUNRUN_MODULE_CACHE_SIZE,
            *FUNRUN_MODULE_MAX_CONCURRENCY,
            "function_runner_module_cache",
        ))
    }
}

pub(crate) struct FunctionRunnerModuleLoader<RT: Runtime> {
    pub cache: ModuleCache<RT>,
    pub instance_name: String,
    pub transaction_ingredients: TransactionIngredients<RT>,
    pub modules_storage: Arc<dyn Storage>,
}

#[async_trait]
impl<RT: Runtime> ModuleLoader<RT> for FunctionRunnerModuleLoader<RT> {
    #[minitrace::trace]
    async fn get_module_with_metadata(
        &self,
        tx: &mut Transaction<RT>,
        module_metadata: ParsedDocument<ModuleMetadata>,
    ) -> anyhow::Result<Arc<FullModuleSource>> {
        // The transaction we're getting modules for should be from the same ts as when
        // this module loader was created.
        assert_eq!(tx.begin_timestamp(), self.transaction_ingredients.ts);

        let namespace = tx
            .table_mapping()
            .tablet_namespace(module_metadata.id().table().tablet_id)?;
        // If this transaction wrote to module_versions (true for REPLs), we cannot use
        // the cache, load the module directly.
        let module_versions_table_id = tx
            .table_mapping()
            .namespace(namespace)
            .id(&MODULE_VERSIONS_TABLE)?;
        if tx.writes().has_written_to(&module_versions_table_id) {
            let source = ModuleModel::new(tx)
                .get_source_from_db(module_metadata.id(), module_metadata.latest_version)
                .await?;
            return Ok(Arc::new(source));
        }

        let instance_name = self.instance_name.clone();
        let key = ModuleCacheKey {
            instance_name: self.instance_name.clone(),
            module_id: module_metadata.id(),
            module_version: module_metadata.latest_version,
        };
        let mut transaction = self.transaction_ingredients.clone().try_into()?;
        let modules_storage = self.modules_storage.clone();
        let result = self
            .cache
            .0
            .get_and_prepopulate(
                key.clone(),
                async move {
                    let modules =
                        get_module_and_prefetch(&mut transaction, modules_storage, module_metadata)
                            .await;
                    modules
                        .into_iter()
                        .map(move |((module_id, module_version), source)| {
                            (
                                ModuleCacheKey {
                                    instance_name: instance_name.clone(),
                                    module_id,
                                    module_version,
                                },
                                source,
                            )
                        })
                        .collect()
                }
                .boxed(),
            )
            .await?;
        // Record read dependency on the module version so the transactions
        // read same is the same regardless if we hit the cache or not.
        // This is not technically needed since the module version is immutable,
        // but better safe and consistent that sorry.
        ModuleModel::new(tx).record_module_version_read_dependency(key.module_id)?;

        Ok(result)
    }
}
