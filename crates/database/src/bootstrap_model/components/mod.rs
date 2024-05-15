pub mod definition;

use std::sync::LazyLock;

use anyhow::Context;
use common::{
    bootstrap_model::components::ComponentMetadata,
    components::{
        ComponentId,
        ComponentName,
        ComponentPath,
    },
    document::{
        ParsedDocument,
        ResolvedDocument,
    },
    maybe_val,
    query::{
        IndexRange,
        IndexRangeExpression,
        Order,
        Query,
    },
    runtime::Runtime,
    types::IndexName,
};
use value::{
    FieldPath,
    InternalId,
    TableIdentifier,
    TableName,
};

use crate::{
    defaults::{
        system_index,
        SystemIndex,
        SystemTable,
    },
    ResolvedQuery,
    Transaction,
};

pub static COMPONENTS_TABLE: LazyLock<TableName> = LazyLock::new(|| {
    "_components"
        .parse()
        .expect("Invalid built-in _components table")
});

pub static COMPONENTS_BY_PARENT_INDEX: LazyLock<IndexName> =
    LazyLock::new(|| system_index(&COMPONENTS_TABLE, "by_parent_and_name"));
static PARENT_FIELD: LazyLock<FieldPath> = LazyLock::new(|| "parent".parse().unwrap());
static NAME_FIELD: LazyLock<FieldPath> = LazyLock::new(|| "name".parse().unwrap());

pub struct ComponentsTable;

impl SystemTable for ComponentsTable {
    fn table_name(&self) -> &'static TableName {
        &COMPONENTS_TABLE
    }

    fn indexes(&self) -> Vec<SystemIndex> {
        vec![SystemIndex {
            name: COMPONENTS_BY_PARENT_INDEX.clone(),
            fields: vec![PARENT_FIELD.clone(), NAME_FIELD.clone()]
                .try_into()
                .unwrap(),
        }]
    }

    fn validate_document(&self, document: ResolvedDocument) -> anyhow::Result<()> {
        ParsedDocument::<ComponentMetadata>::try_from(document)?;
        Ok(())
    }
}

#[allow(dead_code)]
pub struct ComponentsModel<'a, RT: Runtime> {
    pub tx: &'a mut Transaction<RT>,
}

#[allow(dead_code)]
impl<'a, RT: Runtime> ComponentsModel<'a, RT> {
    pub fn new(tx: &'a mut Transaction<RT>) -> Self {
        Self { tx }
    }

    pub async fn component_in_parent(
        &mut self,
        parent_and_name: Option<(InternalId, ComponentName)>,
    ) -> anyhow::Result<Option<ParsedDocument<ComponentMetadata>>> {
        let range = match parent_and_name {
            Some((parent, name)) => vec![
                IndexRangeExpression::Eq(PARENT_FIELD.clone(), maybe_val!(parent.to_string())),
                IndexRangeExpression::Eq(NAME_FIELD.clone(), maybe_val!(name.to_string())),
            ],
            None => vec![IndexRangeExpression::Eq(
                PARENT_FIELD.clone(),
                maybe_val!(null),
            )],
        };
        let mut query = ResolvedQuery::new(
            self.tx,
            Query::index_range(IndexRange {
                index_name: COMPONENTS_BY_PARENT_INDEX.clone(),
                range,
                order: Order::Asc,
            }),
        )?;
        let doc = query.next(self.tx, Some(1)).await?;
        doc.map(TryFrom::try_from).transpose()
    }

    pub async fn root_component(
        &mut self,
    ) -> anyhow::Result<Option<ParsedDocument<ComponentMetadata>>> {
        self.component_in_parent(None).await
    }

    pub async fn resolve_path(
        &mut self,
        path: ComponentPath,
    ) -> anyhow::Result<Option<ParsedDocument<ComponentMetadata>>> {
        let mut component_doc = match self.root_component().await? {
            Some(doc) => doc,
            None => return Ok(None),
        };
        for name in path.path.iter() {
            component_doc = match self
                .component_in_parent(Some((component_doc.id().internal_id(), name.clone())))
                .await?
            {
                Some(doc) => doc,
                None => return Ok(None),
            };
        }
        Ok(Some(component_doc))
    }

    pub async fn get_component_path(
        &mut self,
        mut component_id: ComponentId,
    ) -> anyhow::Result<ComponentPath> {
        let mut path = Vec::new();
        let component_table = self.tx.table_mapping().id(&COMPONENTS_TABLE)?;
        while let ComponentId::Child(internal_id) = component_id {
            let component_doc: ParsedDocument<ComponentMetadata> = self
                .tx
                .get(component_table.id(internal_id))
                .await?
                .with_context(|| format!("component {internal_id} missing"))?
                .try_into()?;
            component_id = match &component_doc.parent_and_name {
                Some((parent_internal_id, name)) => {
                    path.push(name.clone());
                    ComponentId::Child(*parent_internal_id)
                },
                None => ComponentId::Root,
            };
        }
        path.reverse();
        Ok(ComponentPath { path })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use common::{
        bootstrap_model::components::{
            definition::{
                ComponentDefinitionMetadata,
                ComponentInstantiation,
            },
            ComponentMetadata,
        },
        components::{
            ComponentDefinitionPath,
            ComponentId,
            ComponentPath,
        },
    };
    use keybroker::Identity;
    use runtime::testing::TestRuntime;

    use super::definition::COMPONENT_DEFINITIONS_TABLE;
    use crate::{
        bootstrap_model::components::{
            ComponentsModel,
            COMPONENTS_TABLE,
        },
        test_helpers::new_test_database,
        SystemMetadataModel,
    };

    #[convex_macro::test_runtime]
    async fn test_component_path(rt: TestRuntime) -> anyhow::Result<()> {
        let db = new_test_database(rt.clone()).await;
        let mut tx = db.begin(Identity::system()).await?;
        let child_definition_path: ComponentDefinitionPath = "../app/child".parse().unwrap();
        let child_definition_id = SystemMetadataModel::new(&mut tx)
            .insert(
                &COMPONENT_DEFINITIONS_TABLE,
                ComponentDefinitionMetadata {
                    name: "child".parse().unwrap(),
                    path: child_definition_path.clone(),
                    args: BTreeMap::new(),
                    child_components: Vec::new(),
                    exports: BTreeMap::new(),
                }
                .try_into()?,
            )
            .await?;
        let root_definition_id = SystemMetadataModel::new(&mut tx)
            .insert(
                &COMPONENT_DEFINITIONS_TABLE,
                ComponentDefinitionMetadata {
                    name: "root".parse().unwrap(),
                    path: "convex/app".parse().unwrap(),
                    child_components: vec![ComponentInstantiation {
                        name: "child_subcomponent".parse().unwrap(),
                        path: child_definition_path,
                        args: BTreeMap::new(),
                    }],
                    args: Default::default(),
                    exports: BTreeMap::new(),
                }
                .try_into()?,
            )
            .await?;
        let root_id = SystemMetadataModel::new(&mut tx)
            .insert(
                &COMPONENTS_TABLE,
                ComponentMetadata {
                    definition_id: root_definition_id.internal_id(),
                    parent_and_name: None,
                    args: Default::default(),
                }
                .try_into()?,
            )
            .await?;
        let child_id = SystemMetadataModel::new(&mut tx)
            .insert(
                &COMPONENTS_TABLE,
                ComponentMetadata {
                    definition_id: child_definition_id.internal_id(),
                    parent_and_name: Some((root_id.internal_id(), "subcomponent_child".parse()?)),
                    args: Default::default(),
                }
                .try_into()?,
            )
            .await?;
        let resolved_path = ComponentsModel::new(&mut tx)
            .resolve_path(ComponentPath {
                path: vec!["subcomponent_child".parse()?],
            })
            .await?;
        assert_eq!(resolved_path.unwrap().id(), child_id);
        let path = ComponentsModel::new(&mut tx)
            .get_component_path(ComponentId::Child(child_id.internal_id()))
            .await?;
        assert_eq!(
            path,
            ComponentPath {
                path: vec!["subcomponent_child".parse()?]
            }
        );
        Ok(())
    }
}
