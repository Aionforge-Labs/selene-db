//! F04-PR07: primary-value authority and pinned vector candidate validation.

use selene_core::{CoreError, GraphId, LabelSet, PropertyMap, Value, db_string};

use super::*;
use crate::{VectorIndex, VectorIndexKind, graph::VectorIndexEntry};

fn vector(values: &[f32]) -> VectorValue {
    VectorValue::new(values.to_vec()).unwrap()
}

fn fixture() -> (SeleneGraph, DbString, DbString, Vec<NodeId>) {
    let shared = SharedGraph::new(GraphId::new(907));
    let label = db_string("Memory").unwrap();
    let property = db_string("embedding").unwrap();
    let mut txn = shared.begin_write();
    let ids = [
        Some(Value::Vector(vector(&[1.0, 0.0]))),
        Some(Value::Vector(vector(&[0.0, 1.0]))),
        Some(Value::Vector(vector(&[0.0, 0.0]))),
        Some(Value::Int(42)),
        None,
    ]
    .into_iter()
    .map(|value| {
        let props = PropertyMap::from_pairs(value.map(|v| (property.clone(), v))).unwrap();
        txn.mutator()
            .create_node(LabelSet::single(label.clone()), props)
            .unwrap()
    })
    .collect();
    txn.commit().unwrap();
    (shared.read().as_ref().clone(), label, property, ids)
}

#[test]
fn failed_rebuild_cannot_make_exact_search_trust_a_stale_bitmap() {
    let (mut graph, label, property, ids) = fixture();
    // Fault injection: primary values are valid, but the retained declaration
    // cannot admit a zero-norm cosine vector or a non-vector property. Its old
    // accelerator has no rows. A failed rebuild must not hide primary vectors.
    graph.vector_index.insert(
        (label.clone(), property.clone()),
        VectorIndexEntry::new(
            VectorIndex::new(VectorIndexKind::HnswCosine, 2).unwrap(),
            None,
        ),
    );
    assert!(matches!(
        crate::vector_index::rebuild_vector_indexes_strict(&mut graph),
        Err(GraphError::VectorIndexValueRejected { .. })
    ));
    assert_eq!(
        graph
            .vector_index_for(&label, &property)
            .unwrap()
            .cardinality(),
        0
    );
    let query = vector(&[1.0, 0.0]);
    // Independently derived squared distances: (1,0)->(1,0)=0,
    // (1,0)->(0,0)=1, (1,0)->(0,1)=2. No metric kernel supplies expected values.
    let expected = [(ids[0], 0.0), (ids[2], 1.0), (ids[1], 2.0)]
        .map(|(node_id, distance)| VectorNodeSearchHit { node_id, distance })
        .to_vec();
    assert_eq!(
        graph
            .exact_vector_search_nodes(
                &label,
                &property,
                &query,
                VectorMetric::SquaredEuclidean,
                99
            )
            .unwrap(),
        expected
    );
    assert_eq!(
        graph
            .exact_vector_search_nodes_batch_checked(
                &label,
                &property,
                &[query.clone(), query.clone()],
                VectorMetric::SquaredEuclidean,
                99,
                CancellationChecker::disabled()
            )
            .unwrap(),
        vec![expected.clone(), expected]
    );
    assert!(matches!(
        graph.exact_vector_search_nodes(&label, &property, &query, VectorMetric::Cosine, 99),
        Err(GraphError::Core(CoreError::VectorZeroNorm { .. }))
    ));
}

#[test]
fn lenient_rebuild_declines_partial_ann_but_preserves_primary_exact_values() {
    let (mut graph, label, property, ids) = fixture();
    graph.vector_index.insert(
        (label.clone(), property.clone()),
        VectorIndexEntry::new(
            VectorIndex::new(VectorIndexKind::HnswCosine, 2).unwrap(),
            None,
        ),
    );
    crate::vector_index::rebuild_vector_indexes(&mut graph).unwrap();
    assert!(graph.vector_index_for(&label, &property).is_none());
    assert_eq!(graph.vector_index_count(), 1, "registration is retained");
    let query = vector(&[1.0, 0.0]);
    assert!(matches!(
        graph.approximate_vector_search_nodes_checked(
            &label,
            &property,
            &query,
            ApproximateVectorSearchOptions::new(VectorMetric::Cosine, 3, 64),
            CancellationChecker::disabled()
        ),
        Err(VectorSearchError::ApproximateIndexMissing)
    ));
    assert_eq!(
        graph
            .exact_vector_search_nodes(
                &label,
                &property,
                &query,
                VectorMetric::SquaredEuclidean,
                99
            )
            .unwrap()
            .len(),
        3
    );
    // Fault repair seam: correct primary data without asking strict mutation
    // validation to admit the corrupted old values, then retry the retained
    // incomplete registration. Rebuild, not ordinary maintenance, restores it.
    let entry = graph
        .vector_index
        .remove(&(label.clone(), property.clone()))
        .unwrap();
    let shared = SharedGraph::from_graph(graph);
    let mut txn = shared.begin_write();
    txn.mutator().delete_node(ids[2]).unwrap();
    txn.mutator().delete_node(ids[3]).unwrap();
    txn.commit().unwrap();
    let mut graph = shared.read().as_ref().clone();
    graph
        .vector_index
        .insert((label.clone(), property.clone()), entry);
    crate::vector_index::rebuild_vector_indexes_strict(&mut graph).unwrap();
    assert_eq!(
        graph
            .vector_index_for(&label, &property)
            .unwrap()
            .cardinality(),
        2
    );
    assert_eq!(
        graph
            .approximate_vector_search_nodes_checked(
                &label,
                &property,
                &query,
                ApproximateVectorSearchOptions::new(VectorMetric::Cosine, 3, 64),
                CancellationChecker::disabled()
            )
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn generic_binding_is_liveness_only_and_scoring_skips_absent_or_non_vector_values() {
    let (graph, _, property, ids) = fixture();
    let bound = graph.bind_node_candidates(ids.iter().copied()).unwrap();
    assert_eq!(bound.len(), 5);
    assert_eq!(graph.validate_node_candidates(&bound).unwrap().len(), 5);
    let hits = graph
        .score_vector_nodes(
            &property,
            &vector(&[1.0, 0.0]),
            &ids,
            VectorMetric::SquaredEuclidean,
            99,
        )
        .unwrap();
    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|hit| ids[..3].contains(&hit.node_id)));
}

#[test]
fn filtered_ann_validates_foreign_candidates_even_for_empty_or_zero_k() {
    let (graph, label, property, ids) = fixture();
    let other = SharedGraph::new(GraphId::new(908));
    let other = other.read();
    for candidates in [
        graph.bind_node_candidates([]).unwrap(),
        graph.bind_node_candidates(ids).unwrap(),
    ] {
        for k in [0, 3] {
            assert!(matches!(
                other.approximate_vector_search_nodes_in_candidates_checked(
                    &label,
                    &property,
                    &vector(&[1.0, 0.0]),
                    &candidates,
                    ApproximateVectorSearchOptions::new(VectorMetric::Cosine, k, 64),
                    CancellationChecker::disabled()
                ),
                Err(VectorSearchError::Graph(GraphError::Inconsistent { .. }))
            ));
        }
    }
}

#[test]
fn selective_hnsw_filter_does_not_silently_refill_a_bounded_beam() {
    let shared = SharedGraph::new(GraphId::new(909));
    let label = db_string("Memory").unwrap();
    let property = db_string("embedding").unwrap();
    let mut txn = shared.begin_write();
    let ids: Vec<_> = (0..128)
        .map(|i| {
            txn.mutator()
                .create_node(
                    LabelSet::single(label.clone()),
                    PropertyMap::from_pairs([(
                        property.clone(),
                        Value::Vector(vector(&[1.0, i as f32])),
                    )])
                    .unwrap(),
                )
                .unwrap()
        })
        .collect();
    txn.commit().unwrap();
    shared
        .create_vector_index(
            label.clone(),
            property.clone(),
            VectorIndexKind::HnswSquaredEuclidean,
            2,
        )
        .unwrap();
    let graph = shared.read();
    let candidates = graph.bind_node_candidates([ids[127]]).unwrap();
    let query = vector(&[1.0, 0.0]);
    let low = graph
        .approximate_vector_search_nodes_in_candidates_checked(
            &label,
            &property,
            &query,
            &candidates,
            ApproximateVectorSearchOptions::new(VectorMetric::SquaredEuclidean, 1, 1),
            CancellationChecker::disabled(),
        )
        .unwrap();
    assert!(
        low.is_empty(),
        "no unrequested exact refill of the global beam"
    );
    let wide = graph
        .approximate_vector_search_nodes_in_candidates_checked(
            &label,
            &property,
            &query,
            &candidates,
            ApproximateVectorSearchOptions::new(VectorMetric::SquaredEuclidean, 1, 128),
            CancellationChecker::disabled(),
        )
        .unwrap();
    assert_eq!(
        wide,
        vec![VectorNodeSearchHit {
            node_id: ids[127],
            distance: 16129.0
        }]
    );
}
