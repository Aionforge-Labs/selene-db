//! Typed mutation funnel per spec 03 section 4.3.

mod assignment;
mod catalog;
mod catalog_alter;
mod composite_property_index;
mod delete;
mod delete_set;
mod factory_reset;
mod property_index;
mod remove;
mod text_index;
mod vector_index;

use std::sync::Arc;

use immutable_chunkmap::map::MapM;
use roaring::RoaringBitmap;
use selene_core::{
    Change, DbString, EdgeId, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, SchemaChange,
};

use crate::adjacency::AdjacencyEdge;
use crate::error::{GraphError, GraphResult};
use crate::graph_types::{GraphTypeDef, PropertyTypeDef};
use crate::id_map::get_or_insert_default;
use crate::index_provider::{IndexProvider, ProviderTag};
use crate::store::{EdgeRow, NodeRow};
use crate::type_validator::{EntityId, TypeViolation};
use crate::write_txn::WriteTxn;

/// Borrowed mutation builder for one write transaction.
pub struct Mutator<'tx, 'g> {
    txn: &'tx mut WriteTxn<'g>,
}

impl<'tx, 'g> Mutator<'tx, 'g> {
    pub(crate) fn new(txn: &'tx mut WriteTxn<'g>) -> Self {
        // A transaction may expose its COW working graph through `read()`. Give
        // every mutable-workspace lease a distinct candidate binding before
        // any mutation can occur, so candidates from aborted work cannot
        // validate against the restored published snapshot. Reacquisition
        // remints conservatively and never changes persisted generation
        // semantics.
        txn.guard_mut().remint_candidate_binding();
        Self { txn }
    }

    /// Create a node, emit `Change::NodeCreated`, and return its ID.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::RowSpaceExhausted`] when the dense row store fills
    /// the v1 row-index range (max 2^32 rows).
    pub fn create_node(&mut self, labels: LabelSet, mut props: PropertyMap) -> GraphResult<NodeId> {
        props.validate_stored_values()?;
        fill_node_defaults(self.txn.read(), &labels, &mut props)?;
        assignment::coerce_node_properties(self.txn.read(), &labels, &mut props)?;
        props.validate_stored_values()?;
        let id = self.txn.allocator.allocate_node()?;
        {
            let graph = self.txn.guard_mut();
            // BRIEF-Item-4c: append at the dense end (row = current row count)
            // instead of `id - 1` arithmetic. After 4b compaction the monotonic
            // high-water id far exceeds the dense row count, so an arith row would
            // re-pad exactly the holes compaction reclaimed; append keeps the store
            // dense and never resurrects a reclaimed slot. The u32 row-space cap
            // therefore moves from the id value to the row count.
            let row = u32::try_from(graph.node_store.len())
                .ok()
                // u32::MAX is reserved as RowIndex::TOMBSTONE; the last real row
                // is u32::MAX - 1, so a live row never aliases the sentinel.
                .filter(|&row| row != u32::MAX)
                .ok_or(GraphError::RowSpaceExhausted {
                    kind: "node",
                    rows: graph.node_store.len() as u64,
                    max_rows: u32::MAX as u64,
                })?;
            let row = NodeRow::new(row);
            // BRIEF-153 fix-cycle C2: run property-index admission BEFORE
            // mutating row state so a cap-exhaustion error rolls back
            // cleanly with no half-written row. Index updates only touch
            // their own maps; node_store stays untouched if any admission
            // fails. The txn boundary publishes everything atomically on
            // commit, so a partial-index publication is impossible.
            crate::property_index::apply_node_create(
                &mut graph.property_index,
                &labels,
                &props,
                row.get(),
            )?;
            crate::composite_property_index::apply_node_create(
                &mut graph.composite_property_index,
                &labels,
                &props,
                row.get(),
            )?;
            crate::vector_index::apply_node_create(
                &mut graph.vector_index,
                &labels,
                &props,
                row.get(),
            )?;
            crate::text_index::apply_node_create(
                &mut graph.text_index,
                &labels,
                &props,
                row.get(),
                id,
            );
            graph.node_store.labels.push(labels.clone());
            graph.node_store.properties.push(props.clone());
            graph.node_store.row_to_id.push(id);
            graph.node_store.mark_alive(row);
            // BRIEF-Item-4a: bind the external id to its row in both directions.
            // The live commit path never re-runs `rebuild_id_maps`, so the
            // `id -> row` map must be populated here. The row is remappable once
            // 4b compaction renumbers rows under stable ids.
            graph.node_rows.insert_cow(id, row);
            insert_node_labels(&mut graph.idx_label, row, &labels);
        }
        self.txn.changes.push(Change::NodeCreated {
            id,
            labels,
            properties: props,
        });
        Ok(id)
    }

    /// Create an edge between two alive nodes.
    pub fn create_edge(
        &mut self,
        label: DbString,
        source: NodeId,
        target: NodeId,
        props: PropertyMap,
    ) -> GraphResult<EdgeId> {
        self.create_mixed_edge(
            label,
            source,
            target,
            selene_core::EdgeDirectionality::Directed,
            props,
        )
    }

    /// Create one edge identity with intrinsic directionality. Undirected
    /// endpoints are canonicalized; reverse construction still creates a new
    /// parallel edge, never a second directed half of an existing identity.
    ///
    /// Directionality is immutable for the lifetime of an identity. Directed
    /// endpoint/type validation retains commit-time behavior; undirected
    /// endpoints are checked before incidence is staged, matching either order
    /// against the closed type's endpoint declarations. Ambiguous unordered
    /// declarations are rejected rather than choosing a property schema.
    ///
    /// # Errors
    /// Returns an error for absent endpoints, incompatible closed endpoint
    /// types, invalid properties, or exhausted identity/row space.
    ///
    /// ```
    /// use selene_core::{EdgeDirectionality, GraphId, LabelSet, PropertyMap, db_string};
    /// use selene_graph::SharedGraph;
    /// let graph = SharedGraph::new(GraphId::new(1));
    /// let mut tx = graph.begin_write();
    /// let mut m = tx.mutator();
    /// let a = m.create_node(LabelSet::new(), PropertyMap::new())?;
    /// let b = m.create_node(LabelSet::new(), PropertyMap::new())?;
    /// let edge = m.create_mixed_edge(db_string("E")?, b, a,
    ///     EdgeDirectionality::Undirected, PropertyMap::new())?;
    /// tx.commit()?;
    /// assert_eq!(graph.read().edge_endpoints(edge), Some((a, b)));
    /// assert_eq!(graph.read().undirected_edges(b).unwrap().len(), 1);
    /// # Ok::<(), selene_graph::GraphError>(())
    /// ```
    pub fn create_mixed_edge(
        &mut self,
        label: DbString,
        first: NodeId,
        second: NodeId,
        directionality: selene_core::EdgeDirectionality,
        mut props: PropertyMap,
    ) -> GraphResult<EdgeId> {
        let (source, target) = directionality.canonical_endpoints(first, second);
        props.validate_stored_values()?;
        self.require_live_node(source)?;
        self.require_live_node(target)?;
        fill_edge_defaults(
            self.txn.read(),
            label.clone(),
            source,
            target,
            directionality,
            &mut props,
        )?;
        assignment::coerce_edge_properties(
            self.txn.read(),
            label.clone(),
            source,
            target,
            directionality,
            &mut props,
        )?;
        let id = self.txn.allocator.allocate_edge()?;
        props.validate_stored_values()?;
        if directionality == selene_core::EdgeDirectionality::Undirected
            && let Some(type_def) = self.txn.read().meta.bound_type.as_deref()
        {
            crate::type_validator::validate_edge_endpoints(
                id,
                label.clone(),
                (source, target),
                directionality,
                self.txn.read(),
                type_def,
            )
            .map_err(GraphError::TypeViolation)?;
        }
        {
            let graph = self.txn.guard_mut();
            // BRIEF-Item-4c: append at the dense end (see create_node).
            let row = u32::try_from(graph.edge_store.len())
                .ok()
                .filter(|&row| row != u32::MAX) // u32::MAX is RowIndex::TOMBSTONE
                .ok_or(GraphError::RowSpaceExhausted {
                    kind: "edge",
                    rows: graph.edge_store.len() as u64,
                    max_rows: u32::MAX as u64,
                })?;
            let row = EdgeRow::new(row);
            crate::property_index::apply_edge_create(
                &mut graph.edge_property_index,
                &label,
                &props,
                row.get(),
            )?;
            graph.edge_store.label.push(label.clone());
            graph.edge_store.directionality.push(directionality);
            graph.edge_store.source.push(source);
            graph.edge_store.target.push(target);
            graph.edge_store.properties.push(props.clone());
            graph.edge_store.row_to_id.push(id);
            graph.edge_store.mark_alive(row);
            // BRIEF-Item-4a: bind the external edge id to its row (live path).
            graph.edge_rows.insert_cow(id, row);
            insert_index_row(&mut graph.idx_edge_label, label.clone(), row.get());

            if directionality == selene_core::EdgeDirectionality::Directed {
                get_or_insert_default(&mut graph.adjacency_out, source).add(AdjacencyEdge {
                    label: label.clone(),
                    neighbor: target,
                    edge_id: id,
                });
                get_or_insert_default(&mut graph.adjacency_in, target).add(AdjacencyEdge {
                    label: label.clone(),
                    neighbor: source,
                    edge_id: id,
                });
            } else {
                get_or_insert_default(&mut graph.adjacency_undirected, source).add(AdjacencyEdge {
                    label: label.clone(),
                    neighbor: target,
                    edge_id: id,
                });
                if source != target {
                    get_or_insert_default(&mut graph.adjacency_undirected, target).add(
                        AdjacencyEdge {
                            label: label.clone(),
                            neighbor: source,
                            edge_id: id,
                        },
                    );
                }
            }
        }
        self.txn.changes.push(Change::EdgeCreated {
            id,
            directionality,
            label,
            source,
            target,
            properties: props,
        });
        Ok(id)
    }

    /// Update an alive node and emit `Change::NodeUpdated`.
    pub fn update_node(
        &mut self,
        id: NodeId,
        labels_diff: LabelDiff,
        mut props_diff: PropertyDiff,
    ) -> GraphResult<()> {
        props_diff.validate_stored_values()?;
        let row = self.require_live_node(id)?;

        // Compute the new label set without mutating the working graph yet.
        let old_labels = self
            .txn
            .read()
            .node_store
            .labels
            .get(row.index())
            .cloned()
            .unwrap_or_default();
        let mut labels = old_labels.clone();
        for label in labels_diff.added.iter().cloned() {
            labels.insert(label);
        }
        for label in labels_diff.removed.iter() {
            labels.remove(label);
        }
        reject_immutable_node_update(self.txn.read(), id, &old_labels, &props_diff)?;
        reject_immutable_node_update(self.txn.read(), id, &labels, &props_diff)?;
        assignment::coerce_node_property_diff(self.txn.read(), &labels, &mut props_diff)?;

        // Apply the property diff up front; if it errors we leave the working
        // graph (including idx_label) untouched so the transaction can still
        // be safely rolled back or aborted without leaking inconsistent state.
        let old_props = self
            .txn
            .read()
            .node_store
            .properties
            .get(row.index())
            .cloned()
            .unwrap_or_default();
        let mut props = old_props.clone();
        apply_property_diff(&mut props, &props_diff)?;

        // BRIEF-153 fix-cycle C2: run property-index admission BEFORE
        // mutating row state so a cap-exhaustion error rolls back cleanly
        // with no half-written row. The index updates borrow `labels`/`props`;
        // both move into the row stores only after every fallible step.
        {
            let graph = self.txn.guard_mut();
            crate::property_index::apply_node_update(
                &mut graph.property_index,
                &old_labels,
                &old_props,
                &labels,
                &props,
                row.get(),
            )?;
            crate::composite_property_index::apply_node_update(
                &mut graph.composite_property_index,
                &old_labels,
                &old_props,
                &labels,
                &props,
                row.get(),
            )?;
            crate::vector_index::apply_node_update(
                &mut graph.vector_index,
                &old_labels,
                &old_props,
                &labels,
                &props,
                row.get(),
            )?;
            crate::text_index::apply_node_update(
                &mut graph.text_index,
                &old_labels,
                &old_props,
                &labels,
                &props,
                row.get(),
                id,
            );
            graph.node_store.labels.set(row.index(), labels);
            graph.node_store.properties.set(row.index(), props);
            for label in labels_diff.added.iter().cloned() {
                insert_index_row(&mut graph.idx_label, label, row.get());
            }
            for label in labels_diff.removed.iter() {
                remove_index_row(&mut graph.idx_label, label, row.get());
            }
        }

        self.txn.changes.push(Change::NodeUpdated {
            id,
            labels_diff,
            properties_diff: props_diff,
        });
        Ok(())
    }

    /// Update an alive edge and emit `Change::EdgeUpdated`.
    ///
    /// Edge labels are immutable, so property updates do not touch
    /// `idx_edge_label`.
    pub fn update_edge(&mut self, id: EdgeId, mut props_diff: PropertyDiff) -> GraphResult<()> {
        props_diff.validate_stored_values()?;
        let row = self.require_live_edge(id)?;
        reject_immutable_edge_update(self.txn.read(), id, &props_diff)?;
        assignment::coerce_edge_property_diff(self.txn.read(), id, &mut props_diff)?;
        let label = self
            .txn
            .read()
            .edge_store
            .label
            .get(row.index())
            .cloned()
            .ok_or(GraphError::EdgeNotFound { id })?;
        let old_props = self
            .txn
            .read()
            .edge_store
            .properties
            .get(row.index())
            .cloned()
            .unwrap_or_default();
        let mut props = old_props.clone();
        apply_property_diff(&mut props, &props_diff)?;
        {
            let graph = self.txn.guard_mut();
            graph.edge_store.properties.set(row.index(), props.clone());
            crate::property_index::apply_edge_update(
                &mut graph.edge_property_index,
                &label,
                &old_props,
                &props,
                row.get(),
            )?;
        }
        self.txn.changes.push(Change::EdgeUpdated {
            id,
            properties_diff: props_diff,
        });
        Ok(())
    }

    /// Append a raw [`SchemaChange`] WAL payload through the write funnel.
    ///
    /// This is a pass-through accumulator: catalog graph mutation and
    /// closed-graph validation are handled by higher-level validation layers
    /// (the typed catalog DDL methods on this `Mutator` — e.g. `create_node_type`
    /// — call those layers and then funnel here).
    ///
    /// The emitted record is stamped with the live transaction's graph id. It
    /// is not a caller parameter: since #1104 the id is load-bearing on the read
    /// side, where a record carrying a foreign id makes the directory
    /// unrecoverable under *every* id because recovery refuses it as
    /// cross-wired. Deriving it here makes that state unrepresentable rather
    /// than merely rejected.
    ///
    /// The catalog DDL methods are the production producers and push
    /// [`Change::SchemaChanged`] directly rather than routing through here, so
    /// this is a raw funnel rather than the only one. It stays `pub` so a future
    /// schema-event producer has a low-level entry that already stamps identity
    /// correctly instead of re-implementing the write path; tests and benches
    /// drive it to exercise the funnel without the DDL validation layer on top.
    pub fn schema_change(&mut self, change: SchemaChange) {
        let graph = self.txn.read().graph_id();
        self.txn
            .changes
            .push(Change::SchemaChanged { graph, change });
    }

    /// Look up a registered index provider through the held write transaction.
    #[must_use]
    pub fn index_provider_by_tag(&self, tag: ProviderTag) -> Option<Arc<dyn IndexProvider>> {
        self.txn
            .providers
            .iter()
            .find(|provider| provider.provider_tag() == tag)
            .map(Arc::clone)
    }

    /// Borrow the transaction-local working graph.
    #[must_use]
    pub fn read(&self) -> &crate::SeleneGraph {
        self.txn.read()
    }

    fn require_live_node(&self, id: NodeId) -> GraphResult<NodeRow> {
        let graph = self.txn.read();
        // Map-backed: a never-committed (aborted-tx hole) id is absent from the
        // map -> NotFound. A deleted id stays mapped to its dead row -> NotAlive.
        let row = graph
            .node_row_for_id(id)
            .ok_or(GraphError::NodeNotFound { id })?;
        if row.index() >= graph.node_store.len() {
            return Err(GraphError::NodeNotFound { id });
        }
        if !graph.node_store.is_alive_row(row) {
            return Err(GraphError::NodeNotAlive { id });
        }
        Ok(row)
    }

    fn require_live_edge(&self, id: EdgeId) -> GraphResult<EdgeRow> {
        let graph = self.txn.read();
        let row = graph
            .edge_row_for_id(id)
            .ok_or(GraphError::EdgeNotFound { id })?;
        if row.index() >= graph.edge_store.len() {
            return Err(GraphError::EdgeNotFound { id });
        }
        if !graph.edge_store.is_alive_row(row) {
            return Err(GraphError::EdgeNotAlive { id });
        }
        Ok(row)
    }
}

fn insert_node_labels(index: &mut MapM<DbString, RoaringBitmap>, row: NodeRow, labels: &LabelSet) {
    for label in labels.iter().cloned() {
        insert_index_row(index, label, row.get());
    }
}

fn remove_node_labels(index: &mut MapM<DbString, RoaringBitmap>, row: NodeRow, labels: &LabelSet) {
    for label in labels.iter() {
        remove_index_row(index, label, row.get());
    }
}

fn insert_index_row(index: &mut MapM<DbString, RoaringBitmap>, label: DbString, row: u32) {
    // Mutate the bitmap behind the persistent-map entry. `guard_mut` already
    // owns the working snapshot, so bulk-loading one label remains O(N), not
    // O(N²) from cloning the whole bitmap per row.
    get_or_insert_default(index, label).insert(row);
}

fn remove_index_row(index: &mut MapM<DbString, RoaringBitmap>, label: &DbString, row: u32) {
    // Mirror `insert_index_row`: mutate the bitmap behind the persistent entry
    // instead of cloning the whole RoaringBitmap for every row removed.
    let now_empty = match index.get_mut_cow(label) {
        Some(bitmap) => {
            bitmap.remove(row);
            bitmap.is_empty()
        }
        None => false,
    };
    if now_empty {
        index.remove_cow(label);
    }
}

fn fill_node_defaults(
    graph: &crate::SeleneGraph,
    labels: &LabelSet,
    props: &mut PropertyMap,
) -> GraphResult<()> {
    let Some(graph_type) = graph.meta.bound_type.as_deref() else {
        return Ok(());
    };
    let Some(node_type) = graph_type.find_node_type(labels) else {
        return Ok(());
    };
    fill_property_defaults(&node_type.properties, props)
}

fn fill_edge_defaults(
    graph: &crate::SeleneGraph,
    label: DbString,
    source: NodeId,
    target: NodeId,
    directionality: selene_core::EdgeDirectionality,
    props: &mut PropertyMap,
) -> GraphResult<()> {
    let Some(graph_type) = graph.meta.bound_type.as_deref() else {
        return Ok(());
    };
    let Some(source_type) = node_type_index_for_node(graph, graph_type, source) else {
        return Ok(());
    };
    let Some(target_type) = node_type_index_for_node(graph, graph_type, target) else {
        return Ok(());
    };
    let Some(edge_type) =
        graph_type.find_mixed_edge_type(label, source_type, target_type, directionality)
    else {
        return Ok(());
    };
    fill_property_defaults(&edge_type.properties, props)
}

fn fill_property_defaults(
    declarations: &[PropertyTypeDef],
    props: &mut PropertyMap,
) -> GraphResult<()> {
    for declaration in declarations {
        if props.contains_key(&declaration.name) {
            continue;
        }
        if let Some(default) = &declaration.default {
            props.set(declaration.name.clone(), default.to_value()?)?;
        }
    }
    Ok(())
}

fn reject_immutable_node_update(
    graph: &crate::SeleneGraph,
    id: NodeId,
    labels: &LabelSet,
    diff: &PropertyDiff,
) -> GraphResult<()> {
    let Some(graph_type) = graph.meta.bound_type.as_deref() else {
        return Ok(());
    };
    let Some(node_type) = graph_type.find_node_type(labels) else {
        return Ok(());
    };
    reject_immutable_property_update(
        EntityId::Node(id),
        node_type.name.clone(),
        &node_type.properties,
        diff,
    )
}

fn reject_immutable_edge_update(
    graph: &crate::SeleneGraph,
    id: EdgeId,
    diff: &PropertyDiff,
) -> GraphResult<()> {
    let Some(graph_type) = graph.meta.bound_type.as_deref() else {
        return Ok(());
    };
    let Some(label) = graph.edge_label(id).cloned() else {
        return Ok(());
    };
    let Some((source, target)) = graph.edge_endpoints(id) else {
        return Ok(());
    };
    let Some(source_type) = node_type_index_for_node(graph, graph_type, source) else {
        return Ok(());
    };
    let Some(target_type) = node_type_index_for_node(graph, graph_type, target) else {
        return Ok(());
    };
    let Some(edge_type) = graph_type.find_mixed_edge_type(
        label,
        source_type,
        target_type,
        graph
            .edge_directionality(id)
            .ok_or(GraphError::EdgeNotFound { id })?,
    ) else {
        return Ok(());
    };
    reject_immutable_property_update(
        EntityId::Edge(id),
        edge_type.name.clone(),
        &edge_type.properties,
        diff,
    )
}

fn node_type_index_for_node(
    graph: &crate::SeleneGraph,
    graph_type: &GraphTypeDef,
    node: NodeId,
) -> Option<u32> {
    let labels = graph.node_labels(node)?;
    graph_type.find_node_type_index(labels)
}

fn reject_immutable_property_update(
    entity_id: EntityId,
    declared_in: DbString,
    declarations: &[PropertyTypeDef],
    diff: &PropertyDiff,
) -> GraphResult<()> {
    for (key, _) in &diff.set {
        reject_if_immutable(entity_id, declared_in.clone(), declarations, key.clone())?;
    }
    for key in &diff.removed {
        reject_if_immutable(entity_id, declared_in.clone(), declarations, key.clone())?;
    }
    Ok(())
}

fn reject_if_immutable(
    entity_id: EntityId,
    declared_in: DbString,
    declarations: &[PropertyTypeDef],
    property: DbString,
) -> GraphResult<()> {
    if declarations
        .iter()
        .any(|declaration| declaration.name == property && declaration.immutable)
    {
        return Err(GraphError::TypeViolation(
            TypeViolation::ImmutablePropertyUpdate {
                entity_id,
                property,
                declared_in,
            },
        ));
    }
    Ok(())
}

fn apply_property_diff(map: &mut PropertyMap, diff: &PropertyDiff) -> GraphResult<()> {
    for (key, value) in diff.set.iter() {
        map.set(key.clone(), value.clone())?;
    }
    for key in diff.removed.iter() {
        map.remove(key);
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod id_map_tests;

#[cfg(test)]
mod row_cap_tests;

#[cfg(test)]
mod truncate_tests;

#[cfg(test)]
mod factory_reset_tests;

#[cfg(test)]
mod hub_delete_tests;

#[cfg(test)]
mod delete_set_tests;

#[cfg(test)]
mod payload_clear_tests;
