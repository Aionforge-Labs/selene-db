//! Normalize trusted, unpublished graph schema changes into the outer catalog.

use selene_catalog::{
    CatalogDescriptor, CatalogName, CatalogObjectId, CatalogParent, CatalogPayload,
    CatalogTransaction, CreationMetadata, DeclarationMetadata, DeclarationState, ElementKind,
    GraphId, IndexConfiguration, IndexDeclaration, IndexFamily, IndexId, PropertyTarget,
    generated_index_name,
};
use selene_core::{Change, DbString, SchemaChange};
use selene_graph::write_txn::PreparedGraphCommit;

use crate::{Error, Result, catalog_snapshot::next_id, transaction::DatabaseDraft};

impl DatabaseDraft {
    pub(crate) fn stage_registrations(
        &mut self,
        owner: GraphId,
        prepared: &PreparedGraphCommit,
    ) -> Result<()> {
        if !prepared.schema_changed() {
            return Ok(());
        }
        let mut transaction =
            CatalogTransaction::new(&self.catalog).map_err(Error::from_catalog_invariant)?;
        let mut high_water = self.high_water;
        // Start with actual runtime bindings, not every declaration that happens
        // to mention the same target. Track trusted creates/drops in order so a
        // create followed by drop inside one prepared change list also has an ID.
        let mut bound_indexes = self
            .selected_graph()?
            .catalog_bound_indexes()
            .filter(|descriptor| !matches!(descriptor.payload(), CatalogPayload::Index(index) if matches!(index.configuration, IndexConfiguration::Constraint { .. } | IndexConfiguration::Expression { .. })))
            .map(|descriptor| {
                let (CatalogObjectId::Index(id), CatalogPayload::Index(index)) =
                    (descriptor.id(), descriptor.payload())
                else {
                    return Err(Error::catalog_invariant("non-index runtime binding"));
                };
                Ok((index_key(&index.target, index.configuration.family()), id))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
        // This is the trusted mutation funnel's logical change list, not a post-store scan.
        for change in prepared.changes() {
            let Change::SchemaChanged { change, .. } = change else {
                continue;
            };
            let Some(event) = index_event(change) else {
                continue;
            };
            match event {
                IndexEvent::Create { name, declaration } => {
                    let raw = next_id(high_water.index, "index")?;
                    let id = IndexId::new(raw).map_err(Error::from_catalog_invariant)?;
                    if bound_indexes
                        .insert(
                            index_key(&declaration.target, declaration.configuration.family()),
                            id,
                        )
                        .is_some()
                    {
                        return Err(Error::catalog_invariant(
                            "trusted index creation replaced a binding without a drop",
                        ));
                    }
                    let name = CatalogName::delimited(&name).map_err(Error::from_catalog_name)?;
                    let descriptor = CatalogDescriptor::index(
                        id,
                        name,
                        CatalogParent::Graph(owner),
                        transaction.generation(),
                        CreationMetadata::new(transaction.generation(), None),
                        declaration,
                    )
                    .map_err(Error::from_catalog_invariant)?;
                    transaction
                        .insert(descriptor)
                        .map_err(Error::from_catalog_invariant)?;
                    high_water.index = raw;
                }
                IndexEvent::Drop { target, family } => {
                    let id = bound_indexes
                        .remove(&index_key(&target, family))
                        .ok_or_else(|| {
                            Error::catalog_invariant(
                                "trusted index drop has no runtime-bound declaration",
                            )
                        })?;
                    transaction
                        .remove(CatalogObjectId::Index(id))
                        .ok_or_else(|| {
                            Error::catalog_invariant("runtime-bound index declaration is missing")
                        })?;
                }
            }
        }
        stage_constraints(
            &mut transaction,
            owner,
            prepared.snapshot(),
            &mut high_water,
        )?;
        self.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
        self.high_water = high_water;
        Ok(())
    }
}

pub(crate) fn stage_constraints(
    transaction: &mut CatalogTransaction,
    owner: GraphId,
    graph: &selene_graph::SeleneGraph,
    high_water: &mut crate::database::HighWaterMarks,
) -> Result<()> {
    let mut rules = graph.unique_declarations();
    let existing: Vec<_> = transaction
        .descriptors()
        .filter(|descriptor| descriptor.parent() == CatalogParent::Graph(owner))
        .filter_map(|descriptor| match descriptor.payload() {
            CatalogPayload::Constraint(rule) if rule.metadata.state == DeclarationState::Ready => {
                Some((descriptor.id(), rule.clone()))
            }
            _ => None,
        })
        .collect();
    for (id, rule) in existing {
        if let Some(position) = rules.iter().position(|expected| {
            expected.target == rule.target
                && expected.kind == rule.kind
                && expected.declaring_type == rule.declaring_type
        }) {
            rules.swap_remove(position);
        } else {
            // Named catalog constraints survive unrelated schema changes. A
            // dropped/changed target must be explicitly removed with its backing.
            if graph.validate_constraint_target(&rule).is_ok()
                && rule.kind != selene_catalog::ConstraintKind::Unique
            {
                continue;
            }
            transaction.remove(id);
            if let Some(backing) = rule.backing_index {
                transaction.remove(CatalogObjectId::Index(backing));
            }
        }
    }
    rules.sort_by(|a, b| (&a.target, &a.declaring_type).cmp(&(&b.target, &b.declaring_type)));
    for mut rule in rules {
        let raw = next_id(high_water.constraint, "constraint")?;
        let id = selene_catalog::ConstraintId::new(raw).map_err(Error::from_catalog_invariant)?;
        let name = format!(
            "unique:{:?}:{}:{}:{}:{}",
            rule.target.element,
            rule.declaring_type.len(),
            rule.declaring_type,
            rule.target.properties[0].len(),
            rule.target.properties[0]
        );
        add_constraint_backing(transaction, owner, &name, &mut rule, high_water)?;
        let descriptor = CatalogDescriptor::constraint(
            id,
            CatalogName::delimited(&name).map_err(Error::from_catalog_name)?,
            CatalogParent::Graph(owner),
            transaction.generation(),
            CreationMetadata::new(transaction.generation(), None),
            rule,
        )
        .map_err(Error::from_catalog_invariant)?;
        transaction
            .insert(descriptor)
            .map_err(Error::from_catalog_invariant)?;
        high_water.constraint = raw;
    }
    Ok(())
}

pub(crate) fn add_constraint_backing(
    transaction: &mut CatalogTransaction,
    owner: GraphId,
    name: &str,
    rule: &mut selene_catalog::ConstraintDeclaration,
    high_water: &mut crate::database::HighWaterMarks,
) -> Result<()> {
    let raw = next_id(high_water.index, "constraint backing")?;
    let id = IndexId::new(raw).map_err(Error::from_catalog_invariant)?;
    transaction
        .insert(
            CatalogDescriptor::index(
                id,
                CatalogName::delimited(format!("constraint-backing:{name}"))
                    .map_err(Error::from_catalog_name)?,
                CatalogParent::Graph(owner),
                transaction.generation(),
                CreationMetadata::new(transaction.generation(), None),
                IndexDeclaration {
                    metadata: DeclarationMetadata::new(DeclarationState::Ready),
                    target: rule.target.clone(),
                    configuration: IndexConfiguration::Constraint {
                        declaring_type: rule.declaring_type.clone(),
                    },
                },
            )
            .map_err(Error::from_catalog_invariant)?,
        )
        .map_err(Error::from_catalog_invariant)?;
    rule.backing_index = Some(id);
    rule.metadata
        .dependencies
        .push(selene_catalog::DeclarationDependency {
            id: CatalogObjectId::Index(id),
            generation: transaction.generation(),
        });
    high_water.index = raw;
    Ok(())
}

fn index_key(
    target: &PropertyTarget,
    family: IndexFamily,
) -> (ElementKind, IndexFamily, String, Vec<String>) {
    let mut properties = target.properties.clone();
    properties.sort();
    (target.element, family, target.label.clone(), properties)
}

enum IndexEvent {
    Create {
        name: String,
        declaration: IndexDeclaration,
    },
    Drop {
        target: PropertyTarget,
        family: IndexFamily,
    },
}

fn target(element: ElementKind, label: &DbString, properties: &[DbString]) -> PropertyTarget {
    PropertyTarget {
        element,
        label: label.to_string(),
        properties: properties.iter().map(ToString::to_string).collect(),
    }
}

fn create(
    target: PropertyTarget,
    configuration: IndexConfiguration,
    name: Option<&DbString>,
) -> IndexEvent {
    let name = name.map(ToString::to_string).unwrap_or_else(|| {
        generated_index_name(
            configuration.family(),
            &target.label,
            target.properties.iter().map(String::as_str),
        )
    });
    IndexEvent::Create {
        name,
        declaration: IndexDeclaration {
            metadata: DeclarationMetadata::new(DeclarationState::Ready),
            target,
            configuration,
        },
    }
}

fn index_event(change: &SchemaChange) -> Option<IndexEvent> {
    use ElementKind::{Edge, Node};
    use SchemaChange::*;
    Some(match change {
        PropertyIndexCreated {
            label,
            property,
            kind,
        } => create(
            target(Node, label, std::slice::from_ref(property)),
            IndexConfiguration::Property(vec![*kind]),
            None,
        ),
        PropertyIndexCreatedNamed {
            label,
            property,
            kind,
            name,
        } => create(
            target(Node, label, std::slice::from_ref(property)),
            IndexConfiguration::Property(vec![*kind]),
            name.as_ref(),
        ),
        EdgePropertyIndexCreated {
            label,
            property,
            kind,
            name,
        } => create(
            target(Edge, label, std::slice::from_ref(property)),
            IndexConfiguration::Property(vec![*kind]),
            name.as_ref(),
        ),
        CompositePropertyIndexCreated {
            label,
            properties,
            kinds,
            name,
        } => create(
            target(Node, label, properties),
            IndexConfiguration::Property(kinds.to_vec()),
            name.as_ref(),
        ),
        VectorIndexCreated {
            label,
            property,
            kind,
            dimension,
            name,
            hnsw_config,
            ivf_config,
        } => create(
            target(Node, label, std::slice::from_ref(property)),
            IndexConfiguration::Vector {
                kind: *kind,
                dimension: *dimension,
                hnsw: *hnsw_config,
                ivf: *ivf_config,
            },
            name.as_ref(),
        ),
        TextIndexCreated {
            label,
            property,
            name,
        } => create(
            target(Node, label, std::slice::from_ref(property)),
            IndexConfiguration::Text,
            name.as_ref(),
        ),
        PropertyIndexDropped { label, property } => IndexEvent::Drop {
            target: target(Node, label, std::slice::from_ref(property)),
            family: IndexFamily::Property,
        },
        EdgePropertyIndexDropped { label, property } => IndexEvent::Drop {
            target: target(Edge, label, std::slice::from_ref(property)),
            family: IndexFamily::Property,
        },
        CompositePropertyIndexDropped { label, properties } => IndexEvent::Drop {
            target: target(Node, label, properties),
            family: IndexFamily::Property,
        },
        VectorIndexDropped { label, property } => IndexEvent::Drop {
            target: target(Node, label, std::slice::from_ref(property)),
            family: IndexFamily::Vector,
        },
        TextIndexDropped { label, property } => IndexEvent::Drop {
            target: target(Node, label, std::slice::from_ref(property)),
            family: IndexFamily::Text,
        },
        _ => return None,
    })
}
