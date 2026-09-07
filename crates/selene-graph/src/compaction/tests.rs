//! BRIEF-Item-4b CORE compaction tests.
//!
//! Bar ("would this catch the DbString admission race"): the suite drives a graph
//! with a cascade-deleting node delete through `compact_core`, then walks every
//! surviving external id through every read path (labels, properties,
//! endpoints, adjacency, label index) asserting observational equivalence,
//! plus the dense-layout / NotFound-for-reclaimed / preserved-high-water-mark
//! invariants that a remap bug would break.

use std::collections::HashSet;

use selene_core::{DbString, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, Value, db_string};

use super::{
    COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_BASIS_POINTS,
    COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS, CompactionStats, compact_core,
};
use crate::error::GraphError;
use crate::store::{EdgeRow, NodeRow};
use crate::{AdjacencyEntry, CompactionReport, SeleneGraph, SharedGraph};

fn prop(key: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(key).unwrap(), value)]).unwrap()
}

/// Sorted (neighbor, edge_id, label) projection of an adjacency entry, for
/// order-independent equality across the row renumber.
fn adjacency_summary(entry: &AdjacencyEntry) -> Vec<(NodeId, EdgeId, DbString)> {
    let mut edges: Vec<(NodeId, EdgeId, DbString)> = entry
        .iter()
        .map(|edge| (edge.neighbor, edge.edge_id, edge.label.clone()))
        .collect();
    edges.sort_by_key(|(neighbor, edge_id, _)| (*neighbor, *edge_id));
    edges
}

/// 4 nodes (ids 1..=4), 3 edges (ids 1..=3), then `delete_node(2)` which
/// cascade-deletes its incident edge (id 2). Surviving: nodes 1,3,4; edges 1,3.
fn graph_with_a_deletion() -> SharedGraph {
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("cmp.a").unwrap();
    let lb = db_string("cmp.b").unwrap();
    let el = db_string("cmp.e").unwrap();
    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        let n1 = m
            .create_node(LabelSet::single(la.clone()), prop("name", Value::Int(1)))
            .unwrap();
        let n2 = m
            .create_node(LabelSet::single(lb), prop("name", Value::Int(2)))
            .unwrap();
        let n3 = m
            .create_node(LabelSet::single(la.clone()), prop("name", Value::Int(3)))
            .unwrap();
        let n4 = m
            .create_node(LabelSet::single(la), prop("name", Value::Int(4)))
            .unwrap();
        assert_eq!(
            (n1, n2, n3, n4),
            (
                NodeId::new(1),
                NodeId::new(2),
                NodeId::new(3),
                NodeId::new(4)
            )
        );
        m.create_edge(el.clone(), n1, n3, prop("w", Value::Int(10)))
            .unwrap(); // id 1 — survives
        m.create_edge(el.clone(), n1, n2, PropertyMap::new())
            .unwrap(); // id 2 — cascade-deleted
        m.create_edge(el, n3, n4, PropertyMap::new()).unwrap(); // id 3 — survives
        m.delete_node(n2).unwrap();
    }
    txn.commit().unwrap();
    shared
}

#[test]
fn compaction_drops_dead_rows_and_renumbers_dense() {
    let shared = graph_with_a_deletion();
    let before = shared.read();
    let next_node_before = before.meta.next_node_id;
    let next_edge_before = before.meta.next_edge_id;
    let live_nodes = before.node_count();
    let len_before = before.node_store.len();

    let compacted = compact_core(&before).unwrap();
    let g = &compacted.graph;

    // Dense: every row alive, no holes, len == live count.
    assert_eq!(g.node_store.len(), live_nodes);
    assert!(
        len_before > g.node_store.len(),
        "dead rows existed to reclaim"
    );
    assert_eq!(g.node_count(), live_nodes);
    for row in 0..g.node_store.len() as u32 {
        let id = g
            .node_id_for_node_row(NodeRow::new(row))
            .expect("every dense row has a live external id");
        assert!(g.is_node_alive(id), "dense row {row} ({id}) must be alive");
    }
    assert_eq!(
        compacted.report.reclaimed_nodes,
        (len_before - g.node_store.len()) as u64
    );
    assert!(
        compacted.report.reclaimed_edges >= 1,
        "the n1->n2 edge (id 2) was reclaimed"
    );

    // Survivors renumber dense in ascending old-row order: 1@old0->new0,
    // 3@old2->new1, 4@old3->new2 (old row 1, node 2, is gone).
    assert_eq!(g.node_row_for_id(NodeId::new(1)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(3)), Some(NodeRow::new(1)));
    assert_eq!(g.node_row_for_id(NodeId::new(4)), Some(NodeRow::new(2)));

    // The reclaimed deleted id flips NotAlive -> NotFound (the deliberate
    // compaction semantic): pre-compaction it stayed mapped to its dead row;
    // post-compaction the row is gone and the id resolves to nothing.
    assert!(!before.is_node_alive(NodeId::new(2)));
    assert!(
        before.node_row_for_id(NodeId::new(2)).is_some(),
        "pre-compaction: deleted id still mapped (NotAlive, Option B)"
    );
    assert!(
        g.node_row_for_id(NodeId::new(2)).is_none(),
        "post-compaction: deleted id reclaimed (NotFound)"
    );
    assert!(!g.is_node_alive(NodeId::new(2)));

    // Allocator high-water marks preserved verbatim — no external id reuse.
    assert_eq!(g.meta.next_node_id, next_node_before);
    assert_eq!(g.meta.next_edge_id, next_edge_before);
}

#[test]
fn compaction_preserves_observable_reads() {
    let shared = graph_with_a_deletion();
    let before = shared.read();
    let compacted = compact_core(&before).unwrap();
    let g = &compacted.graph;

    for id in [NodeId::new(1), NodeId::new(3), NodeId::new(4)] {
        assert_eq!(g.node_labels(id), before.node_labels(id), "labels {id}");
        assert_eq!(
            g.node_properties(id),
            before.node_properties(id),
            "properties {id}"
        );
        assert_eq!(
            g.outgoing_edges(id).map(adjacency_summary),
            before.outgoing_edges(id).map(adjacency_summary),
            "outgoing adjacency {id}"
        );
        assert_eq!(
            g.incoming_edges(id).map(adjacency_summary),
            before.incoming_edges(id).map(adjacency_summary),
            "incoming adjacency {id}"
        );
    }
    for id in [EdgeId::new(1), EdgeId::new(3)] {
        assert_eq!(
            g.edge_endpoints(id),
            before.edge_endpoints(id),
            "endpoints {id}"
        );
        assert_eq!(g.edge_label(id), before.edge_label(id), "edge label {id}");
        assert_eq!(
            g.edge_properties(id),
            before.edge_properties(id),
            "edge properties {id}"
        );
    }

    // The label index resolves the SAME external ids after the row renumber.
    let la = db_string("cmp.a").unwrap();
    let external_with_label = |graph: &SeleneGraph| -> HashSet<NodeId> {
        graph
            .node_candidates_with_label(&la)
            .unwrap()
            .iter()
            .collect()
    };
    assert_eq!(external_with_label(g), external_with_label(&before));
    assert_eq!(
        external_with_label(g),
        HashSet::from([NodeId::new(1), NodeId::new(3), NodeId::new(4)])
    );
}

#[test]
fn compacted_graph_republishes_cleanly() {
    // The compacted graph is a valid publishable graph: SharedGraph::from_graph
    // re-runs the full rebuild + (debug) consistency assertion, and every read
    // survives the round-trip. This is the 4c-readiness proof.
    let shared = graph_with_a_deletion();
    let compacted = compact_core(&shared.read()).unwrap();
    let republished = SharedGraph::from_graph(compacted.graph);
    let g = republished.read();

    assert_eq!(g.node_count(), 3);
    assert_eq!(g.edge_count(), 2);
    assert!(g.is_node_alive(NodeId::new(1)));
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
    assert_eq!(
        g.edge_endpoints(EdgeId::new(1)),
        Some((NodeId::new(1), NodeId::new(3)))
    );
    assert_eq!(
        g.edge_endpoints(EdgeId::new(3)),
        Some((NodeId::new(3), NodeId::new(4)))
    );
}

#[test]
fn compacting_a_dense_graph_is_idempotent() {
    // A graph with no dead rows compacts to an observationally identical graph
    // (same ids, same dense rows) and reclaims nothing.
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("cmp.idem").unwrap();
    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        m.create_node(LabelSet::single(la.clone()), PropertyMap::new())
            .unwrap();
        m.create_node(LabelSet::single(la), PropertyMap::new())
            .unwrap();
    }
    txn.commit().unwrap();

    let before = shared.read();
    let compacted = compact_core(&before).unwrap();
    let g = &compacted.graph;
    assert_eq!(compacted.report, CompactionReport::default());
    assert_eq!(g.node_store.len(), before.node_store.len());
    // Every external id keeps its exact row (nothing to renumber), asserted per
    // id against the explicit dense layout — not by a single survivor that
    // happened not to move.
    for id in [NodeId::new(1), NodeId::new(2)] {
        assert_eq!(
            g.node_row_for_id(id),
            before.node_row_for_id(id),
            "dense graph: {id} must keep its row"
        );
    }
    assert_eq!(g.node_row_for_id(NodeId::new(1)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(2)), Some(NodeRow::new(1)));
    assert_eq!(g.meta.next_node_id, before.meta.next_node_id);
}

#[test]
fn compaction_of_empty_graph_is_a_clean_noop() {
    let shared = SharedGraph::new(GraphId::new(1));
    let before = shared.read();
    let compacted = compact_core(&before).unwrap();

    assert_eq!(compacted.graph.node_store.len(), 0);
    assert_eq!(compacted.graph.edge_store.len(), 0);
    assert_eq!(compacted.graph.node_count(), 0);
    assert_eq!(compacted.report, CompactionReport::default());
    // High-water marks of a virgin allocator carry through untouched.
    assert_eq!(compacted.graph.meta.next_node_id, before.meta.next_node_id);
    // An empty compacted graph is still a valid publishable graph.
    let _ = SharedGraph::from_graph(compacted.graph);
}

#[test]
fn compaction_stats_reclaimable_ratio_uses_allocated_rows() {
    let stats = CompactionStats {
        allocated_nodes: 800,
        live_nodes: 600,
        reclaimable_nodes: 200,
        allocated_edges: 200,
        live_edges: 100,
        reclaimable_edges: 100,
    };

    assert_eq!(stats.allocated_rows(), 1_000);
    assert_eq!(stats.reclaimable_rows(), 300);
    assert_eq!(stats.reclaimable_row_basis_points(), 3_000);
}

#[test]
fn compaction_recommendation_requires_dead_row_floor_and_ratio() {
    let below_floor = CompactionStats {
        allocated_nodes: COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS * 2,
        live_nodes: COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS + 1,
        reclaimable_nodes: COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS - 1,
        ..CompactionStats::default()
    };
    let below_ratio = CompactionStats {
        allocated_nodes: 100_000,
        live_nodes: 100_000 - COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS,
        reclaimable_nodes: COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS,
        ..CompactionStats::default()
    };
    let recommended = CompactionStats {
        allocated_nodes: COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS * 4,
        live_nodes: COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS * 3,
        reclaimable_nodes: COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS,
        ..CompactionStats::default()
    };

    assert_eq!(CompactionStats::default().reclaimable_row_basis_points(), 0);
    assert!(!below_floor.compaction_recommended());
    assert!(!below_ratio.compaction_recommended());
    assert_eq!(
        recommended.reclaimable_row_basis_points(),
        COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_BASIS_POINTS
    );
    assert!(recommended.compaction_recommended());
}

#[test]
fn compaction_of_an_all_deleted_graph_reclaims_everything() {
    // Every row dead: compaction reclaims all of them, the result is empty, and
    // the allocator high-water marks are still preserved so the deleted ids are
    // never reissued by a later create.
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("cmp.alldel").unwrap();
    let el = db_string("cmp.alldele").unwrap();
    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        let n1 = m
            .create_node(LabelSet::single(la.clone()), PropertyMap::new())
            .unwrap();
        let n2 = m
            .create_node(LabelSet::single(la), PropertyMap::new())
            .unwrap();
        m.create_edge(el, n1, n2, PropertyMap::new()).unwrap();
        m.delete_node(n1).unwrap(); // cascade-deletes the edge
        m.delete_node(n2).unwrap();
    }
    txn.commit().unwrap();

    let before = shared.read();
    assert_eq!(before.node_count(), 0);
    let rows_before = before.node_store.len() as u64;
    let next_node_before = before.meta.next_node_id;
    let next_edge_before = before.meta.next_edge_id;

    let compacted = compact_core(&before).unwrap();
    let g = &compacted.graph;

    assert_eq!(g.node_store.len(), 0);
    assert_eq!(g.edge_store.len(), 0);
    assert_eq!(g.node_count(), 0);
    assert_eq!(compacted.report.reclaimed_nodes, rows_before);
    assert!(compacted.report.reclaimed_edges >= 1);
    assert!(g.node_row_for_id(NodeId::new(1)).is_none());
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
    // No id reuse: the deleted ids stay burned.
    assert_eq!(g.meta.next_node_id, next_node_before);
    assert_eq!(g.meta.next_edge_id, next_edge_before);
    let _ = SharedGraph::from_graph(compacted.graph);
}

#[test]
fn aborted_tx_leaves_no_hole_so_store_stays_dense() {
    // BRIEF-Item-4c: with append-based creation an aborted txn burns its id but
    // materializes NO row (the txn clone is discarded; the next committed create
    // appends at the dense end rather than padding `id - 1`). So the canonical
    // store never grows an interior TOMBSTONE hole for a burned id — n1 and n3
    // land at rows 0 and 1 with NO gap. The burned id is still NotFound, the
    // high-water is still preserved, and compaction has nothing to reclaim.
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("cmp.hole").unwrap();

    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        let n1 = m
            .create_node(LabelSet::single(la.clone()), PropertyMap::new())
            .unwrap();
        assert_eq!(n1, NodeId::new(1));
    }
    txn.commit().unwrap();

    // Aborted txn: allocate id 2, then drop without commit — id burned, no row.
    {
        let mut txn = shared.begin_write();
        let mut m = txn.mutator();
        let n2 = m
            .create_node(LabelSet::single(la.clone()), PropertyMap::new())
            .unwrap();
        assert_eq!(n2, NodeId::new(2));
        // txn dropped here, uncommitted.
    }

    // The next committed create appends — id 3 lands at row 1 (NOT row 2): no
    // hole is padded for the burned id 2.
    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        let n3 = m
            .create_node(LabelSet::single(la), PropertyMap::new())
            .unwrap();
        assert_eq!(n3, NodeId::new(3));
    }
    txn.commit().unwrap();

    let before = shared.read();
    assert_eq!(before.node_count(), 2, "n1 + n3 alive; id 2 burned");
    assert_eq!(
        before.node_store.len(),
        2,
        "append leaves NO interior hole row for the burned id"
    );
    assert_eq!(
        before.node_row_for_id(NodeId::new(1)),
        Some(NodeRow::new(0))
    );
    assert_eq!(
        before.node_row_for_id(NodeId::new(3)),
        Some(NodeRow::new(1)),
        "id 3 appended at the dense end, not arith row 2"
    );
    assert!(
        before.node_row_for_id(NodeId::new(2)).is_none(),
        "aborted id was never committed -> NotFound"
    );
    let next_node_before = before.meta.next_node_id;

    // The store is already dense, so compaction reclaims nothing.
    let compacted = compact_core(&before).unwrap();
    let g = &compacted.graph;

    assert_eq!(g.node_store.len(), 2);
    assert_eq!(g.node_count(), 2);
    assert_eq!(compacted.report.reclaimed_nodes, 0, "nothing to reclaim");
    assert_eq!(g.node_row_for_id(NodeId::new(1)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(3)), Some(NodeRow::new(1)));
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
    assert_eq!(
        g.meta.next_node_id, next_node_before,
        "burned id 2 must never be reissued"
    );
}

#[test]
fn republished_compacted_graph_allocates_without_reuse() {
    // The 4c-critical property: after a compacted graph goes live, the very next
    // create must allocate the PRESERVED next_node_id (not reuse a reclaimed id,
    // not alias a survivor's renumbered row). With BRIEF-Item-4c append creation
    // the new node lands at the dense end (row 3 here) rather than its high-water
    // arith row, so the store does NOT re-bloat — proving append keeps a
    // republished compacted graph dense.
    let shared = graph_with_a_deletion();
    let next_node_before = shared.read().meta.next_node_id;

    let compacted = compact_core(&shared.read()).unwrap();
    let republished = SharedGraph::from_graph(compacted.graph);

    let new_id = {
        let mut txn = republished.begin_write();
        let id = {
            let mut m = txn.mutator();
            m.create_node(
                LabelSet::single(db_string("cmp.a").unwrap()),
                PropertyMap::new(),
            )
            .unwrap()
        };
        txn.commit().unwrap();
        id
    };

    let g = republished.read();
    assert_eq!(
        new_id,
        NodeId::new(next_node_before),
        "the next create must take the preserved high-water mark, not a reclaimed id"
    );
    assert!(g.is_node_alive(new_id));
    // Dense append: the new node took the next dense row (3), NOT its high-water
    // arith row (4) — so the republished compacted store did not re-bloat.
    assert_eq!(g.node_row_for_id(new_id), Some(NodeRow::new(3)));
    assert_eq!(g.node_store.len(), 4, "store stays dense, no hole re-bloat");
    // Survivors untouched by the new allocation.
    assert!(g.is_node_alive(NodeId::new(1)));
    assert!(g.is_node_alive(NodeId::new(3)));
    assert!(g.is_node_alive(NodeId::new(4)));
    // The reclaimed deleted id is not resurrected by the new create.
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
}

// ───────────────────────── GRAPH-37: fail-loud guards ─────────────────────────
//
// `compact_core` has three `GraphError::Inconsistent` guards that only fire on a
// corrupt (raw-column-built) graph the funnel can never produce: an alive node
// row with no mapped external id, an alive edge row with no mapped id, and an
// edge that survives compaction whose endpoint node does not. The funnel's
// delete-node cascade guarantees the last can't happen for engine graphs, so
// these are tested by hand-building the corrupt store. A regression that
// silently dropped a dangling edge (instead of failing loud) would rebuild
// adjacency keyed by a vanished node — exactly the corruption these reject.

/// A single alive node (id 1, row 0) with a valid id↔row binding, ready to have
/// a corrupt edge attached.
fn graph_with_one_live_node() -> SeleneGraph {
    let mut graph = SeleneGraph::new(GraphId::new(1));
    graph
        .node_store
        .labels
        .push(LabelSet::single(db_string("cmp.live").unwrap()));
    graph.node_store.properties.push(PropertyMap::new());
    graph.node_store.row_to_id.push(NodeId::new(1));
    graph.node_store.alive_mut().insert(0);
    graph.node_rows.insert_cow(NodeId::new(1), NodeRow::new(0));
    graph
}

/// Run `compact_core` expecting an [`GraphError::Inconsistent`], returning it.
/// Used instead of `Result::expect_err` because the `Ok` variant
/// (`CompactedCore`) is intentionally not `Debug` (it holds a whole graph).
fn expect_compact_inconsistent(graph: &SeleneGraph) -> GraphError {
    match compact_core(graph) {
        Ok(_) => panic!("compaction should have failed loudly on a corrupt graph"),
        Err(err) => {
            assert!(
                matches!(err, GraphError::Inconsistent { .. }),
                "expected GraphError::Inconsistent, got {err:?}",
            );
            err
        }
    }
}

#[test]
fn compaction_rejects_edge_with_dead_endpoint() {
    // Alive edge (row 0) from the live node (1) to a node id (99) that is NOT in
    // the alive set: the edge "survives compaction but its endpoint does not".
    let mut graph = graph_with_one_live_node();
    graph
        .edge_store
        .label
        .push(db_string("cmp.dangling").unwrap());
    graph.edge_store.source.push(NodeId::new(1));
    graph.edge_store.target.push(NodeId::new(99)); // dead endpoint
    graph.edge_store.properties.push(PropertyMap::new());
    graph.edge_store.row_to_id.push(EdgeId::new(1));
    graph.edge_store.alive_mut().insert(0);
    graph.edge_rows.insert_cow(EdgeId::new(1), EdgeRow::new(0));

    let GraphError::Inconsistent { reason } = expect_compact_inconsistent(&graph) else {
        unreachable!("helper returns only Inconsistent");
    };
    assert!(
        reason.contains("survives compaction but endpoint"),
        "expected dead-endpoint rejection, got: {reason}",
    );
}

#[test]
fn compaction_rejects_alive_node_row_with_no_external_id() {
    // An alive node row whose `row_to_id` holds the TOMBSTONE sentinel resolves to
    // no external id — a corrupt id↔row mapping that must fail loud rather than
    // rebuild a dense store with a phantom row.
    let mut graph = SeleneGraph::new(GraphId::new(1));
    graph
        .node_store
        .labels
        .push(LabelSet::single(db_string("cmp.noid").unwrap()));
    graph.node_store.properties.push(PropertyMap::new());
    graph.node_store.row_to_id.push(NodeId::TOMBSTONE); // alive but no id
    graph.node_store.alive_mut().insert(0);

    let GraphError::Inconsistent { reason } = expect_compact_inconsistent(&graph) else {
        unreachable!("helper returns only Inconsistent");
    };
    assert!(
        reason.contains("alive node row") && reason.contains("no external id"),
        "expected missing-node-id rejection, got: {reason}",
    );
}

#[test]
fn compaction_rejects_alive_edge_row_with_no_external_id() {
    // The edge-side missing-id guard: an alive edge row whose `row_to_id` holds
    // the TOMBSTONE sentinel (endpoints are the live node so the dead-endpoint
    // guard is not what fires).
    let mut graph = graph_with_one_live_node();
    graph
        .edge_store
        .label
        .push(db_string("cmp.noid.edge").unwrap());
    graph.edge_store.source.push(NodeId::new(1));
    graph.edge_store.target.push(NodeId::new(1));
    graph.edge_store.properties.push(PropertyMap::new());
    graph.edge_store.row_to_id.push(EdgeId::TOMBSTONE); // alive but no id
    graph.edge_store.alive_mut().insert(0);

    let GraphError::Inconsistent { reason } = expect_compact_inconsistent(&graph) else {
        unreachable!("helper returns only Inconsistent");
    };
    assert!(
        reason.contains("alive edge row") && reason.contains("no external id"),
        "expected missing-edge-id rejection, got: {reason}",
    );
}
