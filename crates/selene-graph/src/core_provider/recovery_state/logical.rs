//! Temporary isolated data replay bridge. No legacy byte decoder is called here.
//! Catalog and complete type definitions are handled outside RecoveryState.

use super::RecoveryState;
use crate::{GraphTypeDef, SeleneGraph};
use selene_core::{
    Change,
    logical::{Budget, CodecError as E, CodecResult, Encoder, GraphDelta},
};
use std::sync::Arc;

pub(crate) fn logical_graph(
    original: Option<&SeleneGraph>,
    delta: &GraphDelta,
    bound: Option<Arc<GraphTypeDef>>,
    budget: &mut Budget,
) -> CodecResult<SeleneGraph> {
    let mut state = RecoveryState::new();
    let mut node_floor = 1;
    let mut edge_floor = 1;
    if let Some(graph) = original {
        node_floor = graph.meta.next_node_id;
        edge_floor = graph.meta.next_edge_id;
        let count = graph
            .node_store
            .len()
            .checked_add(graph.edge_store.len())
            .ok_or(E::Limit)?;
        budget.charge(count, count.checked_mul(1024).ok_or(E::Limit)?)?;
        for labels in graph.node_store.labels.iter() {
            budget.charge(labels.len(), labels.len().checked_mul(256).ok_or(E::Limit)?)?;
        }
        let unique_names: std::collections::BTreeSet<_> = bound
            .iter()
            .flat_map(|ty| {
                ty.node_types
                    .iter()
                    .flat_map(|n| &n.properties)
                    .chain(ty.edge_types.iter().flat_map(|e| &e.properties))
            })
            .filter(|property| property.unique)
            .map(|property| &property.name)
            .collect();
        for properties in graph
            .node_store
            .properties
            .iter()
            .chain(graph.edge_store.properties.iter())
        {
            for (name, value) in properties.iter() {
                if unique_names.contains(name) {
                    // UNIQUE owns canonical key buffers even for shared JSON,
                    // vectors, strings and bytes. Charge before its later scan.
                    let mut e = Encoder::counting(budget.clone());
                    e.value(value, 1)?;
                    *budget = e.budget;
                } else {
                    budget.stored_clone(value)?;
                }
            }
        }
        for id in graph.node_store.row_to_id.iter() {
            if let (Some(labels), Some(properties)) =
                (graph.node_labels(*id), graph.node_properties(*id))
            {
                state
                    .apply_change(&Change::NodeCreated {
                        id: *id,
                        labels: labels.clone(),
                        properties: properties.clone(),
                    })
                    .map_err(|_| E::Semantic)?;
            }
        }
        for id in graph.edge_store.row_to_id.iter() {
            if let Some(edge) = graph.edge_record(*id) {
                state
                    .apply_change(&Change::EdgeCreated {
                        id: *id,
                        directionality: edge.directionality,
                        label: edge.label,
                        source: edge.first,
                        target: edge.second,
                        properties: graph.edge_properties(*id).ok_or(E::Semantic)?.clone(),
                    })
                    .map_err(|_| E::Semantic)?;
            }
        }
    }
    for change in &delta.changes {
        match change {
            Change::NodeCreated { id, .. } => {
                if id.get() < node_floor {
                    return Err(E::Admission("reused node identity"));
                }
                node_floor = id.get().checked_add(1).ok_or(E::Limit)?;
            }
            Change::EdgeCreated { id, .. } => {
                if id.get() < edge_floor {
                    return Err(E::Admission("reused edge identity"));
                }
                edge_floor = id.get().checked_add(1).ok_or(E::Limit)?;
            }
            Change::SchemaChanged { .. } => return Err(E::Invalid("legacy schema event")),
            Change::NodesOfTypeTruncated { .. }
            | Change::EdgesOfTypeTruncated { .. }
            | Change::GraphReset {} => {
                budget.charge(
                    state
                        .nodes
                        .len()
                        .checked_add(state.edges.len())
                        .ok_or(E::Limit)?,
                    0,
                )?;
            }
            _ => {}
        }
        state.apply_change(change).map_err(|_| E::Semantic)?;
    }
    if delta.next_node_id < node_floor || delta.next_edge_id < edge_floor {
        return Err(E::Admission("element high water"));
    }
    state.schema_reset_to_open = false;
    charge_columns(state.nodes.len(), state.edges.len(), budget)?;
    let mut graph = state.into_graph(delta.id, bound).map_err(|_| E::Semantic)?;
    graph.meta.generation = delta.generation;
    graph.meta.next_node_id = delta.next_node_id;
    graph.meta.next_edge_id = delta.next_edge_id;
    crate::shared::rebuild_derived_state(&mut graph).map_err(|_| E::Semantic)?;
    if let Some(definition) = graph.meta.bound_type.as_deref() {
        crate::type_validator::validate_entity_state(&graph, definition)
            .map_err(|_| E::Semantic)?;
        for change in &delta.changes {
            if matches!(change, Change::NodeUpdated { labels_diff, .. } if !labels_diff.is_empty())
            {
                budget.charge(graph.edge_count(), 0)?;
            }
            crate::type_validator::validate_change(change, &graph, definition)
                .map_err(|_| E::Semantic)?;
        }
    }
    for id in graph.edge_store.row_to_id.iter() {
        if let Some(edge) = graph.edge_record(*id)
            && (!graph.is_node_alive(edge.first) || !graph.is_node_alive(edge.second))
        {
            return Err(E::Admission("missing edge endpoint"));
        }
    }
    Ok(graph)
}

fn charge_columns(nodes: usize, edges: usize, budget: &mut Budget) -> CodecResult<()> {
    use selene_core::{DbString, EdgeDirectionality, EdgeId, LabelSet, NodeId, PropertyMap};
    use std::mem::size_of;
    budget.charge(1, size_of::<SeleneGraph>() * 4 + 4096)?;
    let node_width = size_of::<LabelSet>() + size_of::<PropertyMap>() + size_of::<NodeId>();
    let edge_width = size_of::<DbString>()
        + size_of::<EdgeDirectionality>()
        + 2 * size_of::<NodeId>()
        + size_of::<PropertyMap>()
        + size_of::<EdgeId>();
    for (rows, width) in [(nodes, node_width), (edges, edge_width)] {
        // ChunkedVec reserves a complete 2048-element tail even for the first
        // row. Four copies cover freezing, maps/incidence and materialization.
        let slots = rows
            .div_ceil(crate::chunked_vec::CHUNK_SIZE)
            .checked_mul(crate::chunked_vec::CHUNK_SIZE)
            .ok_or(E::Limit)?;
        budget.charge(
            0,
            slots
                .checked_mul(width)
                .and_then(|n| n.checked_mul(4))
                .ok_or(E::Limit)?,
        )?;
    }
    Ok(())
}
