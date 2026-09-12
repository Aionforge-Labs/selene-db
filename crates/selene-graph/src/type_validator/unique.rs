//! Unique property validation helpers.

use std::collections::HashMap;

use selene_core::{
    Change, ComparisonMode, DbString, EdgeId, NodeId, PropertyMap, Value, ValueComparisonDomain,
};

use super::{EntityId, TypeViolation, validate_edge_state, validate_node_state};
use crate::graph::SeleneGraph;
use crate::graph_types::{GraphTypeDef, PropertyTypeDef};

pub(crate) fn graph_type_has_unique_properties(type_def: &GraphTypeDef) -> bool {
    type_def
        .node_types
        .iter()
        .any(|node_type| node_type.properties.iter().any(|property| property.unique))
        || type_def
            .edge_types
            .iter()
            .any(|edge_type| edge_type.properties.iter().any(|property| property.unique))
}

#[cfg(test)]
pub(crate) fn unique_property_check_required(
    changes: &[Change],
    graph: &SeleneGraph,
    type_def: &GraphTypeDef,
) -> Result<bool, TypeViolation> {
    Ok(!collect_changed_unique_candidates(changes, graph, type_def)?.is_empty())
}

pub(crate) fn validate_unique_property_changes(
    changes: &[Change],
    graph: &SeleneGraph,
    type_def: &GraphTypeDef,
) -> Result<(), TypeViolation> {
    let candidates = collect_changed_unique_candidates(changes, graph, type_def)?;
    if candidates.is_empty() {
        return Ok(());
    }
    let (candidate_by_key, mut impacted_domains) = index_unique_candidates(&candidates)?;
    validate_candidate_conflicts(graph, type_def, &candidate_by_key, &mut impacted_domains)
}

pub(crate) fn validate_unique_property_state(
    graph: &SeleneGraph,
    type_def: &GraphTypeDef,
) -> Result<(), TypeViolation> {
    if !graph_type_has_unique_properties(type_def) {
        return Ok(());
    }

    let mut seen = HashMap::new();
    let mut domains = HashMap::new();
    let nodes = graph
        .live_node_candidates()
        .expect("alive nodes have consistent typed stable-ID mappings");
    for id in nodes.iter() {
        let (node_type_index, _) = validate_node_state(id, graph, type_def)?;
        let node_type = &type_def.node_types[node_type_index as usize];
        let empty_props = PropertyMap::new();
        let properties = graph.node_properties(id).unwrap_or(&empty_props);
        record_unique_properties(
            EntityId::Node(id),
            UniqueEntityKind::Node,
            node_type.name.clone(),
            &node_type.properties,
            properties,
            &mut seen,
            &mut domains,
        )?;
    }
    let edges = graph
        .live_edge_candidates()
        .expect("alive edges have consistent typed stable-ID mappings");
    for id in edges.iter() {
        let (edge_type, _) = validate_edge_state(id, graph, type_def)?;
        let empty_props = PropertyMap::new();
        let properties = graph.edge_properties(id).unwrap_or(&empty_props);
        record_unique_properties(
            EntityId::Edge(id),
            UniqueEntityKind::Edge,
            edge_type.name.clone(),
            &edge_type.properties,
            properties,
            &mut seen,
            &mut domains,
        )?;
    }
    Ok(())
}

fn collect_changed_unique_candidates<'g>(
    changes: &[Change],
    graph: &'g SeleneGraph,
    type_def: &GraphTypeDef,
) -> Result<Vec<UniqueCandidate<'g>>, TypeViolation> {
    if !graph_type_has_unique_properties(type_def) {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    for change in changes {
        match change {
            Change::NodeCreated { id, .. } => collect_node_candidates(
                *id,
                graph,
                type_def,
                UniqueSelection::All,
                &mut candidates,
            )?,
            Change::NodeUpdated {
                id,
                labels_diff,
                properties_diff,
            } => {
                let selection = if labels_diff.is_empty() {
                    UniqueSelection::SetProperties(properties_diff)
                } else {
                    UniqueSelection::All
                };
                collect_node_candidates(*id, graph, type_def, selection, &mut candidates)?;
            }
            Change::EdgeCreated { id, .. } => collect_edge_candidates(
                *id,
                graph,
                type_def,
                UniqueSelection::All,
                &mut candidates,
            )?,
            Change::EdgeUpdated {
                id,
                properties_diff,
            } => collect_edge_candidates(
                *id,
                graph,
                type_def,
                UniqueSelection::SetProperties(properties_diff),
                &mut candidates,
            )?,
            Change::NodeLabelRemoved { id, .. } => collect_node_candidates(
                *id,
                graph,
                type_def,
                UniqueSelection::All,
                &mut candidates,
            )?,
            Change::NodeDeleted { .. }
            | Change::EdgeDeleted { .. }
            | Change::SchemaChanged { .. }
            | Change::NodePropertyRemoved { .. }
            | Change::EdgePropertyRemoved { .. }
            | Change::NodesOfTypeTruncated { .. }
            | Change::EdgesOfTypeTruncated { .. }
            | Change::GraphReset { .. } => {}
        }
    }
    Ok(candidates)
}

fn collect_node_candidates<'g>(
    id: NodeId,
    graph: &'g SeleneGraph,
    type_def: &GraphTypeDef,
    selection: UniqueSelection<'_>,
    candidates: &mut Vec<UniqueCandidate<'g>>,
) -> Result<(), TypeViolation> {
    if !graph.is_node_alive(id) {
        return Ok(());
    }
    let (node_type_index, _) = validate_node_state(id, graph, type_def)?;
    let node_type = &type_def.node_types[node_type_index as usize];
    let Some(properties) = graph.node_properties(id) else {
        return Ok(());
    };
    collect_entity_candidates(
        EntityId::Node(id),
        UniqueEntityKind::Node,
        node_type.name.clone(),
        &node_type.properties,
        properties,
        selection,
        candidates,
    )?;
    Ok(())
}

fn collect_edge_candidates<'g>(
    id: EdgeId,
    graph: &'g SeleneGraph,
    type_def: &GraphTypeDef,
    selection: UniqueSelection<'_>,
    candidates: &mut Vec<UniqueCandidate<'g>>,
) -> Result<(), TypeViolation> {
    if !graph.is_edge_alive(id) {
        return Ok(());
    }
    let (edge_type, _) = validate_edge_state(id, graph, type_def)?;
    let Some(properties) = graph.edge_properties(id) else {
        return Ok(());
    };
    collect_entity_candidates(
        EntityId::Edge(id),
        UniqueEntityKind::Edge,
        edge_type.name.clone(),
        &edge_type.properties,
        properties,
        selection,
        candidates,
    )?;
    Ok(())
}

fn collect_entity_candidates<'g>(
    entity_id: EntityId,
    entity_kind: UniqueEntityKind,
    declared_in: DbString,
    declarations: &[PropertyTypeDef],
    properties: &'g PropertyMap,
    selection: UniqueSelection<'_>,
    candidates: &mut Vec<UniqueCandidate<'g>>,
) -> Result<(), TypeViolation> {
    for declaration in declarations
        .iter()
        .filter(|declaration| declaration.unique && selection.includes(&declaration.name))
    {
        let Some(value) = properties.get(&declaration.name) else {
            continue;
        };
        if matches!(value, Value::Null) {
            continue;
        }
        candidates.push(UniqueCandidate {
            entity_id,
            value,
            key: UniquePropertyKey {
                entity_kind,
                declared_in: declared_in.clone(),
                property: declaration.name.clone(),
                value: UniqueValueKey::new(value, entity_id, &declaration.name, &declared_in)?,
            },
        });
    }
    Ok(())
}

type IndexedUniqueCandidates = (
    HashMap<UniquePropertyKey, EntityId>,
    HashMap<UniquePropertyDomain, ValueComparisonDomain>,
);

fn index_unique_candidates(
    candidates: &[UniqueCandidate<'_>],
) -> Result<IndexedUniqueCandidates, TypeViolation> {
    let mut candidate_by_key = HashMap::with_capacity(candidates.len());
    let mut impacted_domains = HashMap::new();
    for candidate in candidates {
        observe_unique_value(
            impacted_domains.entry(candidate.key.domain()).or_default(),
            candidate.entity_id,
            &candidate.key.property,
            &candidate.key.declared_in,
            candidate.value,
        )?;
        if let Some(conflicting_entity_id) =
            candidate_by_key.insert(candidate.key.clone(), candidate.entity_id)
        {
            if conflicting_entity_id == candidate.entity_id {
                continue;
            }
            return Err(TypeViolation::UniquePropertyDuplicate {
                entity_id: candidate.entity_id,
                conflicting_entity_id,
                property: candidate.key.property.clone(),
                declared_in: candidate.key.declared_in.clone(),
            });
        }
    }
    Ok((candidate_by_key, impacted_domains))
}

fn validate_candidate_conflicts(
    graph: &SeleneGraph,
    type_def: &GraphTypeDef,
    candidate_by_key: &HashMap<UniquePropertyKey, EntityId>,
    impacted_domains: &mut HashMap<UniquePropertyDomain, ValueComparisonDomain>,
) -> Result<(), TypeViolation> {
    let nodes = graph
        .live_node_candidates()
        .expect("alive nodes have consistent typed stable-ID mappings");
    for id in nodes.iter() {
        let (node_type_index, _) = validate_node_state(id, graph, type_def)?;
        let node_type = &type_def.node_types[node_type_index as usize];
        let empty_props = PropertyMap::new();
        let properties = graph.node_properties(id).unwrap_or(&empty_props);
        validate_entity_candidate_conflicts(
            EntityId::Node(id),
            UniqueEntityKind::Node,
            node_type.name.clone(),
            &node_type.properties,
            properties,
            candidate_by_key,
            impacted_domains,
        )?;
    }
    let edges = graph
        .live_edge_candidates()
        .expect("alive edges have consistent typed stable-ID mappings");
    for id in edges.iter() {
        let (edge_type, _) = validate_edge_state(id, graph, type_def)?;
        let empty_props = PropertyMap::new();
        let properties = graph.edge_properties(id).unwrap_or(&empty_props);
        validate_entity_candidate_conflicts(
            EntityId::Edge(id),
            UniqueEntityKind::Edge,
            edge_type.name.clone(),
            &edge_type.properties,
            properties,
            candidate_by_key,
            impacted_domains,
        )?;
    }
    Ok(())
}

fn validate_entity_candidate_conflicts(
    entity_id: EntityId,
    entity_kind: UniqueEntityKind,
    declared_in: DbString,
    declarations: &[PropertyTypeDef],
    properties: &PropertyMap,
    candidate_by_key: &HashMap<UniquePropertyKey, EntityId>,
    impacted_domains: &mut HashMap<UniquePropertyDomain, ValueComparisonDomain>,
) -> Result<(), TypeViolation> {
    for declaration in declarations.iter().filter(|property| property.unique) {
        let domain = UniquePropertyDomain {
            entity_kind,
            declared_in: declared_in.clone(),
            property: declaration.name.clone(),
        };
        let Some(observed) = impacted_domains.get_mut(&domain) else {
            continue;
        };
        let Some(value) = properties.get(&declaration.name) else {
            continue;
        };
        if matches!(value, Value::Null) {
            continue;
        }
        observe_unique_value(
            observed,
            entity_id,
            &domain.property,
            &domain.declared_in,
            value,
        )?;
        let key = UniquePropertyKey {
            value: UniqueValueKey::new(value, entity_id, &domain.property, &domain.declared_in)?,
            entity_kind: domain.entity_kind,
            declared_in: domain.declared_in.clone(),
            property: domain.property.clone(),
        };
        if let Some(candidate_entity_id) = candidate_by_key.get(&key).copied()
            && candidate_entity_id != entity_id
        {
            return Err(TypeViolation::UniquePropertyDuplicate {
                entity_id: candidate_entity_id,
                conflicting_entity_id: entity_id,
                property: key.property,
                declared_in: key.declared_in,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct UniqueCandidate<'g> {
    entity_id: EntityId,
    key: UniquePropertyKey,
    value: &'g Value,
}

#[derive(Clone, Copy)]
enum UniqueSelection<'a> {
    All,
    SetProperties(&'a selene_core::PropertyDiff),
}

impl UniqueSelection<'_> {
    fn includes(self, property: &DbString) -> bool {
        match self {
            Self::All => true,
            Self::SetProperties(diff) => diff
                .set
                .iter()
                .any(|(changed_property, _)| changed_property == property),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct UniquePropertyKey {
    entity_kind: UniqueEntityKind,
    declared_in: DbString,
    property: DbString,
    value: UniqueValueKey,
}

impl UniquePropertyKey {
    fn domain(&self) -> UniquePropertyDomain {
        UniquePropertyDomain {
            entity_kind: self.entity_kind,
            declared_in: self.declared_in.clone(),
            property: self.property.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct UniquePropertyDomain {
    entity_kind: UniqueEntityKind,
    declared_in: DbString,
    property: DbString,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum UniqueEntityKind {
    Node,
    Edge,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct UniqueValueKey(Vec<u8>);

impl UniqueValueKey {
    fn new(
        value: &Value,
        entity_id: EntityId,
        property: &DbString,
        declared_in: &DbString,
    ) -> Result<Self, TypeViolation> {
        let mut bytes = Vec::new();
        key::write(value, &mut bytes, 1).map_err(|source| {
            TypeViolation::UniquePropertyComparison {
                entity_id,
                property: property.clone(),
                declared_in: declared_in.clone(),
                source,
            }
        })?;
        Ok(Self(bytes))
    }
}

fn record_unique_properties(
    entity_id: EntityId,
    entity_kind: UniqueEntityKind,
    declared_in: DbString,
    declarations: &[PropertyTypeDef],
    properties: &PropertyMap,
    seen: &mut HashMap<UniquePropertyKey, EntityId>,
    domains: &mut HashMap<UniquePropertyDomain, ValueComparisonDomain>,
) -> Result<(), TypeViolation> {
    for declaration in declarations.iter().filter(|property| property.unique) {
        let Some(value) = properties.get(&declaration.name) else {
            continue;
        };
        if matches!(value, Value::Null) {
            continue;
        }
        let key = UniquePropertyKey {
            entity_kind,
            declared_in: declared_in.clone(),
            property: declaration.name.clone(),
            value: UniqueValueKey::new(value, entity_id, &declaration.name, &declared_in)?,
        };
        observe_unique_value(
            domains.entry(key.domain()).or_default(),
            entity_id,
            &key.property,
            &key.declared_in,
            value,
        )?;
        if let Some(conflicting_entity_id) = seen.get(&key).copied() {
            return Err(TypeViolation::UniquePropertyDuplicate {
                entity_id,
                conflicting_entity_id,
                property: declaration.name.clone(),
                declared_in,
            });
        }
        seen.insert(key, entity_id);
    }
    Ok(())
}

fn observe_unique_value(
    domain: &mut ValueComparisonDomain,
    entity_id: EntityId,
    property: &DbString,
    declared_in: &DbString,
    value: &Value,
) -> Result<(), TypeViolation> {
    domain
        .observe(value, ComparisonMode::Distinctness)
        .map_err(|source| TypeViolation::UniquePropertyComparison {
            entity_id,
            property: property.clone(),
            declared_in: declared_in.clone(),
            source,
        })
}

mod key;
