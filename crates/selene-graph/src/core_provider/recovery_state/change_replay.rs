//! WAL change replay for core-provider recovery state.

use std::collections::BTreeSet;

use rustc_hash::FxHashMap;
use selene_core::{
    Change, DbString, EdgeId, GraphId, LabelSet, NodeId, PropertyDiff, PropertyMap, SchemaChange,
    db_string,
};

use crate::{
    ProviderError,
    core_provider::{
        inconsistent,
        sections::{EdgeRow, NodeRow},
    },
};

use super::{
    RecoveredEdgeRow, RecoveredNodeRow, RecoveryState,
    index_replay::{
        pending_composite_property_index_change, pending_property_index_change,
        pending_text_index_change, pending_vector_index_change,
    },
    schema_replay,
};

impl RecoveryState {
    /// Apply one WAL change to recovery state.
    ///
    /// `SchemaChange` routing is intentionally exhaustive: silent-skip
    /// wildcards are forbidden. The executable intent matrix lives in
    /// `SCHEMA_CHANGE_INTENT` in this module's tests; new variants must update
    /// both this match and that table.
    pub(crate) fn apply_change(&mut self, change: &Change) -> Result<(), ProviderError> {
        change
            .validate_stored_values()
            .map_err(|error| inconsistent(error.to_string()))?;
        // Schema records are the only changes that name their authoring graph,
        // which makes them the sole on-disk identity of a WAL-only directory.
        // Recorded here rather than in the arm below so the identity is captured
        // even for a schema record the arm goes on to reject.
        if let Change::SchemaChanged { graph, .. } = change {
            self.observe_wal_graph_id(*graph);
        }
        match change {
            Change::NodeCreated {
                id,
                labels,
                properties,
            } => {
                if self.nodes.contains_key(id) {
                    return Err(inconsistent(format!(
                        "WAL replay attempted to recreate node {id}; node ids are never \
                         reused once allocated (D11)"
                    )));
                }
                self.nodes.insert(
                    *id,
                    RecoveredNodeRow::from_wal(NodeRow {
                        labels: labels.clone(),
                        properties: properties.clone(),
                        alive: true,
                    }),
                );
                self.wal_node_order.push(*id);
            }
            Change::NodeUpdated {
                id,
                labels_diff,
                properties_diff,
            } => {
                let row = require_live_node(&mut self.nodes, *id)?;
                for label in labels_diff.added.iter().cloned() {
                    row.labels.insert(label);
                }
                for label in labels_diff.removed.iter() {
                    row.labels.remove(label);
                }
                apply_property_diff(&mut row.properties, properties_diff)?;
            }
            Change::NodeDeleted { id } => {
                let row = require_live_node(&mut self.nodes, *id)?;
                clear_node_row(row);
            }
            Change::EdgeCreated {
                id,
                directionality,
                label,
                source,
                target,
                properties,
            } => {
                require_live_node_ref(&self.nodes, *source)?;
                require_live_node_ref(&self.nodes, *target)?;
                if directionality.canonical_endpoints(*source, *target) != (*source, *target) {
                    return Err(inconsistent(format!(
                        "edge {id} has noncanonical undirected endpoints"
                    )));
                }
                if self.edges.contains_key(id) {
                    return Err(inconsistent(format!(
                        "WAL replay attempted to recreate edge {id}; edge ids are never \
                         reused once allocated (D11)"
                    )));
                }
                self.edges.insert(
                    *id,
                    RecoveredEdgeRow::from_wal(EdgeRow {
                        directionality: *directionality,
                        label: label.clone(),
                        source: *source,
                        target: *target,
                        properties: properties.clone(),
                        alive: true,
                    }),
                );
                self.wal_edge_order.push(*id);
            }
            Change::EdgeUpdated {
                id,
                properties_diff,
            } => {
                let row = require_live_edge(&mut self.edges, *id)?;
                apply_property_diff(&mut row.properties, properties_diff)?;
            }
            Change::EdgeDeleted { id } => {
                let row = require_live_edge(&mut self.edges, *id)?;
                clear_edge_row(row);
            }
            Change::NodePropertyRemoved { id, property } => {
                let row = require_live_node(&mut self.nodes, *id)?;
                row.properties.remove(property);
            }
            Change::EdgePropertyRemoved { id, property } => {
                let row = require_live_edge(&mut self.edges, *id)?;
                row.properties.remove(property);
            }
            Change::NodeLabelRemoved { id, label } => {
                let row = require_live_node(&mut self.nodes, *id)?;
                row.labels.remove(label);
            }
            Change::NodesOfTypeTruncated { label } => {
                // Re-derive the truncated rows from the recovered store: every
                // alive node carrying the label, plus every alive edge incident
                // to such a node (any edge type). This reconstructs the exact
                // post-`delete_node`-cascade state without persisting any ids.
                let mut truncated_nodes = BTreeSet::new();
                for (id, recovered) in self.nodes.iter_mut() {
                    if recovered.row.alive && recovered.row.labels.contains(label) {
                        truncated_nodes.insert(*id);
                        clear_node_row(&mut recovered.row);
                    }
                }
                for recovered in self.edges.values_mut() {
                    if recovered.row.alive
                        && (truncated_nodes.contains(&recovered.row.source)
                            || truncated_nodes.contains(&recovered.row.target))
                    {
                        clear_edge_row(&mut recovered.row);
                    }
                }
            }
            Change::EdgesOfTypeTruncated { label } => {
                for recovered in self.edges.values_mut() {
                    if recovered.row.alive && recovered.row.label == *label {
                        clear_edge_row(&mut recovered.row);
                    }
                }
            }
            Change::GraphReset {} => {
                // Re-derive every live row from the recovered store at this WAL
                // position and mark it dead - identical to the runtime mutator,
                // which carries no ids in the declarative change ("replay walks
                // the store"). Wipes ALL nodes/edges incl untyped ones.
                self.nodes
                    .values_mut()
                    .for_each(|recovered| clear_node_row(&mut recovered.row));
                self.edges
                    .values_mut()
                    .for_each(|recovered| clear_edge_row(&mut recovered.row));
                // A reset moots every prior schema/index intent in the WAL up to
                // this point, and forces the recovered graph open.
                self.schema_reset_to_open = true;
                self.pending_schema_changes.clear();
                self.pending_property_index_changes.clear();
                self.pending_composite_property_index_changes.clear();
                self.pending_vector_index_changes.clear();
                self.pending_text_index_changes.clear();
            }
            Change::SchemaChanged { change, .. } => match change {
                SchemaChange::NodeTypeAdded { .. }
                | SchemaChange::EdgeTypeAdded { .. }
                | SchemaChange::NodeTypeAddedV2 { .. }
                | SchemaChange::NodeTypeAlteredV2 { .. }
                | SchemaChange::EdgeTypeAlteredV2 { .. }
                | SchemaChange::EdgeTypeAddedV2 { .. }
                | SchemaChange::NodeTypeDropped { .. }
                | SchemaChange::EdgeTypeDropped { .. } => {
                    self.pending_schema_changes.push(change.clone());
                }
                SchemaChange::PropertyIndexCreated { .. }
                | SchemaChange::PropertyIndexCreatedNamed { .. }
                | SchemaChange::PropertyIndexDropped { .. }
                | SchemaChange::EdgePropertyIndexCreated { .. }
                | SchemaChange::EdgePropertyIndexDropped { .. } => {
                    let pending = pending_property_index_change(change)
                        .expect("property-index variants map to pending recovery intent");
                    self.pending_property_index_changes.push(pending);
                }
                SchemaChange::CompositePropertyIndexCreated { .. }
                | SchemaChange::CompositePropertyIndexDropped { .. } => {
                    let pending = pending_composite_property_index_change(change)
                        .expect("composite property-index variants map to pending recovery intent");
                    self.pending_composite_property_index_changes.push(pending);
                }
                SchemaChange::VectorIndexCreated { .. }
                | SchemaChange::VectorIndexDropped { .. } => {
                    let pending = pending_vector_index_change(change)
                        .expect("vector-index variants map to pending recovery intent");
                    self.pending_vector_index_changes.push(pending);
                }
                SchemaChange::TextIndexCreated { .. } | SchemaChange::TextIndexDropped { .. } => {
                    let pending = pending_text_index_change(change)
                        .expect("text-index variants map to pending recovery intent");
                    self.pending_text_index_changes.push(pending);
                }
                SchemaChange::GraphCreated { .. }
                | SchemaChange::GraphDropped { .. }
                | SchemaChange::GraphTypeCreated { .. }
                | SchemaChange::GraphTypeDropped { .. }
                | SchemaChange::RecordTypeAdded { .. } => {
                    return Err(schema_replay::unsupported_schema_recovery(change));
                }
            },
        }
        Ok(())
    }

    /// Record the graph id a replayed schema record was authored under.
    ///
    /// First id wins; a later disagreeing id is kept separately so
    /// `into_graph` can name both when refusing a cross-wired WAL.
    fn observe_wal_graph_id(&mut self, graph: GraphId) {
        match self.wal_graph_id {
            None => self.wal_graph_id = Some(graph),
            Some(first) if first != graph => {
                self.wal_graph_id_conflict.get_or_insert(graph);
            }
            Some(_) => {}
        }
    }
}

fn require_live_node(
    nodes: &mut FxHashMap<NodeId, RecoveredNodeRow>,
    id: NodeId,
) -> Result<&mut NodeRow, ProviderError> {
    let recovered = nodes
        .get_mut(&id)
        .ok_or_else(|| inconsistent(format!("WAL replay referenced missing node {id}")))?;
    if !recovered.row.alive {
        return Err(inconsistent(format!(
            "WAL replay referenced deleted node {id}"
        )));
    }
    Ok(&mut recovered.row)
}

fn require_live_node_ref(
    nodes: &FxHashMap<NodeId, RecoveredNodeRow>,
    id: NodeId,
) -> Result<(), ProviderError> {
    let recovered = nodes
        .get(&id)
        .ok_or_else(|| inconsistent(format!("WAL replay referenced missing node {id}")))?;
    if !recovered.row.alive {
        return Err(inconsistent(format!(
            "WAL replay referenced deleted node {id}"
        )));
    }
    Ok(())
}

fn require_live_edge(
    edges: &mut FxHashMap<EdgeId, RecoveredEdgeRow>,
    id: EdgeId,
) -> Result<&mut EdgeRow, ProviderError> {
    let recovered = edges
        .get_mut(&id)
        .ok_or_else(|| inconsistent(format!("WAL replay referenced missing edge {id}")))?;
    if !recovered.row.alive {
        return Err(inconsistent(format!(
            "WAL replay referenced deleted edge {id}"
        )));
    }
    Ok(&mut recovered.row)
}

fn clear_node_row(row: &mut NodeRow) {
    row.labels = LabelSet::new();
    row.properties = PropertyMap::new();
    row.alive = false;
}

fn clear_edge_row(row: &mut EdgeRow) {
    row.label = dead_edge_label();
    row.source = NodeId::TOMBSTONE;
    row.target = NodeId::TOMBSTONE;
    row.properties = PropertyMap::new();
    row.alive = false;
}

fn dead_edge_label() -> DbString {
    db_string("").expect("empty edge tombstone label is always within the string cap")
}

fn apply_property_diff(map: &mut PropertyMap, diff: &PropertyDiff) -> Result<(), ProviderError> {
    for (key, value) in diff.set.iter() {
        map.set(key.clone(), value.clone())
            .map_err(|error| inconsistent(format!("WAL replay property set failed: {error}")))?;
    }
    for key in diff.removed.iter() {
        map.remove(key);
    }
    Ok(())
}
