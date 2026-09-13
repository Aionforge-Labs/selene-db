//! Complete, snapshot-owned constraint indexes. All arities use the same tuple
//! and final-state delta implementation. Index state is never a WAL payload.

use immutable_chunkmap::map::MapM;
use selene_catalog::{ConstraintDeclaration, ConstraintKind, ElementKind};
use selene_core::{Change, DbString, PropertyMap, Value};
use std::collections::BTreeSet;

use super::{EntityId, TypeViolation, validate_edge_state, validate_node_state};
use crate::{GraphTypeDef, SeleneGraph};

mod domain;
#[cfg(test)]
mod incremental_tests;
mod key;

#[derive(Clone, Debug)]
struct RuleIndex {
    rule: ConstraintDeclaration,
    properties: Vec<DbString>,
    entries: MapM<Vec<Vec<u8>>, EntityId>,
    domains: domain::Domains,
}

/// Private complete backing, shared copy-on-write with the primary snapshot.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConstraintIndexes {
    indexes: Vec<RuleIndex>,
    #[cfg(test)]
    pub(crate) visited: usize,
}

impl ConstraintIndexes {
    pub(crate) fn matches_rules(&self, rules: &[ConstraintDeclaration]) -> bool {
        self.indexes.len() == rules.len()
            && self
                .indexes
                .iter()
                .zip(rules)
                .all(|(index, rule)| &index.rule == rule)
    }
    pub(crate) fn build(
        graph: &SeleneGraph,
        rules: Vec<ConstraintDeclaration>,
    ) -> Result<Self, TypeViolation> {
        let mut result = Self {
            indexes: rules
                .into_iter()
                .map(|rule| RuleIndex {
                    properties: rule
                        .target
                        .properties
                        .iter()
                        .map(|p| selene_core::db_string(p).expect("admitted constraint property"))
                        .collect(),
                    rule,
                    entries: MapM::new(),
                    domains: domain::Domains::default(),
                })
                .collect(),
            #[cfg(test)]
            visited: 0,
        };
        if result.indexes.is_empty() {
            return Ok(result);
        }
        for id in graph
            .live_node_candidates()
            .expect("validated node mappings")
            .iter()
        {
            result.change(graph, EntityId::Node(id), true)?;
        }
        for id in graph
            .live_edge_candidates()
            .expect("validated edge mappings")
            .iter()
        {
            result.change(graph, EntityId::Edge(id), true)?;
        }
        Ok(result)
    }

    pub(crate) fn apply(
        &self,
        changes: &[Change],
        before: &SeleneGraph,
        after: &SeleneGraph,
    ) -> Result<Self, TypeViolation> {
        let mut result = self.clone();
        #[cfg(test)]
        {
            result.visited = 0;
        }
        if result.indexes.is_empty() {
            return Ok(result);
        }
        let mut affected = BTreeSet::new();
        for change in changes {
            match change {
                Change::NodeCreated { id, .. }
                | Change::NodeUpdated { id, .. }
                | Change::NodeDeleted { id, .. }
                | Change::NodePropertyRemoved { id, .. }
                | Change::NodeLabelRemoved { id, .. } => {
                    affected.insert(EntityId::Node(*id));
                }
                Change::EdgeCreated { id, .. }
                | Change::EdgeUpdated { id, .. }
                | Change::EdgeDeleted { id, .. }
                | Change::EdgePropertyRemoved { id, .. } => {
                    affected.insert(EntityId::Edge(*id));
                }
                // WriteTxn supplies the existing truncate tombstone expansion.
                Change::NodesOfTypeTruncated { .. }
                | Change::EdgesOfTypeTruncated { .. }
                | Change::GraphReset { .. }
                | Change::SchemaChanged { .. } => {}
            }
        }
        // A node label transition can move an incident edge into another declaring
        // type even though the edge itself has no property delta.
        for change in changes {
            let id = match change {
                Change::NodeUpdated {
                    id, labels_diff, ..
                } if !labels_diff.is_empty() => *id,
                Change::NodeLabelRemoved { id, .. } => *id,
                _ => continue,
            };
            for graph in [before, after] {
                for adjacency in [
                    graph.outgoing_edges(id),
                    graph.incoming_edges(id),
                    graph.undirected_edges(id),
                ]
                .into_iter()
                .flatten()
                {
                    for edge in adjacency.iter() {
                        affected.insert(EntityId::Edge(edge.edge_id));
                    }
                }
            }
        }
        // Remove *all* old tuples before probing any final tuple: swaps and reuse
        // are independent of statement/batch iteration order.
        for id in &affected {
            result.change(before, *id, false)?;
        }
        for id in &affected {
            result.change(after, *id, true)?;
        }
        Ok(result)
    }

    fn change(
        &mut self,
        graph: &SeleneGraph,
        id: EntityId,
        insert: bool,
    ) -> Result<(), TypeViolation> {
        let Some(type_def) = graph.meta.bound_type.as_deref() else {
            return Ok(());
        };
        let (element, declared_in, properties) = match id {
            EntityId::Node(id) if graph.is_node_alive(id) => {
                let (index, _) = validate_node_state(id, graph, type_def)?;
                (
                    ElementKind::Node,
                    &type_def.node_types[index as usize].name,
                    graph.node_properties(id),
                )
            }
            EntityId::Edge(id) if graph.is_edge_alive(id) => {
                let (ty, _) = validate_edge_state(id, graph, type_def)?;
                (ElementKind::Edge, &ty.name, graph.edge_properties(id))
            }
            _ => return Ok(()),
        };
        #[cfg(test)]
        {
            self.visited += 1;
        }
        let empty = PropertyMap::new();
        let properties = properties.unwrap_or(&empty);
        for index in &mut self.indexes {
            if index.rule.target.element != element
                || index.rule.declaring_type != declared_in.as_str()
            {
                continue;
            }
            index.change(properties, id, declared_in, insert)?;
        }
        Ok(())
    }
}

impl RuleIndex {
    fn change(
        &mut self,
        properties: &PropertyMap,
        id: EntityId,
        declared_in: &DbString,
        insert: bool,
    ) -> Result<(), TypeViolation> {
        let mut values = Vec::with_capacity(self.properties.len());
        for property in &self.properties {
            match properties.get(property) {
                Some(value) if !matches!(value, Value::Null) => values.push(value),
                _ if self.rule.kind == ConstraintKind::Key => {
                    return Err(TypeViolation::MissingRequiredProperty {
                        entity_id: id,
                        property: property.clone(),
                        declared_in: declared_in.clone(),
                    });
                }
                _ => return Ok(()),
            }
        }
        let comparison = |source| TypeViolation::UniquePropertyComparison {
            entity_id: id,
            property: self.properties[0].clone(),
            declared_in: declared_in.clone(),
            source,
        };
        let tuple = values
            .iter()
            .map(|value| {
                let mut bytes = Vec::new();
                key::write(value, &mut bytes, 1).map_err(comparison)?;
                Ok(bytes)
            })
            .collect::<Result<Vec<_>, TypeViolation>>()?;
        self.domains.change(&values, insert).map_err(comparison)?;
        if insert {
            if let Some(conflicting_entity_id) = self.entries.get(&tuple).copied() {
                return Err(TypeViolation::UniquePropertyDuplicate {
                    entity_id: id,
                    conflicting_entity_id,
                    property: self.properties[0].clone(),
                    declared_in: declared_in.clone(),
                });
            }
            self.entries.insert_cow(tuple, id);
        } else {
            self.entries.remove_cow(&tuple);
        }
        Ok(())
    }
}

pub(crate) fn validate_unique_property_state(
    graph: &SeleneGraph,
    type_def: &GraphTypeDef,
) -> Result<(), TypeViolation> {
    let mut graph = graph.clone();
    graph.meta.bound_type = Some(std::sync::Arc::new(type_def.clone()));
    ConstraintIndexes::build(&graph, graph.constraint_rules()).map(|_| ())
}

#[cfg(test)]
pub(crate) fn unique_property_check_required(
    changes: &[Change],
    graph: &SeleneGraph,
    type_def: &GraphTypeDef,
) -> Result<bool, TypeViolation> {
    // Kept only as a legacy test observation, not an enforcement implementation.
    for change in changes {
        let (id, diff) = match change {
            Change::NodeUpdated {
                id,
                labels_diff,
                properties_diff,
            } if labels_diff.is_empty() => (*id, Some(properties_diff)),
            Change::NodeCreated { id, .. }
            | Change::NodeUpdated { id, .. }
            | Change::NodeLabelRemoved { id, .. } => (*id, None),
            _ => continue,
        };
        let (index, _) = validate_node_state(id, graph, type_def)?;
        if type_def.node_types[index as usize]
            .properties
            .iter()
            .any(|p| p.unique && diff.is_none_or(|d| d.set.iter().any(|(name, _)| name == &p.name)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
