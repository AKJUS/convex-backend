use std::{
    collections::HashMap,
    sync::Arc,
};

use anyhow::anyhow;
use common::{
    components::ComponentId,
    document::ParsedDocument,
    runtime::Runtime,
};
use database::Transaction;
use deno_core::ModuleSpecifier;
use model::{
    modules::{
        module_versions::FullModuleSource,
        types::ModuleMetadata,
        ModuleModel,
    },
    source_packages::{
        types::SourcePackageId,
        upload_download::download_package,
        SourcePackageModel,
    },
};
use storage::Storage;
use sync_types::CanonicalizedModulePath;
use value::{
    ResolvedDocumentId,
    TableNamespace,
    TabletId,
};

use crate::{
    isolate::CONVEX_SCHEME,
    metrics::module_load_timer,
};

#[minitrace::trace]
pub async fn get_module_and_prefetch<RT: Runtime>(
    tx: &mut Transaction<RT>,
    modules_storage: Arc<dyn Storage>,
    module_metadata: ParsedDocument<ModuleMetadata>,
) -> HashMap<(ResolvedDocumentId, SourcePackageId), anyhow::Result<FullModuleSource>> {
    let _timer = module_load_timer("package");
    let all_source_result = download_module_source_from_package(
        tx,
        modules_storage,
        module_metadata.id().table().tablet_id,
        module_metadata.source_package_id,
    )
    .await;
    match all_source_result {
        Err(e) => {
            let mut result = HashMap::new();
            result.insert(
                (module_metadata.id(), module_metadata.source_package_id),
                Err(e),
            );
            result
        },
        Ok(all_source) => all_source
            .into_iter()
            .map(|(path, source)| (path, Ok(source)))
            .collect(),
    }
}

#[minitrace::trace]
async fn download_module_source_from_package<RT: Runtime>(
    tx: &mut Transaction<RT>,
    modules_storage: Arc<dyn Storage>,
    modules_tablet: TabletId,
    source_package_id: SourcePackageId,
) -> anyhow::Result<HashMap<(ResolvedDocumentId, SourcePackageId), FullModuleSource>> {
    let namespace = tx.table_mapping().tablet_namespace(modules_tablet)?;
    let mut result = HashMap::new();
    let source_package = SourcePackageModel::new(tx, namespace)
        .get(source_package_id)
        .await?;
    let mut package = download_package(
        modules_storage,
        source_package.storage_key.clone(),
        source_package.sha256.clone(),
    )
    .await?;
    let component = match namespace {
        // TODO(lee) global namespace should not have modules, but for existing data this is how
        // it's represented.
        TableNamespace::Global => ComponentId::Root,
        TableNamespace::RootComponent => ComponentId::Root,
        TableNamespace::ByComponent(id) => ComponentId::Child(id),
    };
    for module_metadata in ModuleModel::new(tx).get_all_metadata(component).await? {
        match package.remove(&module_metadata.path) {
            None => {
                anyhow::bail!(
                    "module {:?} not found in package {:?}",
                    module_metadata.path,
                    source_package_id
                );
            },
            Some(source) => {
                result.insert(
                    (module_metadata.id(), module_metadata.source_package_id),
                    FullModuleSource {
                        source: source.source,
                        source_map: source.source_map,
                    },
                );
            },
        }
    }
    Ok(result)
}

pub fn module_specifier_from_path(
    path: &CanonicalizedModulePath,
) -> anyhow::Result<ModuleSpecifier> {
    let url = format!("{CONVEX_SCHEME}:/{}", path.as_str());
    Ok(ModuleSpecifier::parse(&url)?)
}

pub fn module_specifier_from_str(path: &str) -> anyhow::Result<ModuleSpecifier> {
    Ok(ModuleSpecifier::parse(path)?)
}

pub fn path_from_module_specifier(
    spec: &ModuleSpecifier,
) -> anyhow::Result<CanonicalizedModulePath> {
    let spec_str = spec.as_str();
    let prefix = format!("{CONVEX_SCHEME}:/");
    spec_str
        .starts_with(&prefix)
        .then(|| {
            spec_str[prefix.len()..]
                .to_string()
                .parse::<CanonicalizedModulePath>()
        })
        .transpose()?
        .ok_or(anyhow!("module specifier did not start with {}", prefix))
}
