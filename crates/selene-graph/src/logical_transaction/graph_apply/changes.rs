//! Apply logical data changes directly to isolated native primary columns.

use crate::{
    SeleneGraph,
    store::{EdgeRow, NodeRow},
};
use selene_core::{
    Change, EdgeId, LabelSet, NodeId, PropertyDiff, PropertyMap,
    logical::{CodecError as E, CodecResult},
};
use std::sync::Arc;

pub(super) fn apply(graph: &mut SeleneGraph, change: &Change) -> CodecResult<()> {
    change.validate_stored_values().map_err(|_| E::Semantic)?;
    match change {
        Change::NodeCreated {
            id,
            labels,
            properties,
        } => {
            if *id == NodeId::TOMBSTONE || graph.node_rows.get(id).is_some() {
                return Err(E::Admission("reused node identity"));
            }
            let row = NodeRow::new(row_number(graph.node_store.len())?);
            graph.node_store.labels.push(labels.clone());
            graph.node_store.properties.push(properties.clone());
            graph.node_store.row_to_id.push(*id);
            graph.node_rows.insert_cow(*id, row);
            Arc::make_mut(&mut graph.node_store.alive).insert(row.get());
        }
        Change::EdgeCreated {
            id,
            directionality,
            label,
            source,
            target,
            properties,
        } => {
            node(graph, *source)?;
            node(graph, *target)?;
            if *id == EdgeId::TOMBSTONE
                || graph.edge_rows.get(id).is_some()
                || directionality.canonical_endpoints(*source, *target) != (*source, *target)
            {
                return Err(E::Admission("edge identity or endpoints"));
            }
            let row = EdgeRow::new(row_number(graph.edge_store.len())?);
            graph.edge_store.label.push(label.clone());
            graph.edge_store.directionality.push(*directionality);
            graph.edge_store.source.push(*source);
            graph.edge_store.target.push(*target);
            graph.edge_store.properties.push(properties.clone());
            graph.edge_store.row_to_id.push(*id);
            graph.edge_rows.insert_cow(*id, row);
            Arc::make_mut(&mut graph.edge_store.alive).insert(row.get());
        }
        Change::NodeUpdated {
            id,
            labels_diff,
            properties_diff,
        } => {
            let row = node(graph, *id)?;
            let mut labels = graph
                .node_store
                .labels
                .get(row.index())
                .ok_or(E::Semantic)?
                .clone();
            for label in &labels_diff.added {
                labels.insert(label.clone());
            }
            for label in &labels_diff.removed {
                labels.remove(label);
            }
            let properties = patch(
                graph.node_store.properties.get(row.index()),
                properties_diff,
            )?;
            graph.node_store.labels.set(row.index(), labels);
            graph.node_store.properties.set(row.index(), properties);
        }
        Change::EdgeUpdated {
            id,
            properties_diff,
        } => {
            let row = edge(graph, *id)?;
            let properties = patch(
                graph.edge_store.properties.get(row.index()),
                properties_diff,
            )?;
            graph.edge_store.properties.set(row.index(), properties);
        }
        Change::NodeDeleted { id } => {
            let row = node(graph, *id)?;
            clear_node(graph, row);
        }
        Change::EdgeDeleted { id } => {
            let row = edge(graph, *id)?;
            clear_edge(graph, row)?;
        }
        Change::NodePropertyRemoved { id, property } => {
            let row = node(graph, *id)?;
            let mut properties = graph
                .node_store
                .properties
                .get(row.index())
                .ok_or(E::Semantic)?
                .clone();
            properties.remove(property);
            graph.node_store.properties.set(row.index(), properties);
        }
        Change::EdgePropertyRemoved { id, property } => {
            let row = edge(graph, *id)?;
            let mut properties = graph
                .edge_store
                .properties
                .get(row.index())
                .ok_or(E::Semantic)?
                .clone();
            properties.remove(property);
            graph.edge_store.properties.set(row.index(), properties);
        }
        Change::NodeLabelRemoved { id, label } => {
            let row = node(graph, *id)?;
            let mut labels = graph
                .node_store
                .labels
                .get(row.index())
                .ok_or(E::Semantic)?
                .clone();
            labels.remove(label);
            graph.node_store.labels.set(row.index(), labels);
        }
        Change::NodesOfTypeTruncated { label } => {
            let mut truncated = std::collections::BTreeSet::new();
            for index in 0..graph.node_store.len() {
                let row = NodeRow::new(row_number(index)?);
                if graph.node_store.is_alive_row(row)
                    && graph
                        .node_store
                        .labels
                        .get(index)
                        .is_some_and(|labels| labels.contains(label))
                {
                    truncated.insert(*graph.node_store.row_to_id.get(index).ok_or(E::Semantic)?);
                    clear_node(graph, row);
                }
            }
            for index in 0..graph.edge_store.len() {
                let row = EdgeRow::new(row_number(index)?);
                if graph.edge_store.is_alive_row(row) {
                    let source = *graph.edge_store.source.get(index).ok_or(E::Semantic)?;
                    let target = *graph.edge_store.target.get(index).ok_or(E::Semantic)?;
                    if truncated.contains(&source) || truncated.contains(&target) {
                        clear_edge(graph, row)?;
                    }
                }
            }
        }
        Change::EdgesOfTypeTruncated { label } => {
            for index in 0..graph.edge_store.len() {
                let row = EdgeRow::new(row_number(index)?);
                if graph.edge_store.is_alive_row(row)
                    && graph.edge_store.label.get(index) == Some(label)
                {
                    clear_edge(graph, row)?;
                }
            }
        }
        Change::GraphReset {} => {
            for index in 0..graph.node_store.len() {
                clear_node(graph, NodeRow::new(row_number(index)?));
            }
            for index in 0..graph.edge_store.len() {
                clear_edge(graph, EdgeRow::new(row_number(index)?))?;
            }
        }
        Change::SchemaChanged { .. } => return Err(E::Invalid("legacy schema event")),
    }
    Ok(())
}

fn row_number(index: usize) -> CodecResult<u32> {
    u32::try_from(index)
        .ok()
        .filter(|row| *row != u32::MAX)
        .ok_or(E::Limit)
}
fn node(graph: &SeleneGraph, id: NodeId) -> CodecResult<NodeRow> {
    graph
        .node_rows
        .get(&id)
        .copied()
        .filter(|row| graph.node_store.is_alive_row(*row))
        .ok_or(E::Semantic)
}
fn edge(graph: &SeleneGraph, id: EdgeId) -> CodecResult<EdgeRow> {
    graph
        .edge_rows
        .get(&id)
        .copied()
        .filter(|row| graph.edge_store.is_alive_row(*row))
        .ok_or(E::Semantic)
}
fn clear_node(graph: &mut SeleneGraph, row: NodeRow) {
    Arc::make_mut(&mut graph.node_store.alive).remove(row.get());
    graph.node_store.labels.set(row.index(), LabelSet::new());
    graph
        .node_store
        .properties
        .set(row.index(), PropertyMap::new());
}
fn clear_edge(graph: &mut SeleneGraph, row: EdgeRow) -> CodecResult<()> {
    Arc::make_mut(&mut graph.edge_store.alive).remove(row.get());
    graph.edge_store.label.set(
        row.index(),
        selene_core::db_string("").map_err(|_| E::Semantic)?,
    );
    graph.edge_store.source.set(row.index(), NodeId::TOMBSTONE);
    graph.edge_store.target.set(row.index(), NodeId::TOMBSTONE);
    graph
        .edge_store
        .properties
        .set(row.index(), PropertyMap::new());
    Ok(())
}
fn patch(properties: Option<&PropertyMap>, diff: &PropertyDiff) -> CodecResult<PropertyMap> {
    let mut properties = properties.ok_or(E::Semantic)?.clone();
    for (name, value) in &diff.set {
        properties
            .set(name.clone(), value.clone())
            .map_err(|_| E::Semantic)?;
    }
    for name in &diff.removed {
        properties.remove(name);
    }
    Ok(properties)
}
