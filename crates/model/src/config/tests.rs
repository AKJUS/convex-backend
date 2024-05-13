use std::collections::BTreeMap;

use common::{
    components::{
        CanonicalizedComponentModulePath,
        ComponentId,
    },
    db_schema,
    object_validator,
    schemas::{
        validator::{
            FieldValidator,
            Validator,
        },
        DocumentSchema,
    },
    types::ModuleEnvironment,
};
use database::{
    test_helpers::DbFixtures,
    SchemaModel,
};
use keybroker::Identity;
use maplit::btreemap;
use runtime::testing::TestRuntime;
use value::heap_size::WithHeapSize;

use crate::{
    auth::AuthInfoModel,
    config::{
        types::{
            ConfigMetadata,
            ModuleConfig,
        },
        ConfigModel,
    },
    modules::module_versions::AnalyzedModule,
    test_helpers::DbFixturesWithModel,
    udf_config::types::UdfConfig,
};

#[convex_macro::test_runtime]
async fn test_config(rt: TestRuntime) -> anyhow::Result<()> {
    let database = DbFixtures::new(&rt.clone()).await?.with_model().await?.db;

    // Initialize config
    let mut tx = database.begin(Identity::system()).await?;
    let config_metadata = ConfigMetadata::test_example();
    let module1 = ModuleConfig {
        path: "a/b/c.js".parse()?,
        source: "// some js".to_string(),
        source_map: None,
        environment: ModuleEnvironment::Isolate,
    };
    let module2 = ModuleConfig {
        path: "d/e/f.js".parse()?,
        source: "// some other js".to_string(),
        source_map: Some("// source map".to_string()),
        environment: ModuleEnvironment::Isolate,
    };
    let p1 = CanonicalizedComponentModulePath {
        component: ComponentId::Root,
        module_path: module1.path.clone().canonicalize(),
    };
    let p2 = CanonicalizedComponentModulePath {
        component: ComponentId::Root,
        module_path: module2.path.clone().canonicalize(),
    };
    ConfigModel::new(&mut tx)
        .apply(
            config_metadata.clone(),
            vec![module1.clone(), module2.clone()],
            UdfConfig::new_for_test(&rt, "1000.0.0".parse()?),
            None, // source storage key
            btreemap! {
                p1 => AnalyzedModule {
                    functions: WithHeapSize::default(),
                    http_routes: None,
                    cron_specs: None,
                    source_mapped: None,
                },
                p2 =>  AnalyzedModule {
                    functions: WithHeapSize::default(),
                    http_routes: None,
                    cron_specs: None,
                    source_mapped: None,
                },
            },
            None,
        )
        .await?;
    database.commit(tx).await?;

    // Fetch it back and it make sure it's there.
    let mut tx = database.begin(Identity::system()).await?;
    let (config_metadata_read, modules_read, ..) = ConfigModel::new(&mut tx)
        .get()
        .await
        .expect("getting config should succeed");
    assert_eq!(config_metadata, config_metadata_read);
    assert_eq!(modules_read, vec![module1, module2]);
    database.commit(tx).await?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_config_large_modules(rt: TestRuntime) -> anyhow::Result<()> {
    let database = DbFixtures::new(&rt.clone()).await?.with_model().await?.db;

    // Initialize config
    let mut tx = database.begin(Identity::system()).await?;
    let config_metadata = ConfigMetadata::test_example();

    // Write 20MB of modules
    let modules: Vec<_> = (0..10)
        .map(|i| {
            ModuleConfig {
                path: format!("mod_{i}.js").parse().unwrap(),
                source: "// some js".to_string() + &"a".repeat(1 << 21), // 2MB
                source_map: None,
                environment: ModuleEnvironment::Isolate,
            }
        })
        .collect();
    let analyzed_result = modules
        .iter()
        .map(|m| {
            (
                CanonicalizedComponentModulePath {
                    component: ComponentId::Root,
                    module_path: m.path.clone().canonicalize(),
                },
                AnalyzedModule {
                    functions: WithHeapSize::default(),
                    http_routes: None,
                    cron_specs: None,
                    source_mapped: None,
                },
            )
        })
        .collect();
    ConfigModel::new(&mut tx)
        .apply(
            config_metadata.clone(),
            modules.clone(),
            UdfConfig::new_for_test(&rt, "1000.0.0".parse()?),
            None, // source storage key
            analyzed_result,
            None,
        )
        .await?;
    database.commit(tx).await?;

    let mut tx = database.begin(Identity::system()).await?;
    let (config_metadata_read, modules_read, ..) = ConfigModel::new(&mut tx)
        .get()
        .await
        .expect("getting config should succeed");
    assert_eq!(config_metadata, config_metadata_read);
    assert_eq!(modules, modules_read);

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_config_delete_auth_info(rt: TestRuntime) -> anyhow::Result<()> {
    let database = DbFixtures::new(&rt.clone()).await?.with_model().await?.db;

    // Initialize config
    let mut tx = database.begin(Identity::system()).await?;
    let config_metadata = ConfigMetadata::test_example();
    ConfigModel::new(&mut tx)
        .apply(
            config_metadata,
            vec![],
            UdfConfig::new_for_test(&rt, "1000.0.0".parse()?),
            None, // source storage key
            BTreeMap::new(),
            None,
        )
        .await?;
    database.commit(tx).await?;

    // Delete auth info.
    let mut tx = database.begin(Identity::system()).await?;
    ConfigModel::new(&mut tx)
        .apply(
            ConfigMetadata {
                functions: "convex/".to_string(),
                auth_info: vec![],
            },
            vec![],
            UdfConfig::new_for_test(&rt, "1000.0.0".parse()?),
            None, // source package
            BTreeMap::new(),
            None,
        )
        .await?;
    database.commit(tx).await?;

    // Fetch it back and make sure it's gone
    let mut tx = database.begin(Identity::system()).await?;
    assert!(AuthInfoModel::new(&mut tx)
        .get()
        .await
        .expect("getting auth info should succeed")
        .is_empty());
    database.commit(tx).await?;

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_schema_in_deployment_audit_log(rt: TestRuntime) -> anyhow::Result<()> {
    let database = DbFixtures::new(&rt.clone()).await?.with_model().await?.db;

    // Set a config without a schema
    let mut tx = database.begin(Identity::system()).await?;
    let config_metadata = ConfigMetadata::test_example();
    ConfigModel::new(&mut tx)
        .apply(
            config_metadata.clone(),
            vec![],
            UdfConfig::new_for_test(&rt, "1000.0.0".parse()?),
            None,
            btreemap! {},
            None,
        )
        .await?;
    database.commit(tx).await?;

    // Add a new schema
    let mut tx = database.begin(Identity::system()).await?;
    let first_schema = db_schema!("table1" => DocumentSchema::Any);
    let mut model = SchemaModel::new(&mut tx);
    let (first_schema_id, _) = model.submit_pending(first_schema.clone()).await?;
    model.mark_validated(first_schema_id).await?;
    let (config_diff, schema) = ConfigModel::new(&mut tx)
        .apply(
            config_metadata.clone(),
            vec![],
            UdfConfig::new_for_test(&rt, "1000.0.0".parse()?),
            None,
            btreemap! {},
            Some(first_schema_id),
        )
        .await?;
    database.commit(tx).await?;

    let schema_diff = config_diff.schema_diff.unwrap();
    assert_eq!(schema_diff.previous_schema, None);
    assert_eq!(schema_diff.next_schema, Some(first_schema.clone()));
    assert_eq!(schema, Some(first_schema.clone()));

    // Edit the schema
    let mut tx = database.begin(Identity::system()).await?;
    let mut model = SchemaModel::new(&mut tx);
    let second_schema = db_schema!(
        "table1" => DocumentSchema::Any,
        "table2" => DocumentSchema::Union(vec![
            object_validator!("field1" => FieldValidator::required_field_type(Validator::String))
        ])
    );
    let (second_schema_id, _) = model.submit_pending(second_schema.clone()).await?;
    model.mark_validated(second_schema_id).await?;
    let (config_diff, schema) = ConfigModel::new(&mut tx)
        .apply(
            config_metadata.clone(),
            vec![],
            UdfConfig::new_for_test(&rt, "1000.0.0".parse()?),
            None,
            btreemap! {},
            Some(second_schema_id),
        )
        .await?;
    database.commit(tx).await?;

    let schema_diff = config_diff.schema_diff.unwrap();
    assert_eq!(schema_diff.previous_schema, Some(first_schema));
    assert_eq!(schema_diff.next_schema, Some(second_schema.clone()));
    assert_eq!(schema, Some(second_schema.clone()));

    // Remove the schema
    let mut tx = database.begin(Identity::system()).await?;
    let (config_diff, schema) = ConfigModel::new(&mut tx)
        .apply(
            config_metadata.clone(),
            vec![],
            UdfConfig::new_for_test(&rt, "1000.0.0".parse()?),
            None,
            btreemap! {},
            None,
        )
        .await?;
    database.commit(tx).await?;

    let schema_diff = config_diff.schema_diff.unwrap();
    assert_eq!(schema_diff.previous_schema, Some(second_schema));
    assert_eq!(schema_diff.next_schema, None);
    assert!(schema.is_none());

    Ok(())
}
