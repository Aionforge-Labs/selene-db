//! BRIEF-Item-4a STEP 9/10 recovery proof: a snapshot whose external ids are
//! deliberately NOT `row + 1` round-trips through encode -> recover with the
//! rows placed **positionally** (by section position), never via `id - 1`
//! arithmetic.
//!
//! This is the recovery-path counterpart to
//! `mutator::id_map_tests::non_identity_map_read_paths_resolve_by_map` (which
//! proves the in-memory read stack). Here the same non-identity shape is driven
//! through the real `CORE/NODE` + `CORE/EDGE` snapshot encoder
//! (`sections::encode_nodes`/`encode_edges`, which now persist the explicit id
//! from the `row_to_id` column) and `SharedGraph::recover`. It is the assertion
//! that would catch a regression to arithmetic placement once 4b compaction
//! starts emitting genuinely non-identity snapshots.
//!
//! Coverage matrix (one graph):
//! - non-identity alive rows (NodeId 5@row0, 8@row1, 20@row4) place positionally;
//! - a committed-then-deleted row (NodeId 15@row3, Option B keeps its id) recovers
//!   mapped-but-dead -> `is_node_alive == false` yet `node_row_for_id` resolves;
//! - an aborted-tx hole row (TOMBSTONE@row2) is preserved positionally but stays
//!   OUT of the id->row map -> its would-be arithmetic id resolves to `None`
//!   (NotFound), the live-vs-recovered consistency STEP 9 fixes;
//! - the edge store mirrors all three cases and adjacency rebuilds from the
//!   positional external ids.

use selene_core::{Change, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, db_string};

use super::{append_wal, node_created, sample_shared_graph, temp_dir, write_snapshot};
use crate::store::{EdgeRow, NodeRow};
use crate::{SeleneGraph, SharedGraph};

/// Hand-build a graph whose external ids are not `row + 1`, with an interior
/// hole (aborted-tx) and a deleted-but-kept row in both stores.
fn non_identity_graph() -> SeleneGraph {
    let nlabel = db_string("nidr.node").unwrap();
    let elabel = db_string("nidr.edge").unwrap();
    let hole_elabel = db_string("__selene_hole").unwrap();

    let mut g = SeleneGraph::new(GraphId::new(1));

    // Nodes. Rows: 0->5 (alive), 1->8 (alive), 2->TOMBSTONE hole, 3->15
    // (deleted, id kept), 4->20 (alive).
    let push_node = |g: &mut SeleneGraph, id: NodeId, alive: bool| {
        let row = g.node_store.row_to_id.len() as u32;
        g.node_store.labels.push(if id == NodeId::TOMBSTONE {
            LabelSet::new()
        } else {
            LabelSet::single(nlabel.clone())
        });
        g.node_store.properties.push(PropertyMap::new());
        g.node_store.row_to_id.push(id);
        if alive {
            g.node_store.alive_mut().insert(row);
        }
    };
    push_node(&mut g, NodeId::new(5), true);
    push_node(&mut g, NodeId::new(8), true);
    push_node(&mut g, NodeId::TOMBSTONE, false); // aborted-tx hole
    push_node(&mut g, NodeId::new(15), false); // committed-then-deleted
    push_node(&mut g, NodeId::new(20), true);
    g.meta.next_node_id = 21;

    // Edges. Rows: 0->3 (5->8, alive), 1->TOMBSTONE hole, 2->7 (20->5, alive).
    let push_edge =
        |g: &mut SeleneGraph, id: EdgeId, source: NodeId, target: NodeId, alive: bool| {
            let row = g.edge_store.row_to_id.len() as u32;
            g.edge_store.label.push(if id == EdgeId::TOMBSTONE {
                hole_elabel.clone()
            } else {
                elabel.clone()
            });
            g.edge_store.source.push(source);
            g.edge_store.target.push(target);
            g.edge_store.properties.push(PropertyMap::new());
            g.edge_store.row_to_id.push(id);
            if alive {
                g.edge_store.alive_mut().insert(row);
            }
        };
    push_edge(&mut g, EdgeId::new(3), NodeId::new(5), NodeId::new(8), true);
    push_edge(
        &mut g,
        EdgeId::TOMBSTONE,
        NodeId::TOMBSTONE,
        NodeId::TOMBSTONE,
        false,
    );
    push_edge(
        &mut g,
        EdgeId::new(7),
        NodeId::new(20),
        NodeId::new(5),
        true,
    );
    g.meta.next_edge_id = 8;

    g
}

#[test]
fn non_identity_snapshot_round_trips_positionally() {
    let dir = temp_dir("nodeid-split-positional");
    let shared = SharedGraph::from_graph(non_identity_graph());
    write_snapshot(&dir, &shared, 1);

    let recovered = SharedGraph::recover(&dir, GraphId::new(1)).expect("recovery succeeds");
    let g = recovered.read();

    // --- Nodes: rows placed by stored position, NOT by `id - 1` arithmetic. ---
    assert_eq!(g.node_row_for_id(NodeId::new(5)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(8)), Some(NodeRow::new(1)));
    assert_eq!(g.node_row_for_id(NodeId::new(15)), Some(NodeRow::new(3)));
    assert_eq!(g.node_row_for_id(NodeId::new(20)), Some(NodeRow::new(4)));
    // The arithmetic answer (id 20 -> row 19) is WRONG; positional says row 4.
    assert_ne!(g.node_row_for_id(NodeId::new(20)), Some(NodeRow::new(19)));
    assert_eq!(
        g.node_id_for_node_row(NodeRow::new(0)),
        Some(NodeId::new(5))
    );
    assert_eq!(
        g.node_id_for_node_row(NodeRow::new(4)),
        Some(NodeId::new(20))
    );
    // Interior hole row is preserved positionally but carries no external id.
    assert_eq!(g.node_id_for_node_row(NodeRow::new(2)), None);
    assert_eq!(g.node_store.len(), 5, "the interior hole row is preserved");

    // --- Liveness: deleted (mapped, dead) vs aborted hole (unmapped). ---
    assert!(g.is_node_alive(NodeId::new(5)));
    assert!(g.is_node_alive(NodeId::new(8)));
    assert!(g.is_node_alive(NodeId::new(20)));
    // Deleted node 15 stays mapped to its dead row -> NotAlive, not NotFound.
    assert!(!g.is_node_alive(NodeId::new(15)));
    assert_eq!(g.node_row_for_id(NodeId::new(15)), Some(NodeRow::new(3)));
    // Aborted-tx hole: under the OLD format the row-2 hole encoded `NodeId(3)`
    // (row+1) and recovery bound it -> NotAlive. STEP 9 encodes TOMBSTONE and
    // skips it, so `NodeId(3)` resolves NotFound, matching the live path.
    assert_eq!(g.node_row_for_id(NodeId::new(3)), None);
    assert!(!g.is_node_alive(NodeId::new(3)));
    assert_eq!(g.node_count(), 3, "only the three alive nodes are counted");

    // Row data lands at the right (positional) row.
    assert_eq!(
        g.node_labels(NodeId::new(20))
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        vec![db_string("nidr.node").unwrap()]
    );

    // --- Allocator floor survives so future ids never collide. ---
    assert_eq!(g.meta.next_node_id, 21);
    assert_eq!(g.meta.next_edge_id, 8);

    // --- Edges: positional placement + adjacency rebuilt from external ids. ---
    assert_eq!(g.edge_row_for_id(EdgeId::new(3)), Some(EdgeRow::new(0)));
    assert_eq!(g.edge_row_for_id(EdgeId::new(7)), Some(EdgeRow::new(2)));
    assert_ne!(g.edge_row_for_id(EdgeId::new(7)), Some(EdgeRow::new(6)));
    assert_eq!(g.edge_id_for_edge_row(EdgeRow::new(1)), None); // hole
    assert_eq!(g.edge_store.len(), 3, "the interior edge hole is preserved");
    assert!(g.is_edge_alive(EdgeId::new(3)));
    assert!(g.is_edge_alive(EdgeId::new(7)));
    assert_eq!(
        g.edge_endpoints(EdgeId::new(3)),
        Some((NodeId::new(5), NodeId::new(8)))
    );
    assert_eq!(
        g.edge_endpoints(EdgeId::new(7)),
        Some((NodeId::new(20), NodeId::new(5)))
    );
    assert!(
        g.outgoing_edges(NodeId::new(20))
            .unwrap()
            .iter()
            .any(|e| e.neighbor == NodeId::new(5) && e.edge_id == EdgeId::new(7))
    );
    assert!(
        g.incoming_edges(NodeId::new(8))
            .unwrap()
            .iter()
            .any(|e| e.neighbor == NodeId::new(5) && e.edge_id == EdgeId::new(3))
    );
}

/// Build a snapshot whose section (row) order is NOT id-ascending: row0->id10,
/// row1->id3, row2->id7. This is the layout a 4b compaction may emit, and it is
/// the case that exercises `insert_node_row`'s `set`-overwrites-a-pad branch
/// (the reason the sorted-by-id wire guarantee was dropped).
fn descending_first_graph() -> SeleneGraph {
    let nlabel = db_string("nidr2.node").unwrap();
    let mut g = SeleneGraph::new(GraphId::new(2));
    for id in [10u64, 3, 7] {
        let row = g.node_store.row_to_id.len() as u32;
        g.node_store.labels.push(LabelSet::single(nlabel.clone()));
        g.node_store.properties.push(PropertyMap::new());
        g.node_store.row_to_id.push(NodeId::new(id));
        g.node_store.alive_mut().insert(row);
    }
    g.meta.next_node_id = 11;
    g
}

#[test]
fn out_of_order_positional_snapshot_round_trips() {
    let dir = temp_dir("nodeid-split-out-of-order");
    let shared = SharedGraph::from_graph(descending_first_graph());
    write_snapshot(&dir, &shared, 1);

    let recovered = SharedGraph::recover(&dir, GraphId::new(2)).expect("recovery succeeds");
    let g = recovered.read();

    // into_graph visits ids ascending (3, 7, 10). id 3@pos1 and 7@pos2 push, then
    // 10@pos0 takes the `set` branch and overwrites the row-0 tombstone pad.
    assert_eq!(g.node_row_for_id(NodeId::new(10)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(3)), Some(NodeRow::new(1)));
    assert_eq!(g.node_row_for_id(NodeId::new(7)), Some(NodeRow::new(2)));
    assert_eq!(
        g.node_id_for_node_row(NodeRow::new(0)),
        Some(NodeId::new(10))
    );
    assert_eq!(
        g.node_id_for_node_row(NodeRow::new(1)),
        Some(NodeId::new(3))
    );
    assert_eq!(
        g.node_id_for_node_row(NodeRow::new(2)),
        Some(NodeId::new(7))
    );
    // The row-0 pad must NOT leave a stale tombstone or dead row behind.
    assert!(g.is_node_alive(NodeId::new(10)));
    assert!(g.is_node_alive(NodeId::new(3)));
    assert!(g.is_node_alive(NodeId::new(7)));
    assert_eq!(g.node_count(), 3);
    assert_eq!(g.node_store.len(), 3, "no leftover pad row");
}

#[test]
fn recovered_store_continues_id_allocation_without_clobber() {
    // Defensive: after recovering the (hole-bearing, len-5) non-identity graph,
    // a fresh create_node must allocate above the persisted high-water mark and
    // land at a new row WITHOUT clobbering any recovered row — the allocator
    // floor (max(meta.next_node_id, store.len()+1)) tracks meta, so a dropped
    // trailing hole could never lower it into the live range.
    let dir = temp_dir("nodeid-split-realloc");
    let shared = SharedGraph::from_graph(non_identity_graph());
    write_snapshot(&dir, &shared, 1);
    let recovered = SharedGraph::recover(&dir, GraphId::new(1)).expect("recovery succeeds");

    let new_id = {
        let mut txn = recovered.begin_write();
        let id = {
            let mut m = txn.mutator();
            m.create_node(
                LabelSet::single(db_string("nidr.node").unwrap()),
                PropertyMap::new(),
            )
            .unwrap()
        };
        txn.commit().unwrap();
        id
    };

    let g = recovered.read();
    assert_eq!(new_id, NodeId::new(21), "allocation resumes at the floor");
    assert!(g.is_node_alive(NodeId::new(21)));
    // The recovered rows are untouched: alive ids still resolve, the deleted id
    // stays mapped-but-dead, the aborted id stays NotFound.
    assert_eq!(g.node_row_for_id(NodeId::new(5)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(20)), Some(NodeRow::new(4)));
    assert!(g.is_node_alive(NodeId::new(5)));
    assert!(!g.is_node_alive(NodeId::new(15)));
    assert!(g.node_row_for_id(NodeId::new(15)).is_some());
    assert!(g.node_row_for_id(NodeId::new(3)).is_none());
}

// --- BRIEF-Item-4c: compacted-snapshot recovery + post-compaction WAL append ---
//
// A COMPACTED snapshot (dense rows, sparse high-water ids) round-trips through
// recovery dense, and a post-compaction WAL `NodeCreated` APPENDS at the dense
// end on replay rather than re-padding the reclaimed holes via `id - 1` (the
// re-bloat regression the recovery-path arith→append change closes). The
// pre-compaction deletes that ride below the snapshot's WAL floor are never
// replayed, so reclaimed ids stay NotFound — also exercising the floor that
// keeps the 4e delete hazard out of the normal rotate path.

/// `sample_shared_graph` builds GraphId(7) with nodes 1..=5 (node 5 deleted) and
/// one edge 1->2. Delete nodes 2 and 3 (deleting 2 cascade-deletes the edge),
/// leaving 1 and 4 alive, then live-densify.
fn compacted_sample() -> SharedGraph {
    let shared = sample_shared_graph();
    {
        let mut txn = shared.begin_write();
        {
            let mut m = txn.mutator();
            m.delete_node(NodeId::new(2)).unwrap();
            m.delete_node(NodeId::new(3)).unwrap();
        }
        txn.commit().unwrap();
    }
    let report = shared.compact().unwrap();
    assert_eq!(report.reclaimed_nodes, 3, "rows 2, 3, 5 reclaimed");
    assert!(
        report.reclaimed_edges >= 1,
        "the cascade-deleted edge reclaimed"
    );
    {
        let g = shared.read();
        assert_eq!(g.node_store.len(), 2, "live store densified to 1 and 4");
        assert_eq!(g.meta.next_node_id, 6, "high-water preserved");
    }
    shared
}

#[test]
fn compacted_snapshot_recovers_dense() {
    let dir = temp_dir("compacted-snapshot-dense");
    let shared = compacted_sample();
    write_snapshot(&dir, &shared, 3);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let g = recovered.read();

    assert_eq!(g.node_store.len(), 2, "recovered store is dense (no holes)");
    assert_eq!(g.node_count(), 2);
    assert!(g.is_node_alive(NodeId::new(1)));
    assert!(g.is_node_alive(NodeId::new(4)));
    // Reclaimed ids are gone from the compacted snapshot -> NotFound.
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
    assert!(g.node_row_for_id(NodeId::new(3)).is_none());
    assert!(g.node_row_for_id(NodeId::new(5)).is_none());
    // The monotonic high-water survives recovery so no id is ever reissued.
    assert_eq!(g.meta.next_node_id, 6);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn post_compaction_wal_create_recovers_dense_without_rebloat() {
    // THE re-bloat regression. After the compacted snapshot at seq 3, a WAL
    // `NodeCreated { id: 6 }` (absent from the dense snapshot) must append at the
    // dense end (row 2), NOT at arith row 5 — which would re-pad rows 2..5 and
    // undo the compaction on every reload.
    let dir = temp_dir("compacted-snapshot-wal-rebloat");
    let shared = compacted_sample();
    write_snapshot(&dir, &shared, 3);
    append_wal(&dir, 3, &[node_created(6)]);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let g = recovered.read();

    assert!(g.is_node_alive(NodeId::new(1)));
    assert!(g.is_node_alive(NodeId::new(4)));
    assert!(
        g.is_node_alive(NodeId::new(6)),
        "the WAL-created node recovered"
    );
    // Pre-compaction deletes rode below the snapshot WAL floor -> never replayed.
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
    assert!(g.node_row_for_id(NodeId::new(3)).is_none());
    assert!(g.node_row_for_id(NodeId::new(5)).is_none());
    // No re-bloat: exactly 3 rows (2 dense survivors + 1 appended WAL node), and
    // the WAL node landed at the dense end — not arith row 5.
    assert_eq!(
        g.node_store.len(),
        3,
        "no hole re-bloat from the WAL-created id"
    );
    assert_eq!(g.node_row_for_id(NodeId::new(6)), Some(NodeRow::new(2)));
    // Allocation continues above the recovered high-water (id 6 + 1).
    assert_eq!(g.meta.next_node_id, 7);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn post_compaction_wal_edge_create_recovers_dense_without_rebloat() {
    // The EDGE arm of the re-bloat regression (symmetric to the node arm above).
    // `compacted_sample` cascade-deleted the original edge, so the compacted
    // snapshot has zero edge rows but next_edge_id == 2. A post-snapshot WAL
    // `EdgeCreated { id: 2 }` between the two surviving nodes must append at edge
    // row 0, NOT pad arith row 1.
    let dir = temp_dir("compacted-snapshot-wal-edge");
    let shared = compacted_sample();
    write_snapshot(&dir, &shared, 3);
    let edge = Change::EdgeCreated {
        id: EdgeId::new(2),
        label: db_string("recover.wal.edge").unwrap(),
        source: NodeId::new(1),
        target: NodeId::new(4),
        properties: PropertyMap::new(),
    };
    append_wal(&dir, 3, &[edge]);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let g = recovered.read();

    assert!(g.is_edge_alive(EdgeId::new(2)));
    assert_eq!(
        g.edge_store.len(),
        1,
        "no edge re-bloat: the WAL edge appended at the dense end"
    );
    assert_eq!(g.edge_row_for_id(EdgeId::new(2)), Some(EdgeRow::new(0)));
    assert_eq!(
        g.edge_endpoints(EdgeId::new(2)),
        Some((NodeId::new(1), NodeId::new(4)))
    );
    // Adjacency rebuilt for the appended edge.
    assert!(
        g.outgoing_edges(NodeId::new(1))
            .unwrap()
            .iter()
            .any(|e| e.edge_id == EdgeId::new(2) && e.neighbor == NodeId::new(4))
    );
    assert_eq!(g.meta.next_edge_id, 3);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn post_compaction_wal_delete_of_survivor_replays_against_compacted_snapshot() {
    // BRIEF-Item-4e (delete arm): the only cross-compaction-epoch WAL replay is of
    // POST-snapshot entries — pre-compaction entries are truncated by the rotate
    // and fenced below the WAL floor, so they never reach a compacted snapshot. A
    // post-snapshot `NodeDeleted` of a SURVIVING node finds it in the compacted
    // snapshot (require_live_node succeeds) and applies; no hard-error, no
    // re-bloat. This is the delete counterpart of the create re-bloat test above.
    let dir = temp_dir("compacted-snapshot-wal-delete");
    let shared = compacted_sample(); // nodes 1 and 4 survive; floor 3
    write_snapshot(&dir, &shared, 3);
    append_wal(&dir, 3, &[Change::NodeDeleted { id: NodeId::new(1) }]); // seq 4 — post-snapshot

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let g = recovered.read();

    assert!(
        !g.is_node_alive(NodeId::new(1)),
        "the post-compaction delete of a survivor applied"
    );
    assert!(
        g.is_node_alive(NodeId::new(4)),
        "the other survivor is untouched"
    );
    // The reclaimed pre-compaction ids stay NotFound throughout.
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
    assert!(g.node_row_for_id(NodeId::new(5)).is_none());
    let _ = std::fs::remove_dir_all(dir);
}
