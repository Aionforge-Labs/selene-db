//! Recovery-path tests for `IM_TRUNCATE` (BRIEF-150, audit Item 11).
//!
//! Invariants under test:
//! - Recovering a WAL containing ONE declarative `NodesOfTypeTruncated{L}`
//!   reconstructs the IDENTICAL alive-node/alive-edge state as recovering a WAL
//!   containing the equivalent N `NodeDeleted` + incident `EdgeDeleted`
//!   ("replay walks the store").

use selene_core::{Change, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, db_string};

use super::{append_wal, temp_dir};
use crate::{SeleneGraph, SharedGraph};

fn node_created(id: u64, label: &str) -> Change {
    Change::NodeCreated {
        id: NodeId::new(id),
        labels: LabelSet::single(db_string(label).unwrap()),
        properties: PropertyMap::new(),
    }
}

fn edge_created(id: u64, source: u64, target: u64, label: &str) -> Change {
    Change::EdgeCreated {
        directionality: selene_core::EdgeDirectionality::Directed,
        id: EdgeId::new(id),
        label: db_string(label).unwrap(),
        source: NodeId::new(source),
        target: NodeId::new(target),
        properties: PropertyMap::new(),
    }
}

/// The shared create-section of both scenarios: 3 nodes of :L (ids 1..3), 1
/// keep node (id 4), and edges incident to the :L nodes plus a survivor edge.
fn base_creates() -> Vec<Change> {
    vec![
        node_created(1, "trec.L"),
        node_created(2, "trec.L"),
        node_created(3, "trec.L"),
        node_created(4, "trec.Keep"),
        edge_created(1, 1, 2, "trec.E1"),
        edge_created(2, 2, 3, "trec.E2"),
        edge_created(3, 3, 4, "trec.E1"),
        edge_created(4, 4, 1, "trec.E2"),
        edge_created(5, 4, 4, "trec.E1"), // survivor: both endpoints keep
    ]
}

fn assert_same_observable_state(a: &SeleneGraph, b: &SeleneGraph) {
    assert_eq!(a.node_store.alive, b.node_store.alive, "alive nodes differ");
    assert_eq!(a.edge_store.alive, b.edge_store.alive, "alive edges differ");
    assert_eq!(a.idx_label, b.idx_label, "node label index differs");
    assert_eq!(
        a.idx_edge_label, b.idx_edge_label,
        "edge label index differs"
    );
    assert_eq!(
        a.adjacency_out, b.adjacency_out,
        "outgoing adjacency differs"
    );
    assert_eq!(a.adjacency_in, b.adjacency_in, "incoming adjacency differs");
}

#[test]
fn recovery_of_declarative_truncate_matches_expanded_form() {
    // Scenario A: WAL ends with the single declarative truncate change.
    let dir_a = temp_dir("trec-declarative");
    let mut changes_a = base_creates();
    changes_a.push(Change::NodesOfTypeTruncated {
        label: db_string("trec.L").unwrap(),
    });
    append_wal(&dir_a, 0, &changes_a);
    let recovered_a = SharedGraph::recover(&dir_a, GraphId::new(7)).unwrap();

    // Scenario B: WAL ends with the equivalent expanded per-row tombstones
    // (the :L nodes 1..3 and their incident edges 1,2,3,4 — NOT survivor 5).
    let dir_b = temp_dir("trec-expanded");
    let mut changes_b = base_creates();
    changes_b.push(Change::NodeDeleted { id: NodeId::new(1) });
    changes_b.push(Change::NodeDeleted { id: NodeId::new(2) });
    changes_b.push(Change::NodeDeleted { id: NodeId::new(3) });
    changes_b.push(Change::EdgeDeleted { id: EdgeId::new(1) });
    changes_b.push(Change::EdgeDeleted { id: EdgeId::new(2) });
    changes_b.push(Change::EdgeDeleted { id: EdgeId::new(3) });
    changes_b.push(Change::EdgeDeleted { id: EdgeId::new(4) });
    append_wal(&dir_b, 0, &changes_b);
    let recovered_b = SharedGraph::recover(&dir_b, GraphId::new(7)).unwrap();

    assert_same_observable_state(&recovered_a.read(), &recovered_b.read());

    // Concrete shape: only the keep node and survivor edge remain, no dangling.
    let g = recovered_a.read();
    assert!(g.is_node_alive(NodeId::new(4)));
    for id in [1_u64, 2, 3] {
        assert!(!g.is_node_alive(NodeId::new(id)), "node {id} must be dead");
    }
    assert!(g.is_edge_alive(EdgeId::new(5)), "survivor edge stays alive");
    for id in [1_u64, 2, 3, 4] {
        assert!(!g.is_edge_alive(EdgeId::new(id)), "edge {id} must be dead");
    }
    let _ = std::fs::remove_dir_all(dir_a);
    let _ = std::fs::remove_dir_all(dir_b);
}

#[test]
fn recovery_of_edge_type_truncate_matches_expanded_form() {
    let dir_a = temp_dir("trec-edge-declarative");
    let mut changes_a = base_creates();
    changes_a.push(Change::EdgesOfTypeTruncated {
        label: db_string("trec.E1").unwrap(),
    });
    append_wal(&dir_a, 0, &changes_a);
    let recovered_a = SharedGraph::recover(&dir_a, GraphId::new(7)).unwrap();

    let dir_b = temp_dir("trec-edge-expanded");
    let mut changes_b = base_creates();
    // E1 edges are ids 1, 3, 5.
    changes_b.push(Change::EdgeDeleted { id: EdgeId::new(1) });
    changes_b.push(Change::EdgeDeleted { id: EdgeId::new(3) });
    changes_b.push(Change::EdgeDeleted { id: EdgeId::new(5) });
    append_wal(&dir_b, 0, &changes_b);
    let recovered_b = SharedGraph::recover(&dir_b, GraphId::new(7)).unwrap();

    assert_same_observable_state(&recovered_a.read(), &recovered_b.read());
    let g = recovered_a.read();
    assert_eq!(g.node_count(), 4, "edge truncate leaves all nodes alive");
    assert!(g.edges_with_label(&db_string("trec.E1").unwrap()).is_none());
    let _ = std::fs::remove_dir_all(dir_a);
    let _ = std::fs::remove_dir_all(dir_b);
}

#[test]
fn recovery_of_graph_reset_matches_expanded_form() {
    let dir_a = temp_dir("trec-reset-declarative");
    let mut changes_a = base_creates();
    changes_a.push(Change::GraphReset {});
    append_wal(&dir_a, 0, &changes_a);
    let recovered_a = SharedGraph::recover(&dir_a, GraphId::new(7)).unwrap();

    let dir_b = temp_dir("trec-reset-expanded");
    let mut changes_b = base_creates();
    for id in 1_u64..=4 {
        changes_b.push(Change::NodeDeleted {
            id: NodeId::new(id),
        });
    }
    for id in 1_u64..=5 {
        changes_b.push(Change::EdgeDeleted {
            id: EdgeId::new(id),
        });
    }
    append_wal(&dir_b, 0, &changes_b);
    let recovered_b = SharedGraph::recover(&dir_b, GraphId::new(7)).unwrap();

    assert_same_observable_state(&recovered_a.read(), &recovered_b.read());
    let g = recovered_a.read();
    assert_eq!(g.node_count(), 0, "graph reset wipes every node");
    assert_eq!(g.edge_count(), 0, "graph reset wipes every edge");
    assert!(
        g.nodes_with_label(&db_string("trec.Keep").unwrap())
            .is_none(),
        "label indexes are cleared for reset nodes"
    );
    assert!(
        g.edges_with_label(&db_string("trec.E1").unwrap()).is_none(),
        "edge-label indexes are cleared for reset edges"
    );
    let _ = std::fs::remove_dir_all(dir_a);
    let _ = std::fs::remove_dir_all(dir_b);
}
