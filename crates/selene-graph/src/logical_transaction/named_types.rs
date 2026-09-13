//! Named catalog constraints remain authoritative independently of instance schema.

use super::{LogicalTransaction, ReplayState, schema};
use crate::{GraphTypeDef, SeleneGraph};
use selene_catalog::{
    CatalogDescriptor, CatalogLogicalChange, CatalogObjectId, CatalogPayload, CatalogSnapshot,
    GraphTypeId,
};
use selene_core::{
    Change, GraphId,
    logical::{Budget, CodecError as E, CodecResult, Encoder, GraphDefinition},
};
use std::{collections::BTreeMap, sync::Arc};

pub(super) fn require_changed_bodies(
    tx: &LogicalTransaction,
    budget: &mut Budget,
) -> CodecResult<()> {
    budget.charge(tx.catalog.changes.len(), 0)?;
    for change in &tx.catalog.changes {
        let id = match change {
            CatalogLogicalChange::Created(d)
            | CatalogLogicalChange::Replaced { descriptor: d, .. } => d.id(),
            CatalogLogicalChange::Dropped { id, .. } => *id,
        };
        if let CatalogObjectId::GraphType(id) = id
            && tx
                .graph_types
                .binary_search_by_key(&id, |ty| ty.id)
                .is_err()
        {
            return Err(E::Invalid("type descriptor change without payload"));
        }
    }
    Ok(())
}

pub(super) fn check_name(def: &GraphDefinition, descriptor: &CatalogDescriptor) -> CodecResult<()> {
    // The facade constructs runtime names from the descriptor's display spelling.
    // Empty strings and shape-equivalent bodies from another named ID do not bind.
    if def.name.as_str() != descriptor.name().display() {
        return Err(E::Invalid("named type body name"));
    }
    Ok(())
}

pub(super) fn materialize(
    def: &GraphDefinition,
    budget: &mut Budget,
) -> CodecResult<Arc<GraphTypeDef>> {
    // Retained definitions were not decoded in this transaction. Account for
    // their recursive fields and owner conversion before materializing them.
    let mut e = Encoder::counting(budget.clone());
    e.budget.charge(1, 512)?;
    e.graph_definition(def)?;
    *budget = e.budget;
    schema::materialize(def).map(Arc::new)
}

pub(super) fn validate_bindings(
    candidate: &mut ReplayState,
    previous: Option<&ReplayState>,
    old: &CatalogSnapshot,
    catalog: &CatalogSnapshot,
    tx: &LogicalTransaction,
    runtime: &mut BTreeMap<GraphTypeId, Arc<GraphTypeDef>>,
    budget: &mut Budget,
) -> CodecResult<()> {
    for descriptor in catalog.descriptors() {
        let CatalogPayload::Graph {
            graph_type: Some(type_id),
        } = descriptor.payload()
        else {
            continue;
        };
        let type_descriptor = catalog
            .descriptor(CatalogObjectId::GraphType(*type_id))
            .ok_or(E::Admission("missing constraining type"))?;
        // Same-schema binding is required by the facade's resolve_binding owner.
        if descriptor.parent() != type_descriptor.parent() {
            return Err(E::Invalid("cross-schema named type binding"));
        }
        let def = candidate
            .graph_types
            .get(type_id)
            .ok_or(E::Admission("missing constraining type"))?;
        check_name(def, type_descriptor)?;
        let graph_id = GraphId::new(descriptor.id().get());
        let graph = candidate
            .graphs
            .get(&graph_id)
            .ok_or(E::Invalid("missing named graph payload"))?;
        let instance = graph
            .meta
            .bound_type
            .as_ref()
            .ok_or(E::Invalid("named graph missing instance definition"))?;
        if instance.name != def.name {
            return Err(E::Invalid("named instance type name"));
        }
        let delta = tx
            .graphs
            .binary_search_by_key(&graph_id, |graph| graph.id)
            .ok()
            .map(|i| &tx.graphs[i]);
        let type_changed = tx
            .graph_types
            .binary_search_by_key(type_id, |ty| ty.id)
            .is_ok();
        if delta.is_none() && !type_changed && old.descriptor(descriptor.id()) == Some(descriptor) {
            // This immutable graph/type/binding triple was already validated.
            continue;
        }
        let named = match runtime.entry(*type_id) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(materialize(def, budget)?)
            }
        };
        account_validation(graph, named, budget)?;
        for change in delta.into_iter().flat_map(|delta| &delta.changes) {
            budget.charge(1, 0)?;
            let label_change = match change {
                Change::NodeUpdated {
                    id, labels_diff, ..
                } if !labels_diff.is_empty() => Some(*id),
                Change::NodeLabelRemoved { id, .. } => Some(*id),
                _ => None,
            };
            if label_change.is_some_and(|id| graph.is_node_alive(id)) {
                budget.charge(graph.edge_count(), 0)?;
            }
            // Also preserves named immutability. The owner intentionally skips
            // entities deleted later in this atomic unit; do not require liveness here.
            crate::type_validator::validate_change(change, graph, named)
                .map_err(|_| E::Invalid("named graph type operation"))?;
        }
        let graph = Arc::make_mut(candidate.graphs.get_mut(&graph_id).expect("named graph"));
        graph
            .admit_named_constraints(
                previous
                    .and_then(|p| p.graphs.get(&graph_id))
                    .map(AsRef::as_ref),
                named.clone(),
                delta.map_or(&[], |delta| delta.changes.as_slice()),
            )
            .map_err(|_| E::Invalid("named graph type conformance"))?;
    }
    Ok(())
}

fn account_validation(
    graph: &SeleneGraph,
    named: &GraphTypeDef,
    budget: &mut Budget,
) -> CodecResult<()> {
    let rows = graph
        .node_store
        .len()
        .checked_add(graph.edge_store.len())
        .ok_or(E::Limit)?;
    let type_work = named
        .node_types
        .len()
        .checked_add(named.edge_types.len())
        .and_then(|n| n.checked_add(1))
        .ok_or(E::Limit)?;
    // Candidate-ID scans, constraint/domain/key buffers and repeated type lookups
    // are work/allocation even when graph primary data is retained through Arc.
    let mut e = Encoder::counting(budget.clone());
    e.budget.charge(
        rows.checked_mul(type_work).ok_or(E::Limit)?,
        rows.checked_mul(512).ok_or(E::Limit)?,
    )?;
    for labels in graph.node_store.labels.iter() {
        e.labels(labels)?;
    }
    for properties in graph
        .node_store
        .properties
        .iter()
        .chain(graph.edge_store.properties.iter())
    {
        e.count(properties.len())?;
        for (name, value) in properties.iter() {
            e.text(name.as_str())?;
            e.value(value, 1)?;
        }
    }
    *budget = e.budget;
    Ok(())
}
