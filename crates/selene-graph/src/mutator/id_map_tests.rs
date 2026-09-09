//! BRIEF-Item-4a external-id <-> [`RowIndex`](crate::store::RowIndex) mapping
//! tests, kept separate from `mutator/tests.rs` to stay under the 700-LOC cap.
//!
//! Increment 2 lands the population divergence guard; Increment 6 adds the
//! non-identity proof test (a manually constructed map where `id != row + 1`).

use selene_core::{EdgeId, GraphId, LabelSet, NodeId, PropertyMap, db_string};

use crate::store::{EdgeRow, NodeRow};
use crate::{SeleneGraph, SharedGraph};

#[test]
fn id_row_maps_round_trip_for_all_alive() {
    // The populated id<->row maps and per-store `row_to_id` columns must
    // round-trip for every ALIVE node and edge, and a deleted id must stay mapped
    // (so a later map-backed read yields NotAlive, not NotFound). BRIEF-Item-4c
    // made creation APPEND-based, so `row == id - 1` is no longer an invariant the
    // engine maintains — the durable contract is purely that the bidirectional
    // map agrees with the `row_to_id` column. (For this sequential-create graph
    // the rows happen to still be 0,1,2, but the test asserts the map, not the
    // arithmetic.) Bar: "would this catch the DbString admission race" — it walks
    // every alive row through both directions plus the delete-tombstone path.
    let shared = SharedGraph::new(GraphId::new(1));
    let a = selene_core::db_string("inc2.a").unwrap();
    let b = selene_core::db_string("inc2.b").unwrap();
    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        let n0 = m
            .create_node(LabelSet::single(a.clone()), PropertyMap::new())
            .unwrap();
        let n1 = m
            .create_node(LabelSet::single(a.clone()), PropertyMap::new())
            .unwrap();
        let n2 = m
            .create_node(LabelSet::single(b), PropertyMap::new())
            .unwrap();
        // e0/e1 are incident to n1 (cascade-deleted); e2 (n0->n2) survives.
        m.create_edge(a.clone(), n0, n1, PropertyMap::new())
            .unwrap();
        m.create_edge(a.clone(), n1, n2, PropertyMap::new())
            .unwrap();
        m.create_edge(a, n0, n2, PropertyMap::new()).unwrap();
        m.delete_node(n1).unwrap();
    }
    txn.commit().unwrap();
    let g = shared.read();

    // Every alive node row round-trips through the bidirectional map.
    for row in g.node_store.alive.iter() {
        let id = *g
            .node_store
            .row_to_id
            .get(row as usize)
            .expect("row_to_id in-bounds");
        assert_ne!(
            id,
            NodeId::TOMBSTONE,
            "alive node row {row} has tombstone id"
        );
        assert_eq!(
            g.node_rows.get(&id).copied(),
            Some(NodeRow::new(row)),
            "id->row map disagrees with row_to_id for alive {id}"
        );
    }
    // n1 == row 1 == NodeId(2): deleted, but its id stays mapped to the dead row
    // AND its row_to_id slot keeps the real id (Option B) so the snapshot/STEP-9
    // encoder persists the dead row's id and recovery rebuilds NotAlive (not
    // NotFound) for it.
    assert!(!g.node_store.alive.contains(1));
    assert_eq!(
        g.node_rows.get(&NodeId::new(2)).copied(),
        Some(NodeRow::new(1)),
        "deleted node id must stay mapped (NotAlive, not NotFound)"
    );
    assert_eq!(
        *g.node_store.row_to_id.get(1).unwrap(),
        NodeId::new(2),
        "deleted row keeps its real external id in row_to_id"
    );

    // Every alive edge row round-trips; e2 (row 2 == EdgeId(3)) is the survivor.
    for row in g.edge_store.alive.iter() {
        let id = *g
            .edge_store
            .row_to_id
            .get(row as usize)
            .expect("row_to_id in-bounds");
        assert_ne!(
            id,
            selene_core::EdgeId::TOMBSTONE,
            "alive edge row {row} has tombstone id"
        );
        assert_eq!(
            g.edge_rows.get(&id).copied(),
            Some(EdgeRow::new(row)),
            "id->row map disagrees with row_to_id for alive {id}"
        );
    }
    assert!(g.edge_store.alive.contains(2));
    // Cascade-deleted edge ids stay mapped to their dead rows, and their
    // row_to_id slots keep the real id (Option B), same as deleted nodes.
    assert_eq!(
        g.edge_rows.get(&selene_core::EdgeId::new(1)).copied(),
        Some(EdgeRow::new(0))
    );
    assert_eq!(
        g.edge_rows.get(&selene_core::EdgeId::new(2)).copied(),
        Some(EdgeRow::new(1))
    );
    assert_eq!(
        *g.edge_store.row_to_id.get(0).unwrap(),
        selene_core::EdgeId::new(1)
    );
    assert_eq!(
        *g.edge_store.row_to_id.get(1).unwrap(),
        selene_core::EdgeId::new(2)
    );

    // The row_to_id columns track the row columns exactly (length-locked).
    assert_eq!(g.node_store.row_to_id.len(), g.node_store.len());
    assert_eq!(g.edge_store.row_to_id.len(), g.edge_store.len());
}

#[test]
fn non_identity_map_read_paths_resolve_by_map() {
    // BRIEF-Item-4a Increment 6 — THE proof. Hand-build a graph whose external
    // ids are deliberately NOT `row + 1` (NodeId 5 @ row 0, NodeId 8 @ row 1,
    // EdgeId 3 @ row 0) and feed it through SharedGraph::from_graph, whose
    // rebuild_id_maps seeds the maps from the row_to_id column — so the whole
    // read stack runs against a genuine non-identity mapping (a preview of what
    // 4b compaction will produce). Every read path must resolve by the map, not
    // by arithmetic; the arithmetic answers are asserted to be WRONG. This is the
    // assertion that would catch any read site Increment 3 failed to migrate.
    let label = selene_core::db_string("ni.node").unwrap();
    let elabel = selene_core::db_string("ni.edge").unwrap();

    let mut built = SeleneGraph::new(GraphId::new(1));
    // Row 0 -> NodeId(5); Row 1 -> NodeId(8).
    built
        .node_store
        .labels
        .push(LabelSet::single(label.clone()));
    built.node_store.properties.push(PropertyMap::new());
    built.node_store.row_to_id.push(NodeId::new(5));
    built
        .node_store
        .labels
        .push(LabelSet::single(label.clone()));
    built.node_store.properties.push(PropertyMap::new());
    built.node_store.row_to_id.push(NodeId::new(8));
    built.node_store.alive_mut().insert(0);
    built.node_store.alive_mut().insert(1);
    // Row 0 -> EdgeId(3): NodeId(5) -> NodeId(8).
    built.edge_store.label.push(elabel.clone());
    built.edge_store.source.push(NodeId::new(5));
    built
        .edge_store
        .directionality
        .push(selene_core::EdgeDirectionality::Directed);
    built.edge_store.target.push(NodeId::new(8));
    built.edge_store.properties.push(PropertyMap::new());
    built.edge_store.row_to_id.push(EdgeId::new(3));
    built.edge_store.alive_mut().insert(0);
    built.meta.next_node_id = 9;
    built.meta.next_edge_id = 4;

    let shared = SharedGraph::from_graph(built);
    let g = shared.read();

    // Accessors round-trip the NON-identity mapping (seeded from row_to_id).
    assert_eq!(g.node_row_for_id(NodeId::new(5)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(8)), Some(NodeRow::new(1)));
    assert_eq!(
        g.node_id_for_node_row(NodeRow::new(0)),
        Some(NodeId::new(5))
    );
    assert_eq!(
        g.node_id_for_node_row(NodeRow::new(1)),
        Some(NodeId::new(8))
    );
    // The arithmetic answer (NodeId 5 -> row 4) is WRONG; the map says row 0.
    assert_ne!(g.node_row_for_id(NodeId::new(5)), Some(NodeRow::new(4)));

    // Liveness + labels resolve by the map. NodeId(1) was never committed: under
    // the old `id - 1` arithmetic it would map to row 0 and return row 0's label;
    // the map returns None. THIS is the decoupling proof.
    assert!(g.is_node_alive(NodeId::new(5)));
    assert!(g.is_node_alive(NodeId::new(8)));
    assert!(!g.is_node_alive(NodeId::new(1)));
    assert!(g.node_labels(NodeId::new(1)).is_none());
    assert_eq!(
        g.node_labels(NodeId::new(5))
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        vec![label.clone()]
    );
    // The label index is row-keyed: rows 0 and 1 carry the label.
    let labelled = g.nodes_with_label(&label).unwrap();
    assert!(labelled.contains(0) && labelled.contains(1));

    // Edge read paths + adjacency (rebuilt reading edge_id from row_to_id).
    assert_eq!(g.edge_row_for_id(EdgeId::new(3)), Some(EdgeRow::new(0)));
    assert_eq!(
        g.edge_id_for_edge_row(EdgeRow::new(0)),
        Some(EdgeId::new(3))
    );
    assert!(g.is_edge_alive(EdgeId::new(3)));
    assert!(!g.is_edge_alive(EdgeId::new(1)));
    assert_eq!(
        g.edge_endpoints(EdgeId::new(3)),
        Some((NodeId::new(5), NodeId::new(8)))
    );
    assert_eq!(g.edge_label(EdgeId::new(3)).unwrap(), &elabel);
    assert!(
        g.outgoing_edges(NodeId::new(5))
            .unwrap()
            .iter()
            .any(|e| e.neighbor == NodeId::new(8) && e.edge_id == EdgeId::new(3))
    );
    assert!(
        g.incoming_edges(NodeId::new(8))
            .unwrap()
            .iter()
            .any(|e| e.neighbor == NodeId::new(5) && e.edge_id == EdgeId::new(3))
    );
}

#[test]
fn create_node_past_u32_id_space_succeeds_with_append_rows() {
    // BRIEF-Item-4c decoupled id-space (u64) from row-space (u32). A graph whose
    // monotonic high-water id is already past u32::MAX (e.g. after heavy churn +
    // compaction) can still create nodes: the new row is APPENDED at the dense
    // end, not derived from the huge id. (Pre-4c this returned an id-overflow
    // error because row = id - 1; the row cap — now RowSpaceExhausted — only
    // triggers at 2^32 live rows.)
    let mut graph = SeleneGraph::new(GraphId::new(1));
    graph.meta.next_node_id = u32::MAX as u64 + 2;
    let shared = SharedGraph::from_graph(graph);
    let id = {
        let mut txn = shared.begin_write();
        let id = txn
            .mutator()
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create past u32 id-space succeeds with append rows");
        txn.commit().unwrap();
        id
    };
    assert_eq!(id.get(), u32::MAX as u64 + 2);
    let g = shared.read();
    // Row appended at the dense end (0), NOT the huge arith row.
    assert_eq!(g.node_row_for_id(id).map(|r| r.get()), Some(0));
    assert!(g.is_node_alive(id));
}

#[test]
fn create_edge_past_u32_id_space_succeeds_with_append_rows() {
    // Edge counterpart of the node test above (id-space u64 vs row-space u32).
    let mut graph = SeleneGraph::new(GraphId::new(1));
    graph.meta.next_edge_id = u32::MAX as u64 + 2;
    let shared = SharedGraph::from_graph(graph);
    let edge_id = {
        let mut txn = shared.begin_write();
        let id = {
            let mut mutator = txn.mutator();
            let source = mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .expect("create_node ok");
            let target = mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .expect("create_node ok");
            mutator
                .create_edge(
                    db_string("edge.overflow").unwrap(),
                    source,
                    target,
                    PropertyMap::new(),
                )
                .expect("create edge past u32 id-space succeeds with append rows")
        };
        txn.commit().unwrap();
        id
    };
    assert_eq!(edge_id.get(), u32::MAX as u64 + 2);
    let g = shared.read();
    assert_eq!(g.edge_row_for_id(edge_id).map(|r| r.get()), Some(0));
    assert!(g.is_edge_alive(edge_id));
}
