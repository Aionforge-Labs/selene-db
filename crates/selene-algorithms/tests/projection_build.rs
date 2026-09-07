//! Integration tests for `GraphProjection::build` covering the §H test bar.

use proptest::prelude::*;
use selene_algorithms::{GraphProjection, ProjectionConfig};
use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value};
use selene_graph::SharedGraph;

fn db_string(name: &str) -> DbString {
    selene_core::db_string(name).unwrap()
}

fn weight_props(value: f64) -> PropertyMap {
    PropertyMap::from_pairs([(db_string("weight"), Value::Float(value))]).unwrap()
}

fn props_with_weight(value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string("weight"), value)]).unwrap()
}

/// Build a small fixture:
///   Person(1) -- friend(weight=2.5) --> Person(2)
///   Person(2) -- friend(weight=4.0) --> Company(3)
///   Company(3) -- employs(weight=missing) --> Person(1)
fn fixture_small() -> (SharedGraph, [NodeId; 3], [DbString; 2]) {
    let shared = SharedGraph::new(GraphId::new(1));
    let person = db_string("Person");
    let company = db_string("Company");
    let friend = db_string("friend");
    let employs = db_string("employs");
    let weight = db_string("weight");

    let mut txn = shared.begin_write();
    let n1 = txn
        .mutator()
        .create_node(LabelSet::single(person.clone()), PropertyMap::new())
        .unwrap();
    let n2 = txn
        .mutator()
        .create_node(LabelSet::single(person), PropertyMap::new())
        .unwrap();
    let n3 = txn
        .mutator()
        .create_node(LabelSet::single(company), PropertyMap::new())
        .unwrap();

    txn.mutator()
        .create_edge(friend.clone(), n1, n2, weight_props(2.5))
        .unwrap();
    txn.mutator()
        .create_edge(friend.clone(), n2, n3, weight_props(4.0))
        .unwrap();
    // Edge with no weight property — exercises §O.2 default-to-1.0.
    txn.mutator()
        .create_edge(employs.clone(), n3, n1, PropertyMap::new())
        .unwrap();

    txn.commit().unwrap();
    let _ = weight;
    (shared, [n1, n2, n3], [friend, employs])
}

#[test]
fn projection_all_nodes_all_edges() {
    let (shared, nodes, _) = fixture_small();
    let snapshot = shared.read();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "all".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        None,
    )
    .unwrap();

    assert_eq!(proj.node_count(), 3);
    assert_eq!(proj.edge_count(), 3, "all 3 directed edges present");
    assert_eq!(proj.out_neighbors(nodes[0]).len(), 1);
    assert_eq!(proj.in_neighbors(nodes[0]).len(), 1);
    assert!(proj.contains(nodes[0]));
    assert!(!proj.is_weighted());
}

#[test]
fn projection_label_filter() {
    let (shared, nodes, _) = fixture_small();
    let snapshot = shared.read();
    let person = db_string("Person");
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "people".to_string(),
            node_labels: vec![person],
            edge_labels: vec![],
            weight_property: None,
        },
        None,
    )
    .unwrap();

    assert_eq!(proj.node_count(), 2, "only 2 Person nodes");
    assert!(proj.contains(nodes[0]));
    assert!(proj.contains(nodes[1]));
    assert!(!proj.contains(nodes[2]), "Company node excluded");
    // The only edge between Person nodes is n1 -> n2 (friend).
    assert_eq!(proj.edge_count(), 1);
    assert_eq!(proj.out_neighbors(nodes[0])[0].node_id, nodes[1]);
}

#[test]
fn projection_edge_label_filter() {
    let (shared, _, labels) = fixture_small();
    let snapshot = shared.read();
    let [friend, _] = labels;
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "friends".to_string(),
            node_labels: vec![],
            edge_labels: vec![friend.clone()],
            weight_property: None,
        },
        None,
    )
    .unwrap();

    assert_eq!(
        proj.edge_count(),
        2,
        "only 2 friend-typed edges; employs filtered out"
    );
    assert_eq!(proj.edge_labels(), &[friend]);
}

#[test]
fn projection_transpose_invariant_holds_on_directed_dag() {
    let shared = SharedGraph::new(GraphId::new(96_001));
    let label = db_string("N");
    let rel = db_string("R");

    let mut txn = shared.begin_write();
    let mut nodes = Vec::with_capacity(4);
    for _ in 0..4 {
        nodes.push(
            txn.mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap(),
        );
    }
    for (source, target) in [(0, 1), (1, 2), (2, 3)] {
        txn.mutator()
            .create_edge(
                rel.clone(),
                nodes[source],
                nodes[target],
                PropertyMap::new(),
            )
            .unwrap();
    }
    txn.commit().unwrap();

    let snapshot = shared.read();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "directed-dag".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        None,
    )
    .unwrap();

    assert_eq!(proj.node_count(), 4);
    assert_eq!(proj.edge_count(), 3);
    assert_eq!(proj.out_neighbors(nodes[0])[0].node_id, nodes[1]);
    assert_eq!(proj.in_neighbors(nodes[3])[0].node_id, nodes[2]);
}

#[test]
fn projection_transpose_invariant_holds_on_label_filtered_non_reciprocal() {
    let shared = SharedGraph::new(GraphId::new(96_002));
    let label = db_string("N");
    let knows = db_string("KNOWS");
    let owns = db_string("OWNS");

    let mut txn = shared.begin_write();
    let a = txn
        .mutator()
        .create_node(LabelSet::single(label.clone()), PropertyMap::new())
        .unwrap();
    let b = txn
        .mutator()
        .create_node(LabelSet::single(label.clone()), PropertyMap::new())
        .unwrap();
    let c = txn
        .mutator()
        .create_node(LabelSet::single(label.clone()), PropertyMap::new())
        .unwrap();
    let d = txn
        .mutator()
        .create_node(LabelSet::single(label), PropertyMap::new())
        .unwrap();
    txn.mutator()
        .create_edge(knows.clone(), a, b, PropertyMap::new())
        .unwrap();
    txn.mutator()
        .create_edge(owns, c, d, PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();

    let snapshot = shared.read();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "knows-only".to_string(),
            node_labels: vec![],
            edge_labels: vec![knows],
            weight_property: None,
        },
        None,
    )
    .unwrap();

    assert_eq!(proj.node_count(), 4);
    assert_eq!(proj.edge_count(), 1);
    assert_eq!(proj.out_neighbors(a)[0].node_id, b);
    assert_eq!(proj.in_neighbors(b)[0].node_id, a);
    assert!(proj.out_neighbors(b).is_empty(), "KNOWS edge is one-way");
    assert!(
        proj.out_neighbors(c).is_empty(),
        "OWNS edge is filtered out"
    );
    assert!(proj.in_neighbors(d).is_empty(), "OWNS edge is filtered out");
}

#[test]
fn projection_weight_extraction() {
    let (shared, nodes, _) = fixture_small();
    let snapshot = shared.read();
    let weight = db_string("weight");
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "weighted".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: Some(weight),
        },
        None,
    )
    .unwrap();

    assert!(proj.is_weighted());
    let out_n1 = proj.out_neighbors(nodes[0]);
    assert_eq!(out_n1.len(), 1);
    assert_eq!(out_n1[0].weight, 2.5);

    let out_n2 = proj.out_neighbors(nodes[1]);
    assert_eq!(out_n2.len(), 1);
    assert_eq!(out_n2[0].weight, 4.0);
}

#[test]
fn projection_weight_missing_defaults_to_one() {
    // The employs edge n3 -> n1 carries no `weight` property; per spec 16
    // §E04 (and §O.2) the projection MUST default to 1.0, not error.
    let (shared, nodes, _) = fixture_small();
    let snapshot = shared.read();
    let weight = db_string("weight");
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "weighted-mixed".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: Some(weight),
        },
        None,
    )
    .unwrap();

    let out_n3 = proj.out_neighbors(nodes[2]);
    assert_eq!(out_n3.len(), 1, "n3 has one outgoing edge (employs)");
    assert_eq!(
        out_n3[0].weight, 1.0,
        "missing weight property defaults to 1.0 per spec 16 §E04"
    );
    // The other edges still carry their declared weights.
    assert_eq!(proj.out_neighbors(nodes[0])[0].weight, 2.5);
    assert_eq!(proj.out_neighbors(nodes[1])[0].weight, 4.0);
}

#[test]
fn projection_weight_uint_coerced_to_f64() {
    // ALGO-13: a `Value::Uint` weight goes through `extract_weight`'s Uint
    // coercion branch (`*u as f64`), distinct from the Int and Float branches.
    let shared = SharedGraph::new(GraphId::new(13_001));
    let label = db_string("N");
    let rel = db_string("R");
    let weight = db_string("weight");
    let mut txn = shared.begin_write();
    let a = txn
        .mutator()
        .create_node(LabelSet::single(label.clone()), PropertyMap::new())
        .unwrap();
    let b = txn
        .mutator()
        .create_node(LabelSet::single(label), PropertyMap::new())
        .unwrap();
    txn.mutator()
        .create_edge(rel, a, b, props_with_weight(Value::Uint(7)))
        .unwrap();
    txn.commit().unwrap();

    let snapshot = shared.read();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "uint-weight".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: Some(weight),
        },
        None,
    )
    .unwrap();

    let out = proj.out_neighbors(a);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].weight, 7.0,
        "Value::Uint(7) must coerce to 7.0 via the Uint branch"
    );
}

#[test]
fn projection_weight_null_or_non_numeric_defaults_to_one() {
    // ALGO-13: a weight property that IS present but holds Null or a
    // non-numeric value falls through `extract_weight`'s `_ => 1.0` arm. This
    // is a different code path from a *missing* property (the earlier
    // `properties.get(prop)` returns `None`) — here `get` returns `Some(..)`
    // but the variant is not Float/Int/Uint.
    let shared = SharedGraph::new(GraphId::new(13_002));
    let label = db_string("N");
    let rel = db_string("R");
    let weight = db_string("weight");
    let mut txn = shared.begin_write();
    let a = txn
        .mutator()
        .create_node(LabelSet::single(label.clone()), PropertyMap::new())
        .unwrap();
    let b = txn
        .mutator()
        .create_node(LabelSet::single(label.clone()), PropertyMap::new())
        .unwrap();
    let c = txn
        .mutator()
        .create_node(LabelSet::single(label), PropertyMap::new())
        .unwrap();
    // a -> b: weight present but Null.
    txn.mutator()
        .create_edge(rel.clone(), a, b, props_with_weight(Value::Null))
        .unwrap();
    // b -> c: weight present but non-numeric (Bool).
    txn.mutator()
        .create_edge(rel, b, c, props_with_weight(Value::Bool(true)))
        .unwrap();
    txn.commit().unwrap();

    let snapshot = shared.read();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "null-weight".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: Some(weight),
        },
        None,
    )
    .unwrap();

    assert_eq!(
        proj.out_neighbors(a)[0].weight,
        1.0,
        "present-but-Null weight defaults to 1.0 per spec 16 §E04"
    );
    assert_eq!(
        proj.out_neighbors(b)[0].weight,
        1.0,
        "present-but-non-numeric weight defaults to 1.0 per spec 16 §E04"
    );
}

#[test]
fn projection_scope_intersect() {
    let (shared, nodes, _) = fixture_small();
    let snapshot = shared.read();
    let scope = snapshot.bind_node_candidates([nodes[0], nodes[1]]).unwrap();

    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "scoped".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        Some(&scope),
    )
    .unwrap();

    assert_eq!(proj.node_count(), 2);
    assert!(proj.contains(nodes[0]));
    assert!(proj.contains(nodes[1]));
    assert!(!proj.contains(nodes[2]));
    // Only the n1 -> n2 friend edge survives the scope intersection.
    assert_eq!(proj.edge_count(), 1);
}

#[test]
fn projection_generation_pinned() {
    let (shared, _, _) = fixture_small();
    let snapshot_v1 = shared.read();
    let gen_v1 = snapshot_v1.meta.generation;
    let proj = GraphProjection::build(
        &snapshot_v1,
        &ProjectionConfig {
            name: "pinned".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(proj.generation(), gen_v1);
    drop(snapshot_v1);

    // Mutate the graph: append one more Person node and commit.
    let mut txn = shared.begin_write();
    let person = db_string("Person");
    txn.mutator()
        .create_node(LabelSet::single(person), PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();

    let snapshot_v2 = shared.read();
    assert!(
        snapshot_v2.meta.generation > gen_v1,
        "generation should advance after a mutating commit"
    );
    // The projection was built against the v1 snapshot; its generation must
    // remain at gen_v1 (frozen at build time per spec 16 §E02).
    assert_eq!(
        proj.generation(),
        gen_v1,
        "projection generation is frozen at build time per §E02"
    );
}

proptest! {
    /// For any projection over a randomly-built graph, `out_neighbors(n)` MUST
    /// be sorted ASC by `node_id` for every `n` per spec 16 §E03.
    #[test]
    fn projection_neighbor_ordering_invariant(
        edge_seed in 0u64..8u64,
        edge_count in 0usize..32usize,
    ) {
        let shared = SharedGraph::new(GraphId::new(1));
        let label = db_string("N");
        let rel = db_string("R");

        let mut txn = shared.begin_write();
        let mut nodes: Vec<NodeId> = Vec::with_capacity(8);
        for _ in 0..8 {
            let id = txn
                .mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap();
            nodes.push(id);
        }
        let mut state = edge_seed.wrapping_add(1);
        for _ in 0..edge_count {
            // Deterministic xorshift-like step so the property test inputs map
            // onto reproducible edge layouts.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let src_idx = (state % 8) as usize;
            let tgt_idx = ((state >> 8) % 8) as usize;
            txn
                .mutator()
                .create_edge(rel.clone(), nodes[src_idx], nodes[tgt_idx], PropertyMap::new())
                .unwrap();
        }
        txn.commit().unwrap();

        let snapshot = shared.read();
        let proj = GraphProjection::build(
            &snapshot,
            &ProjectionConfig {
                name: "proptest".to_string(),
                node_labels: vec![],
                edge_labels: vec![],
                weight_property: None,
            },
            None,
        )
        .unwrap();

        for n in nodes.iter().copied() {
            let out = proj.out_neighbors(n);
            for window in out.windows(2) {
                prop_assert!(
                    window[0].node_id <= window[1].node_id,
                    "out_neighbors not sorted ASC: {:?} > {:?}",
                    window[0].node_id,
                    window[1].node_id,
                );
            }
            let inc = proj.in_neighbors(n);
            for window in inc.windows(2) {
                prop_assert!(
                    window[0].node_id <= window[1].node_id,
                    "in_neighbors not sorted ASC: {:?} > {:?}",
                    window[0].node_id,
                    window[1].node_id,
                );
            }
        }

    }
}

#[test]
fn algorithm_results_on_sparse_deleted_ids_match_before_and_after_compaction() {
    let shared = SharedGraph::new(GraphId::new(42_001));
    let label = db_string("Node");
    let rel = db_string("CONNECTS");

    let nodes = {
        let mut txn = shared.begin_write();
        let mut created = Vec::new();
        for _ in 0..5 {
            let id = txn
                .mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap();
            created.push(id);
        }
        // Ring between nodes[0], nodes[2], nodes[4]: 1 -> 3 -> 5 -> 1
        txn.mutator()
            .create_edge(rel.clone(), created[0], created[2], PropertyMap::new())
            .unwrap();
        txn.mutator()
            .create_edge(rel.clone(), created[2], created[4], PropertyMap::new())
            .unwrap();
        txn.mutator()
            .create_edge(rel.clone(), created[4], created[0], PropertyMap::new())
            .unwrap();
        // Delete nodes[1] and nodes[3]
        txn.mutator().delete_node(created[1]).unwrap();
        txn.mutator().delete_node(created[3]).unwrap();
        txn.commit().unwrap();
        created
    };

    let config = ProjectionConfig {
        name: "test_compaction_equiv".to_string(),
        node_labels: vec![],
        edge_labels: vec![],
        weight_property: None,
    };

    let pre_snap = shared.read();
    let pre_proj = GraphProjection::build(&pre_snap, &config, None).unwrap();
    assert_eq!(pre_proj.node_count(), 3);
    let pre_nodes: Vec<NodeId> = pre_proj.iter_nodes().collect();
    assert_eq!(pre_nodes, vec![nodes[0], nodes[2], nodes[4]]);

    let pr_config = selene_algorithms::PageRankConfig {
        damping: 0.85,
        max_iter: 100,
        tolerance: 1e-6,
        parallelism: selene_algorithms::Parallelism::Sequential,
        orientation: selene_algorithms::PageRankOrientation::Natural,
        personalization: None,
    };

    // Run algorithms before compaction
    let pre_wcc = selene_algorithms::wcc(&pre_proj);
    let pre_pr = selene_algorithms::pagerank(&pre_proj, pr_config.clone());

    // Compact the graph
    shared.compact().unwrap();

    let post_snap = shared.read();
    let post_proj = GraphProjection::build(&post_snap, &config, None).unwrap();
    assert_eq!(post_proj.node_count(), 3);
    let post_nodes: Vec<NodeId> = post_proj.iter_nodes().collect();
    assert_eq!(post_nodes, vec![nodes[0], nodes[2], nodes[4]]);

    // Run algorithms after compaction
    let post_wcc = selene_algorithms::wcc(&post_proj);
    let post_pr = selene_algorithms::pagerank(&post_proj, pr_config);

    assert_eq!(pre_wcc, post_wcc);
    assert_eq!(pre_pr, post_pr);
}
