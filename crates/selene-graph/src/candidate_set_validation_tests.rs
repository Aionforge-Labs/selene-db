use roaring::RoaringBitmap;
use selene_core::{
    CancellationChecker, EdgeId, GraphId, JsonValue, LabelSet, NodeId, PropertyMap, Value,
    VectorMetric, VectorValue, db_string,
};

use crate::error::CandidateSetError;
use crate::shared::SharedGraph;
use crate::vector_index::VectorIndexSearchHit;

fn sample_vector(components: &[f32]) -> VectorValue {
    VectorValue::new(components.to_vec()).expect("valid vector")
}

fn sample_properties(entries: Vec<(&str, Value)>) -> PropertyMap {
    PropertyMap::from_pairs(entries.into_iter().map(|(k, v)| (db_string(k).unwrap(), v)))
        .expect("valid property map")
}

fn populate_graph_with_data(graph_id: GraphId) -> SharedGraph {
    let shared = SharedGraph::new(graph_id);
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let n1_props = sample_properties(vec![
            ("embedding", Value::Vector(sample_vector(&[1.0, 0.0, 0.0]))),
            ("title", Value::String(db_string("graph database").unwrap())),
            (
                "metadata",
                Value::Json(JsonValue::parse_str(r#"{"status": "active", "score": 10}"#).unwrap()),
            ),
        ]);
        let n2_props = sample_properties(vec![
            ("embedding", Value::Vector(sample_vector(&[0.0, 1.0, 0.0]))),
            ("title", Value::String(db_string("vector index").unwrap())),
            (
                "metadata",
                Value::Json(JsonValue::parse_str(r#"{"status": "pending", "score": 20}"#).unwrap()),
            ),
        ]);
        let first = mutator
            .create_node(LabelSet::single(db_string("Doc").unwrap()), n1_props)
            .unwrap();
        let second = mutator
            .create_node(LabelSet::single(db_string("Doc").unwrap()), n2_props)
            .unwrap();
        let edge_props = sample_properties(vec![(
            "weight",
            Value::String(db_string("primary").unwrap()),
        )]);
        mutator
            .create_edge(db_string("RELATES_TO").unwrap(), first, second, edge_props)
            .unwrap();
    }
    txn.commit().unwrap();
    shared
}

#[test]
fn independent_same_id_same_generation_rejected_in_search_and_score() {
    let shared_a = populate_graph_with_data(GraphId::new(100));
    let shared_b = populate_graph_with_data(GraphId::new(100));

    let snap_a = shared_a.read();
    let snap_b = shared_b.read();

    assert_eq!(snap_a.meta.graph_id, snap_b.meta.graph_id);
    assert_eq!(snap_a.meta.generation, snap_b.meta.generation);

    let node_candidates_a = snap_a.live_node_candidates().unwrap();
    let edge_candidates_a = snap_a.live_edge_candidates().unwrap();

    // 1. Direct validation rejects foreign candidate with LayoutMismatch
    assert_eq!(
        snap_b
            .validate_node_candidates(&node_candidates_a)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
    assert_eq!(
        snap_b
            .validate_edge_candidates(&edge_candidates_a)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );

    // 2. Candidate algebra rejects foreign candidate with LayoutMismatch
    assert_eq!(
        snap_b
            .union_candidates(&node_candidates_a, &node_candidates_a)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );

    // 3. Vector candidate scoring rejects foreign candidate
    let query = sample_vector(&[1.0, 0.0, 0.0]);
    let prop_name = db_string("embedding").unwrap();
    assert!(
        snap_b
            .score_vector_candidate_set_bound_checked(
                &prop_name,
                &query,
                &node_candidates_a,
                VectorMetric::Cosine,
                10,
                CancellationChecker::disabled(),
            )
            .is_err()
    );

    // 4. Batch candidate scoring rejects foreign candidate
    assert!(
        snap_b
            .score_bound_candidate_sets_batch(
                &prop_name,
                std::slice::from_ref(&query),
                std::slice::from_ref(&node_candidates_a),
                VectorMetric::Cosine,
                10,
                CancellationChecker::disabled(),
            )
            .is_err()
    );

    // 5. Text search with foreign allowed set rejects
    let text_prop = db_string("title").unwrap();
    let doc_label = db_string("Doc").unwrap();
    assert!(
        snap_b
            .exact_text_search_nodes_filtered_checked(
                &doc_label,
                &text_prop,
                "database",
                10,
                Some(&node_candidates_a),
                CancellationChecker::disabled(),
            )
            .is_err()
    );
}

#[test]
fn generation_mismatch_rejected_in_search_and_score() {
    let shared = populate_graph_with_data(GraphId::new(101));
    let before = shared.read();
    let candidates = before.live_node_candidates().unwrap();

    // Mutate to bump generation
    let mut txn = shared.begin_write();
    let n3 = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();

    let after = shared.read();
    assert_ne!(before.meta.generation, after.meta.generation);

    // Validation against updated snapshot rejects with GenerationMismatch
    assert_eq!(
        after.validate_node_candidates(&candidates).unwrap_err(),
        CandidateSetError::GenerationMismatch {
            expected: after.meta.generation,
            actual: before.meta.generation,
        }
    );

    // Vector scoring with stale candidate against new snapshot is rejected
    let query = sample_vector(&[1.0, 0.0, 0.0]);
    let prop_name = db_string("embedding").unwrap();
    assert!(
        after
            .score_vector_candidate_set_bound_checked(
                &prop_name,
                &query,
                &candidates,
                VectorMetric::Cosine,
                10,
                CancellationChecker::disabled(),
            )
            .is_err()
    );

    // Text search with stale candidate against new snapshot is rejected
    let text_prop = db_string("title").unwrap();
    let doc_label = db_string("Doc").unwrap();
    assert!(
        after
            .exact_text_search_nodes_filtered_checked(
                &doc_label,
                &text_prop,
                "database",
                10,
                Some(&candidates),
                CancellationChecker::disabled(),
            )
            .is_err()
    );

    let _ = n3;
}

#[test]
fn retained_old_snapshot_can_still_use_its_own_candidates() {
    let shared = populate_graph_with_data(GraphId::new(102));
    let before = shared.read();
    let candidates = before.live_node_candidates().unwrap();

    // Validate candidates against the snapshot that produced them succeeds
    let valid_nodes = before.validate_node_candidates(&candidates).unwrap();
    assert_eq!(valid_nodes.len(), 2);

    // Score on before snapshot succeeds
    let query = sample_vector(&[1.0, 0.0, 0.0]);
    let prop_name = db_string("embedding").unwrap();
    let hits = before
        .score_vector_candidate_set_bound_checked(
            &prop_name,
            &query,
            &candidates,
            VectorMetric::Cosine,
            10,
            CancellationChecker::disabled(),
        )
        .unwrap();
    assert_eq!(hits.len(), 2);

    // Mutate and commit new snapshot
    let mut txn = shared.begin_write();
    txn.mutator().delete_node(NodeId::new(1)).unwrap();
    txn.commit().unwrap();

    let after = shared.read();

    // Old snapshot still functions and produces correct results with its own candidates
    let hits_before = before
        .score_vector_candidate_set_bound_checked(
            &prop_name,
            &query,
            &candidates,
            VectorMetric::Cosine,
            10,
            CancellationChecker::disabled(),
        )
        .unwrap();
    assert_eq!(hits_before.len(), 2);

    // New snapshot rejects candidates from the old snapshot
    assert!(
        after
            .score_vector_candidate_set_bound_checked(
                &prop_name,
                &query,
                &candidates,
                VectorMetric::Cosine,
                10,
                CancellationChecker::disabled(),
            )
            .is_err()
    );
}

#[test]
fn edge_candidates_validation_and_accessors() {
    let shared = populate_graph_with_data(GraphId::new(103));
    let snap = shared.read();
    let edges = snap.live_edge_candidates().unwrap();
    assert!(!edges.is_empty());

    let valid_edges = snap.validate_edge_candidates(&edges).unwrap();
    assert_eq!(valid_edges.len(), 1);
    assert!(!valid_edges.is_empty());
    assert!(valid_edges.ptr_eq(&valid_edges));

    let edge_ids: Vec<EdgeId> = valid_edges.edge_ids().collect();
    assert_eq!(edge_ids, vec![EdgeId::new(1)]);

    let first = valid_edges.as_slice()[0];
    assert_eq!(first.edge_id(), EdgeId::new(1));
    assert_eq!(first.label().unwrap().as_str(), "RELATES_TO");
    assert_eq!(first.endpoints().unwrap(), (NodeId::new(1), NodeId::new(2)));
    assert!(
        first
            .properties()
            .unwrap()
            .contains_key(&db_string("weight").unwrap())
    );
}

#[test]
fn validated_node_candidates_accessors_and_filtering() {
    let shared = populate_graph_with_data(GraphId::new(104));
    let snap = shared.read();
    let nodes = snap.live_node_candidates().unwrap();

    let valid_nodes = snap.validate_node_candidates(&nodes).unwrap();
    assert_eq!(valid_nodes.len(), 2);
    assert!(!valid_nodes.is_empty());
    assert!(valid_nodes.ptr_eq(&valid_nodes));

    let node_ids: Vec<NodeId> = valid_nodes.node_ids().collect();
    assert_eq!(node_ids, vec![NodeId::new(1), NodeId::new(2)]);

    let n1 = valid_nodes.as_slice()[0];
    assert_eq!(n1.node_id(), NodeId::new(1));
    assert!(n1.has_label(&db_string("Doc").unwrap()).unwrap());
    assert!(!n1.has_label(&db_string("Other").unwrap()).unwrap());
    assert!(
        n1.properties()
            .unwrap()
            .contains_key(&db_string("title").unwrap())
    );

    let vector = n1
        .vector_property(&db_string("embedding").unwrap())
        .unwrap();
    assert!(vector.is_some());
    assert_eq!(vector.unwrap().as_slice(), &[1.0, 0.0, 0.0]);

    let title = n1.string_property(&db_string("title").unwrap()).unwrap();
    assert_eq!(title.unwrap().as_str(), "graph database");

    let meta = n1.json_property(&db_string("metadata").unwrap()).unwrap();
    assert!(meta.is_some());

    // Non-existent property returns None
    assert!(
        n1.vector_property(&db_string("nonexistent").unwrap())
            .unwrap()
            .is_none()
    );

    // TurboQuant index bitmap filtering
    let mut index_bitmap = RoaringBitmap::new();
    index_bitmap.insert(0); // Node 1 row is 0
    index_bitmap.insert(99); // foreign row
    let filtered = valid_nodes
        .filter_index_rows(&index_bitmap, &CancellationChecker::disabled())
        .unwrap();
    assert!(filtered.contains(0));
    assert!(!filtered.contains(99));

    // ANN row hit resolution
    let ann_hits = vec![
        VectorIndexSearchHit {
            row: 0,
            distance: 0.1,
        },
        VectorIndexSearchHit {
            row: 99,
            distance: 0.2,
        },
    ];
    let resolved = valid_nodes
        .resolve_ann_row_hits(ann_hits, &CancellationChecker::disabled())
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].node_id, NodeId::new(1));
    assert_eq!(resolved[0].distance, 0.1);

    // ANN rerank
    let ann_hits = vec![
        VectorIndexSearchHit {
            row: 0,
            distance: 0.9,
        },
        VectorIndexSearchHit {
            row: 1,
            distance: 0.1,
        },
    ];
    let reranked = valid_nodes
        .rerank_ann_row_hits(
            &db_string("embedding").unwrap(),
            &sample_vector(&[1.0, 0.0, 0.0]),
            VectorMetric::Cosine,
            10,
            ann_hits,
            &CancellationChecker::disabled(),
        )
        .unwrap();
    assert_eq!(reranked.len(), 2);
    // Node 1 vector [1,0,0] matches query [1,0,0] exactly (distance ~0.0), so it should rank first
    assert_eq!(reranked[0].node_id, NodeId::new(1));
}

#[test]
fn binding_liveness_only_handles_tombstones_and_duplicates() {
    let shared = populate_graph_with_data(GraphId::new(105));
    let snap = shared.read();

    let ids = vec![
        NodeId::new(1),
        NodeId::TOMBSTONE,
        NodeId::new(2),
        NodeId::new(1),
        NodeId::new(999), // absent
    ];
    let bound = snap.bind_node_candidates(ids).unwrap();
    assert_eq!(
        bound.iter().collect::<Vec<_>>(),
        vec![NodeId::new(1), NodeId::new(2)]
    );
}
