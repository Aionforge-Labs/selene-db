//! `DROP GRAPH` factory-reset for the transaction mutator.
//!
//! Extracted from the mutator module to keep `mutator.rs` under the 700-LOC
//! file cap. The reset reuses the same change-free row-removal cores the
//! BRIEF-150 truncate path uses (`remove_node_row` / `remove_edge_row`), so the
//! resulting in-memory state is byte-identical to `MATCH (n) DETACH DELETE n`
//! plus a full schema drop.

use selene_core::Change;

use crate::Mutator;
use crate::error::GraphResult;

impl<'tx, 'g> Mutator<'tx, 'g> {
    /// Factory-reset the entire graph: wipe every node and edge and reset the
    /// schema to open, recording exactly **one** declarative
    /// [`Change::GraphReset`] (deletion-reclamation audit Item 10, BRIEF-152).
    ///
    /// This is the `DROP GRAPH` primitive. Under D1 single-graph it targets the
    /// one bound graph. Behaviour and invariants:
    ///
    /// * **Wipes ALL rows, including untyped ones.** Live rows are enumerated
    ///   from the two alive [`roaring::RoaringBitmap`]s in the node and edge stores,
    ///   **never** per label — so nodes/edges whose labels are not declared
    ///   types (legal in an open GG01 graph) are removed too. A per-type
    ///   truncate would silently miss them.
    /// * **Resets the schema to open.** `meta.bound_type` is set to `None`,
    ///   making a previously closed (GG02) graph open (GG01) again. There are no
    ///   standalone type defs outside `bound_type`, so clearing it is the
    ///   complete schema reset. As a side effect, the commit-time closed-graph
    ///   validation loop (`write_txn`) is skipped entirely once `bound_type` is
    ///   `None`, which is correct — nothing is left to validate.
    /// * **O(1) WAL.** Exactly one [`Change::GraphReset`] is pushed regardless of
    ///   the number of rows removed. The per-row `NodeDeleted`/`EdgeDeleted`
    ///   tombstones are staged into the fan-out buffer (`truncate_expansions`)
    ///   only, never into the persisted changeset, so derived state (e.g. the
    ///   label/property indexes) is reclaimed without leaks while the WAL stays
    ///   constant-size.
    /// * **Idempotent.** On an already-empty + open graph the row enumeration is
    ///   empty and `bound_type` is already `None`; a `GraphReset` is still pushed
    ///   with an empty staged expansion (which the fan-out expander drops), so a
    ///   second `DROP GRAPH` is a clean observable no-op, never an error.
    ///
    /// The MANIFEST epoch and WAL archive lineage are untouched: this is one
    /// committed WAL entry on top of the existing snapshot, not a file-level
    /// wipe.
    ///
    /// # Errors
    ///
    /// Returns a [`crate::GraphError`] only if the change-free removal cores hit
    /// a structural inconsistency (e.g. a missing edge row for a live index
    /// entry) — the same error surface the truncate path exposes.
    pub fn factory_reset(&mut self) -> GraphResult<()> {
        // Snapshot every live row BEFORE any removal: removal mutates the alive
        // bitmaps and adjacency/index structures we are iterating (the same
        // clone-collect discipline truncate_node_type uses).
        let node_rows: Vec<_> = self.txn.read().node_store.alive_rows().collect();
        let edge_rows: Vec<_> = self.txn.read().edge_store.alive_rows().collect();

        let mut expansion = Vec::with_capacity(node_rows.len() + edge_rows.len());
        for row in node_rows {
            // Every row came from the alive bitmap, so its external id is mapped
            // (an unmapped row would be a never-committed hole, never alive).
            let Some(id) = self.txn.read().node_id_for_node_row(row) else {
                continue;
            };
            // remove_node_row scrubs idx_label, property/composite indexes,
            // adjacency, and node liveness. Its returned incident-edge set is
            // discarded here because the alive-edge bitmap below is the
            // authoritative superset (it also covers untyped edges between
            // untyped nodes), so we clear every edge row directly.
            let _ = self.remove_node_row(id, row)?;
            expansion.push(Change::NodeDeleted { id });
        }

        // Remove every still-alive edge row. remove_node_row detached incident
        // edges from adjacency but did NOT clear edge liveness / edge-label
        // index, so iterate the full alive-edge set captured before removal.
        for row in edge_rows {
            // Defensive: a row may already be dead if it shared two truncated
            // endpoints — remove_edge_row is only called for still-alive rows.
            if !self.txn.read().edge_store.is_alive_row(row) {
                continue;
            }
            let Some(id) = self.txn.read().edge_id_for_edge_row(row) else {
                continue;
            };
            debug_assert!(
                self.txn.read().edge_row_for_id(id) == Some(row),
                "edge row/id round-trip must hold"
            );
            self.remove_edge_row(id, row)?;
            expansion.push(Change::EdgeDeleted { id });
        }

        // Reset the schema to open. Different from DROP TYPE (which keeps the
        // graph closed by setting bound_type = Some(next)); factory-reset clears
        // the WHOLE bound_type to None.
        let graph = self.txn.guard_mut();
        graph.meta.bound_type = None;
        graph.remint_layout();

        // Push EXACTLY ONE declarative change (O(1) WAL) and stage the per-row
        // tombstones for subscriber fan-out, keyed to this change's index.
        let index = self.txn.changes.len();
        self.txn.changes.push(Change::GraphReset {});
        self.txn.truncate_expansions.push((index, expansion));
        Ok(())
    }
}
