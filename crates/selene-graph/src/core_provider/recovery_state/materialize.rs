//! Row materialization for recovery: place a decoded `NodeRow`/`EdgeRow` at a
//! caller-resolved row index, padding the `*Id::TOMBSTONE` hole slots in
//! between.
//!
//! Split out of `recovery_state.rs` to keep that file under the 700-LOC cap.
//! BRIEF-Item-4a STEP 9 / BRIEF-Item-4c: the `row_index` is supplied by
//! `into_graph` (the decoded snapshot position, or the dense-end append slot for
//! WAL-created ids) rather than derived from the id here, so snapshots whose rows
//! are not in id order round-trip.

use std::sync::OnceLock;

use crate::core_provider::sections::{EdgeRow as EdgePayload, NodeRow as NodePayload};
use crate::graph::SeleneGraph;
use crate::store::{EdgeRow, NodeRow};
use selene_core::{DbString, EdgeId, LabelSet, NodeId, PropertyMap};

pub(super) fn insert_node_row(
    graph: &mut SeleneGraph,
    id: NodeId,
    row: NodePayload,
    row_index: NodeRow,
) -> crate::GraphResult<()> {
    // The pad-then-set shape materializes the `NodeId::TOMBSTONE` hole slots
    // that sit between the real rows the snapshot recorded.
    while graph.node_store.len() < row_index.index() {
        graph.node_store.labels.push(LabelSet::new());
        graph.node_store.properties.push(PropertyMap::new());
        // BRIEF-Item-4a: hole rows carry the tombstone id; row_to_id stays
        // length-locked with the row columns. `rebuild_id_maps` (shared.rs)
        // builds the id->row maps from the alive rows after recovery.
        graph.node_store.row_to_id.push(NodeId::TOMBSTONE);
    }
    if graph.node_store.len() == row_index.index() {
        graph.node_store.labels.push(row.labels);
        graph.node_store.properties.push(row.properties);
        graph.node_store.row_to_id.push(id);
    } else {
        graph.node_store.labels.set(row_index.index(), row.labels);
        graph
            .node_store
            .properties
            .set(row_index.index(), row.properties);
        graph.node_store.row_to_id.set(row_index.index(), id);
    }
    // BRIEF-Item-4a: bind id -> row in the map for every materialized row (alive
    // AND dead — a deleted recovered id stays mapped -> NotAlive, Option B). The
    // recovery-internal closed-graph validation reads labels through this map, so
    // it must be populated here, before that check (and before shared.rs's
    // rebuild_id_maps re-seeds it). Holes carry the tombstone and never reach
    // this real-row branch.
    graph.node_rows.insert_cow(id, row_index);
    Ok(())
}

pub(super) fn insert_edge_row(
    graph: &mut SeleneGraph,
    id: EdgeId,
    row: EdgePayload,
    row_index: EdgeRow,
) -> crate::GraphResult<()> {
    while graph.edge_store.len() < row_index.index() {
        graph.edge_store.label.push(edge_hole_label()?);
        graph
            .edge_store
            .directionality
            .push(selene_core::EdgeDirectionality::Directed);
        graph.edge_store.source.push(NodeId::TOMBSTONE);
        graph.edge_store.target.push(NodeId::TOMBSTONE);
        graph.edge_store.properties.push(PropertyMap::new());
        // BRIEF-Item-4a: hole rows carry the tombstone id (see insert_node_row).
        graph.edge_store.row_to_id.push(EdgeId::TOMBSTONE);
    }
    if graph.edge_store.len() == row_index.index() {
        graph.edge_store.label.push(row.label);
        graph.edge_store.directionality.push(row.directionality);
        graph.edge_store.source.push(row.source);
        graph.edge_store.target.push(row.target);
        graph.edge_store.properties.push(row.properties);
        graph.edge_store.row_to_id.push(id);
    } else {
        graph.edge_store.label.set(row_index.index(), row.label);
        graph
            .edge_store
            .directionality
            .set(row_index.index(), row.directionality);
        graph.edge_store.source.set(row_index.index(), row.source);
        graph.edge_store.target.set(row_index.index(), row.target);
        graph
            .edge_store
            .properties
            .set(row_index.index(), row.properties);
        graph.edge_store.row_to_id.set(row_index.index(), id);
    }
    // BRIEF-Item-4a: bind id -> row in the map for every materialized row (see
    // insert_node_row).
    graph.edge_rows.insert_cow(id, row_index);
    Ok(())
}

fn edge_hole_label() -> Result<DbString, crate::GraphError> {
    static CELL: OnceLock<DbString> = OnceLock::new();
    if let Some(label) = CELL.get() {
        return Ok(label.clone());
    }
    let label = selene_core::db_string("__selene_hole").map_err(crate::GraphError::Core)?;
    let _ = CELL.set(label.clone());
    Ok(label)
}
