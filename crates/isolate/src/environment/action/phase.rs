use std::{
    collections::BTreeMap,
    mem,
    sync::Arc,
};

use anyhow::Context;
use common::{
    components::{
        ComponentDefinitionId,
        ComponentId,
        ComponentPath,
        Reference,
        Resource,
        COMPONENTS_ENABLED,
    },
    runtime::{
        Runtime,
        UnixTimestamp,
    },
    types::ModuleEnvironment,
};
use database::{
    BootstrapComponentsModel,
    Transaction,
};
use errors::ErrorMetadata;
use model::{
    components::ComponentsModel,
    config::module_loader::ModuleLoader,
    environment_variables::{
        types::{
            EnvVarName,
            EnvVarValue,
        },
        EnvironmentVariablesModel,
    },
    modules::{
        module_versions::FullModuleSource,
        types::ModuleMetadata,
        ModuleModel,
    },
    udf_config::UdfConfigModel,
};
use parking_lot::Mutex;
use rand::{
    Rng,
    SeedableRng,
};
use rand_chacha::ChaCha12Rng;
use sync_types::{
    CanonicalizedModulePath,
    ModulePath,
};
use value::TableNamespace;

use crate::{
    concurrency_limiter::ConcurrencyPermit,
    environment::{
        action::task::TaskRequestEnum,
        helpers::{
            permit::with_release_permit,
            Phase,
        },
    },
    timeout::Timeout,
};

/// This struct is similar to UdfPhase. Action execution also has two
/// phases: 1. We start by loading all imported modules, evaluating them, and
/// inserting them into the module map. 2. We find the endpoint and run it.
///
/// Unlike `UdfPhase`, the DB transaction is read-only (used for reading modules
/// and environment variables), and all writes will be handled in their own
/// separate transactions.
pub struct ActionPhase<RT: Runtime> {
    component: ComponentPath,
    phase: Phase,
    pub rt: RT,
    preloaded: ActionPreloaded<RT>,
}

enum ActionPreloaded<RT: Runtime> {
    Created {
        tx: Transaction<RT>,
        module_loader: Arc<dyn ModuleLoader<RT>>,
        system_env_vars: BTreeMap<EnvVarName, EnvVarValue>,
        resources: Arc<Mutex<BTreeMap<Reference, Resource>>>,
    },
    Preloading,
    Ready {
        modules: BTreeMap<CanonicalizedModulePath, (ModuleMetadata, Arc<FullModuleSource>)>,
        env_vars: BTreeMap<EnvVarName, EnvVarValue>,
        rng: Option<ChaCha12Rng>,
        import_time_unix_timestamp: Option<UnixTimestamp>,
    },
}

impl<RT: Runtime> ActionPhase<RT> {
    pub fn new(
        rt: RT,
        component: ComponentPath,
        tx: Transaction<RT>,
        module_loader: Arc<dyn ModuleLoader<RT>>,
        system_env_vars: BTreeMap<EnvVarName, EnvVarValue>,
        resources: Arc<Mutex<BTreeMap<Reference, Resource>>>,
    ) -> Self {
        Self {
            component,
            phase: Phase::Importing,
            rt,
            preloaded: ActionPreloaded::Created {
                tx,
                module_loader,
                system_env_vars,
                resources,
            },
        }
    }

    #[minitrace::trace]
    pub async fn initialize(
        &mut self,
        timeout: &mut Timeout<RT>,
        permit_slot: &mut Option<ConcurrencyPermit>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.phase == Phase::Importing);

        let preloaded = mem::replace(&mut self.preloaded, ActionPreloaded::Preloading);
        let ActionPreloaded::Created {
            mut tx,
            module_loader,
            system_env_vars,
            resources,
        } = preloaded
        else {
            anyhow::bail!("ActionPhase initialized twice");
        };

        let udf_config = with_release_permit(
            timeout,
            permit_slot,
            UdfConfigModel::new(&mut tx, TableNamespace::TODO()).get(),
        )
        .await?;

        let rng = udf_config
            .as_ref()
            .map(|c| ChaCha12Rng::from_seed(c.import_phase_rng_seed));
        let import_time_unix_timestamp = udf_config.as_ref().map(|c| c.import_phase_unix_timestamp);

        let module_metadata = with_release_permit(timeout, permit_slot, async {
            let result = if !*COMPONENTS_ENABLED {
                anyhow::ensure!(self.component.is_root());
                ModuleModel::new(&mut tx)
                    .get_all_metadata(ComponentId::Root)
                    .await?
            } else {
                let metadata = BootstrapComponentsModel::new(&mut tx)
                    .resolve_path(self.component.clone())
                    .await?
                    .context("Failed to find component")?;
                let (component_id, _) = if self.component.is_root() {
                    (ComponentId::Root, ComponentDefinitionId::Root)
                } else {
                    (
                        ComponentId::Child(metadata.id().internal_id()),
                        ComponentDefinitionId::Child(metadata.definition_id),
                    )
                };
                let module_metadata = ModuleModel::new(&mut tx)
                    .get_all_metadata(component_id)
                    .await?;

                let loaded_resources = ComponentsModel::new(&mut tx)
                    .preload_resources(component_id)
                    .await?;
                {
                    let mut resources = resources.lock();
                    *resources = loaded_resources;
                }

                module_metadata
            };
            Ok(result)
        })
        .await?;

        let modules = with_release_permit(timeout, permit_slot, async {
            let mut modules = BTreeMap::new();
            for metadata in module_metadata {
                if metadata.path.is_system() {
                    continue;
                }
                let path = metadata.path.clone();
                let module = module_loader
                    .get_module_with_metadata(&mut tx, metadata.clone())
                    .await?;
                modules.insert(path, (metadata.into_value(), module));
            }
            Ok(modules)
        })
        .await?;

        let mut env_vars = system_env_vars;
        let user_env_vars = with_release_permit(
            timeout,
            permit_slot,
            EnvironmentVariablesModel::new(&mut tx).get_all(),
        )
        .await?;
        env_vars.extend(user_env_vars);

        self.preloaded = ActionPreloaded::Ready {
            modules,
            env_vars,
            rng,
            import_time_unix_timestamp,
        };

        Ok(())
    }

    pub fn get_module(
        &mut self,
        module_path: &ModulePath,
        _timeout: &mut Timeout<RT>,
        _permit: &mut Option<ConcurrencyPermit>,
    ) -> anyhow::Result<Option<FullModuleSource>> {
        let ActionPreloaded::Ready { ref modules, .. } = self.preloaded else {
            anyhow::bail!("Phase not initialized");
        };
        let module = modules
            .get(&module_path.clone().canonicalize())
            .map(|(module, source)| (module, (**source).clone()));

        if let Some((module, _)) = module.as_ref() {
            anyhow::ensure!(
                module.environment == ModuleEnvironment::Isolate,
                "Trying to execute {:?} in isolate, but it is bundled for {:?}.",
                module_path,
                module.environment
            );
        };

        Ok(module.map(|(_, source)| source))
    }

    pub fn begin_execution(&mut self) -> anyhow::Result<()> {
        if self.phase != Phase::Importing {
            anyhow::bail!("Phase was already {:?}", self.phase)
        }
        let ActionPreloaded::Ready { ref mut rng, .. } = self.preloaded else {
            anyhow::bail!("Phase not initialized");
        };
        self.phase = Phase::Executing;
        let rng_seed = self.rt.with_rng(|rng| rng.gen());
        *rng = Some(ChaCha12Rng::from_seed(rng_seed));
        Ok(())
    }

    pub fn get_environment_variable(
        &mut self,
        name: EnvVarName,
    ) -> anyhow::Result<Option<EnvVarValue>> {
        let ActionPreloaded::Ready { ref env_vars, .. } = self.preloaded else {
            anyhow::bail!("Phase not initialized");
        };
        Ok(env_vars.get(&name).cloned())
    }

    pub fn rng(&mut self) -> anyhow::Result<&mut ChaCha12Rng> {
        let ActionPreloaded::Ready { ref mut rng, .. } = self.preloaded else {
            anyhow::bail!("Phase not initialized");
        };
        let Some(ref mut rng) = rng else {
            // Fail for old module without import time rng populated.
            anyhow::bail!(ErrorMetadata::bad_request(
                "NoRandomDuringImport",
                "Math.random unsupported at import time"
            ));
        };
        Ok(rng)
    }

    pub fn unix_timestamp(&self) -> anyhow::Result<UnixTimestamp> {
        let ActionPreloaded::Ready {
            import_time_unix_timestamp,
            ..
        } = self.preloaded
        else {
            anyhow::bail!("Phase not initialized");
        };
        let timestamp = if self.phase == Phase::Importing {
            let Some(unix_timestamp) = import_time_unix_timestamp else {
                // Fail for old modules without import time timestamp populated.
                anyhow::bail!(ErrorMetadata::bad_request(
                    "NoDateDuringImport",
                    "Date unsupported at import time"
                ));
            };
            unix_timestamp
        } else {
            self.rt.unix_timestamp()
        };
        Ok(timestamp)
    }

    pub fn require_executing(&self, request: &TaskRequestEnum) -> anyhow::Result<()> {
        if self.phase == Phase::Importing {
            anyhow::bail!(ErrorMetadata::bad_request(
                format!("No{}DuringImport", request.name_for_error()),
                format!(
                    "{} unsupported at import time",
                    request.description_for_error()
                ),
            ));
        }
        Ok(())
    }
}
