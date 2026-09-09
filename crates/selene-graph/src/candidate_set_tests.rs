use std::collections::BTreeSet;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use proptest::prelude::*;
use selene_core::{EdgeId, GraphId, LabelSet, NodeId, PropertyMap, db_string};

use crate::candidate_set::{CandidateSet, Edge, Node};
use crate::store::{EdgeRow, NodeRow};
use crate::{
    CandidateSetError, GraphError, SeleneGraph, SharedGraph, VectorCandidateSet, WalConfig,
};

fn identity_graph(width: u32) -> SeleneGraph {
    let mut graph = SeleneGraph::new(GraphId::new(41));
    graph.meta.generation = 7;
    for raw in 0..width {
        let id = NodeId::new(u64::from(raw) + 101);
        let row = NodeRow::new(raw);
        graph.node_store.labels.push(LabelSet::new());
        graph.node_store.properties.push(PropertyMap::new());
        graph.node_store.row_to_id.push(id);
        graph.node_store.mark_alive(row);
        graph.node_rows.insert_cow(id, row);
    }
    for raw in 0..width {
        let id = EdgeId::new(u64::from(raw) + 201);
        let row = EdgeRow::new(raw);
        graph
            .edge_store
            .label
            .push(db_string("candidate.edge").unwrap());
        graph.edge_store.source.push(NodeId::new(101));
        graph
            .edge_store
            .directionality
            .push(selene_core::EdgeDirectionality::Directed);
        graph
            .edge_store
            .target
            .push(NodeId::new(u64::from(raw) + 101));
        graph.edge_store.properties.push(PropertyMap::new());
        graph.edge_store.row_to_id.push(id);
        graph.edge_store.mark_alive(row);
        graph.edge_rows.insert_cow(id, row);
    }
    graph
}

fn node_subset(graph: &SeleneGraph, ids: &BTreeSet<u32>) -> CandidateSet<Node> {
    CandidateSet::from_node_rows(
        graph,
        ids.iter().map(|raw| {
            let id = NodeId::new(u64::from(*raw) + 101);
            (id, graph.node_row_for_id(id).unwrap())
        }),
    )
}

fn edge_subset(graph: &SeleneGraph, ids: &BTreeSet<u32>) -> CandidateSet<Edge> {
    CandidateSet::from_edge_rows(
        graph,
        ids.iter().map(|raw| {
            let id = EdgeId::new(u64::from(*raw) + 201);
            (id, graph.edge_row_for_id(id).unwrap())
        }),
    )
}

proptest! {
    #[test]
    fn node_algebra_matches_btree_reference(
        left in proptest::collection::btree_set(0_u32..64, 0..64),
        right in proptest::collection::btree_set(0_u32..64, 0..64),
    ) {
        let graph = identity_graph(64);
        let left_candidates = node_subset(&graph, &left);
        let right_candidates = node_subset(&graph, &right);
        let stable = |raw: &u32| NodeId::new(u64::from(*raw) + 101);
        let expected_union = left.union(&right).map(stable).collect::<Vec<_>>();
        let expected_intersection = left.intersection(&right).map(stable).collect::<Vec<_>>();
        let expected_difference = left.difference(&right).map(stable).collect::<Vec<_>>();

        prop_assert_eq!(
            graph.union_candidates(&left_candidates, &right_candidates).unwrap().iter().collect::<Vec<_>>(),
            expected_union,
        );
        prop_assert_eq!(
            graph.intersect_candidates(&left_candidates, &right_candidates).unwrap().iter().collect::<Vec<_>>(),
            expected_intersection,
        );
        prop_assert_eq!(
            graph.difference_candidates(&left_candidates, &right_candidates).unwrap().iter().collect::<Vec<_>>(),
            expected_difference,
        );
    }

    #[test]
    fn edge_algebra_matches_btree_reference(
        left in proptest::collection::btree_set(0_u32..64, 0..64),
        right in proptest::collection::btree_set(0_u32..64, 0..64),
    ) {
        let graph = identity_graph(64);
        let left_candidates = edge_subset(&graph, &left);
        let right_candidates = edge_subset(&graph, &right);
        let stable = |raw: &u32| EdgeId::new(u64::from(*raw) + 201);
        let expected_union = left.union(&right).map(stable).collect::<Vec<_>>();
        let expected_intersection = left.intersection(&right).map(stable).collect::<Vec<_>>();
        let expected_difference = left.difference(&right).map(stable).collect::<Vec<_>>();

        prop_assert_eq!(
            graph.union_candidates(&left_candidates, &right_candidates).unwrap().iter().collect::<Vec<_>>(),
            expected_union,
        );
        prop_assert_eq!(
            graph.intersect_candidates(&left_candidates, &right_candidates).unwrap().iter().collect::<Vec<_>>(),
            expected_intersection,
        );
        prop_assert_eq!(
            graph.difference_candidates(&left_candidates, &right_candidates).unwrap().iter().collect::<Vec<_>>(),
            expected_difference,
        );
    }
}

#[test]
fn stable_id_binders_canonicalize_and_filter_non_live_ids_for_both_kinds() {
    let mut graph = identity_graph(4);
    graph.node_store.mark_dead(NodeRow::new(1));
    graph.edge_store.mark_dead(EdgeRow::new(2));

    let nodes = graph
        .bind_node_candidates([
            NodeId::new(104),
            NodeId::TOMBSTONE,
            NodeId::new(101),
            NodeId::new(101),
            NodeId::new(102),
            NodeId::new(999_999),
        ])
        .unwrap();
    let edges = graph
        .bind_edge_candidates([
            EdgeId::new(204),
            EdgeId::TOMBSTONE,
            EdgeId::new(201),
            EdgeId::new(201),
            EdgeId::new(203),
            EdgeId::new(999_999),
        ])
        .unwrap();

    assert_eq!(
        nodes.iter().collect::<Vec<_>>(),
        vec![NodeId::new(101), NodeId::new(104)]
    );
    assert_eq!(
        edges.iter().collect::<Vec<_>>(),
        vec![EdgeId::new(201), EdgeId::new(204)]
    );
}

#[test]
fn vector_candidate_binding_is_liveness_only() {
    let graph = identity_graph(3);
    let vectors =
        VectorCandidateSet::from_nodes([NodeId::new(103), NodeId::new(101), NodeId::new(102)]);

    let bound = graph.bind_vector_candidate_set(&vectors).unwrap();

    assert_eq!(
        bound.iter().collect::<Vec<_>>(),
        vec![NodeId::new(101), NodeId::new(102), NodeId::new(103)]
    );
    assert!(graph.node_properties(NodeId::new(101)).unwrap().is_empty());
}

#[test]
fn stable_id_binders_reject_live_reverse_mapping_disagreement() {
    let mut graph = identity_graph(2);
    graph.node_store.row_to_id.set(0, NodeId::new(102));
    graph.edge_store.row_to_id.set(0, EdgeId::new(202));

    assert!(matches!(
        graph.bind_node_candidates([NodeId::new(101)]),
        Err(GraphError::Inconsistent { .. })
    ));
    assert!(matches!(
        graph.bind_edge_candidates([EdgeId::new(201)]),
        Err(GraphError::Inconsistent { .. })
    ));
}

#[test]
fn typed_label_and_property_producers_match_raw_index_ids() {
    use selene_core::Value;

    let shared = SharedGraph::new(GraphId::new(40_001));
    let label = db_string("candidate.person").unwrap();
    let property = db_string("candidate.rank").unwrap();
    let (first, second) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let first = mutator
            .create_node(
                LabelSet::single(label.clone()),
                PropertyMap::from_pairs([(property.clone(), Value::Int(1))]).unwrap(),
            )
            .unwrap();
        let second = mutator
            .create_node(
                LabelSet::single(label.clone()),
                PropertyMap::from_pairs([(property.clone(), Value::Int(2))]).unwrap(),
            )
            .unwrap();
        mutator
            .create_property_index(label.clone(), property.clone(), crate::TypedIndexKind::I64)
            .unwrap();
        txn.commit().unwrap();
        (first, second)
    };
    let graph = shared.read();

    assert_eq!(
        graph
            .node_candidates_with_label(&label)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![first, second]
    );
    assert_eq!(
        graph
            .node_candidates_with_property_eq(&label, &property, &Value::Int(2))
            .unwrap()
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![second]
    );
}

#[test]
fn candidate_identity_rejects_graph_generation_and_layout_mismatch() {
    let graph = identity_graph(4);
    let nodes = graph.live_node_candidates().unwrap();

    let foreign_graph = SeleneGraph::new(GraphId::new(42));
    assert!(matches!(
        foreign_graph.union_candidates(&nodes, &nodes),
        Err(CandidateSetError::GraphMismatch { .. })
    ));

    let mut newer = graph.clone();
    newer.meta.generation += 1;
    assert_eq!(
        newer.union_candidates(&nodes, &nodes).unwrap_err(),
        CandidateSetError::GenerationMismatch {
            expected: 8,
            actual: 7,
        }
    );

    let mut independent = identity_graph(4);
    independent.meta.graph_id = graph.meta.graph_id;
    independent.meta.generation = graph.meta.generation;
    assert!(!nodes.shares_physical_layout_with(&independent));
    assert!(!nodes.shares_workspace_binding_with(&independent));
    assert_eq!(
        independent.union_candidates(&nodes, &nodes).unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
}

#[test]
fn candidate_validation_rejects_typed_mapping_drift() {
    let mut graph = identity_graph(2);
    let nodes = graph.live_node_candidates().unwrap();
    let edges = graph.live_edge_candidates().unwrap();
    graph
        .node_rows
        .insert_cow(NodeId::new(101), NodeRow::new(1));
    graph
        .edge_rows
        .insert_cow(EdgeId::new(201), EdgeRow::new(1));

    assert!(nodes.validate_identity_for(&graph).is_ok());
    assert!(edges.validate_identity_for(&graph).is_ok());
    assert_eq!(
        graph.union_candidates(&nodes, &nodes).unwrap_err(),
        CandidateSetError::StaleEntry
    );
    assert_eq!(
        graph.union_candidates(&edges, &edges).unwrap_err(),
        CandidateSetError::StaleEntry
    );
}

#[test]
fn candidate_retains_non_reusable_physical_layout_after_graph_drop() {
    let graph = identity_graph(2);
    let candidates = graph.live_node_candidates().unwrap();
    let retained = candidates.physical_layout_weak();
    drop(graph);
    assert!(retained.upgrade().is_some());

    let independent = identity_graph(2);
    assert_eq!(
        independent
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
    drop(candidates);
    assert!(retained.upgrade().is_none());
}

#[test]
fn mutable_workspace_candidate_retains_physical_layout_and_binding_after_graph_drop() {
    let shared = SharedGraph::new(GraphId::new(63));
    let mut txn = shared.begin_write();
    txn.mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let candidates = txn.read().live_node_candidates().unwrap();
    let physical = candidates.physical_layout_weak();
    let workspace = candidates.workspace_binding_weak();
    assert!(!std::sync::Weak::ptr_eq(&physical, &workspace));
    assert!(candidates.shares_physical_layout_with(txn.read()));
    assert!(candidates.shares_workspace_binding_with(txn.read()));

    txn.rollback();
    let published = shared.read();
    assert!(candidates.shares_physical_layout_with(&published));
    assert!(!candidates.shares_workspace_binding_with(&published));
    assert_eq!(
        published
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
    drop(published);
    drop(shared);

    assert!(physical.upgrade().is_some());
    assert!(workspace.upgrade().is_some());
    drop(candidates);
    assert!(physical.upgrade().is_none());
    assert!(workspace.upgrade().is_none());
}

fn populated_shared(graph_id: GraphId) -> SharedGraph {
    let shared = SharedGraph::new(graph_id);
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let first = mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .unwrap();
        let second = mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .unwrap();
        mutator
            .create_edge(
                db_string("candidate.edge").unwrap(),
                first,
                second,
                PropertyMap::new(),
            )
            .unwrap();
    }
    txn.commit().unwrap();
    shared
}

#[test]
fn clone_and_ordinary_publication_preserve_layout_but_generation_rejects() {
    let shared = populated_shared(GraphId::new(51));
    let before = shared.read();
    let clone = before.as_ref().clone();
    let candidates = before.live_node_candidates().unwrap();
    assert!(before.shares_layout_with(&clone));
    assert!(candidates.shares_physical_layout_with(&before));
    assert!(candidates.shares_workspace_binding_with(&before));
    let _ = clone.union_candidates(&candidates, &candidates).unwrap();

    let mut txn = shared.begin_write();
    txn.mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let workspace_candidates = txn.read().live_node_candidates().unwrap();
    assert!(candidates.shares_physical_layout_with(txn.read()));
    assert!(!candidates.shares_workspace_binding_with(txn.read()));
    assert!(workspace_candidates.shares_physical_layout_with(&before));
    assert!(!workspace_candidates.shares_workspace_binding_with(&before));
    txn.commit().unwrap();
    let after = shared.read();
    assert!(before.shares_layout_with(&after));
    assert!(candidates.shares_physical_layout_with(&after));
    assert!(!candidates.shares_workspace_binding_with(&after));
    assert!(workspace_candidates.shares_physical_layout_with(&after));
    assert!(workspace_candidates.shares_workspace_binding_with(&after));
    for stale in [&candidates, &workspace_candidates] {
        assert!(matches!(
            after.union_candidates(stale, stale),
            Err(CandidateSetError::GenerationMismatch { .. })
        ));
    }
}

#[test]
fn explicit_rollback_rejects_candidate_from_aborted_insert() {
    let shared = SharedGraph::new(GraphId::new(57));
    let mut txn = shared.begin_write();
    let aborted_id = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let escaped = txn.read().live_node_candidates().unwrap();
    assert!(escaped.contains(aborted_id));

    txn.rollback();

    let published = shared.read();
    assert!(escaped.shares_physical_layout_with(&published));
    assert!(!escaped.shares_workspace_binding_with(&published));
    assert_eq!(
        published.union_candidates(&escaped, &escaped).unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
    let fresh = published.live_node_candidates().unwrap();
    assert!(!fresh.contains(aborted_id));
    assert!(fresh.is_empty());
}

#[test]
fn implicit_rollback_rejects_candidate_from_aborted_insert() {
    let shared = SharedGraph::new(GraphId::new(58));
    let (aborted_id, escaped) = {
        let mut txn = shared.begin_write();
        let aborted_id = txn
            .mutator()
            .create_node(LabelSet::new(), PropertyMap::new())
            .unwrap();
        let escaped = txn.read().live_node_candidates().unwrap();
        (aborted_id, escaped)
    };

    let published = shared.read();
    assert!(escaped.shares_physical_layout_with(&published));
    assert!(!escaped.shares_workspace_binding_with(&published));
    assert_eq!(
        published.union_candidates(&escaped, &escaped).unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
    let fresh = published.live_node_candidates().unwrap();
    assert!(!fresh.contains(aborted_id));
    assert!(fresh.is_empty());
}

#[test]
fn cancelled_unpublished_preparation_rejects_abandoned_candidate() {
    let shared = SharedGraph::new(GraphId::new(59));
    let mut txn = shared.begin_write();
    let aborted_id = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let escaped = txn.read().live_node_candidates().unwrap();
    let cancelled = AtomicBool::new(true);

    let error = txn
        .prepare_unpublished(None, Some(&cancelled))
        .err()
        .expect("preparation must observe cancellation");
    assert!(matches!(error, GraphError::Cancelled));

    let published = shared.read();
    assert_eq!(published.meta.generation, 0);
    assert!(escaped.shares_physical_layout_with(&published));
    assert!(!escaped.shares_workspace_binding_with(&published));
    assert_eq!(
        published.union_candidates(&escaped, &escaped).unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
    let fresh = published.live_node_candidates().unwrap();
    assert!(!fresh.contains(aborted_id));
    assert!(fresh.is_empty());
}

#[test]
fn same_mutator_deletion_rejects_stale_node_and_edge_entries_before_generation_bump() {
    let shared = populated_shared(GraphId::new(60));
    let generation = shared.read().meta.generation;
    let mut txn = shared.begin_write();
    let mut mutator = txn.mutator();
    let candidates = mutator.read().live_node_candidates().unwrap();
    let edge_candidates = mutator.read().live_edge_candidates().unwrap();

    mutator.delete_node(NodeId::new(1)).unwrap();

    assert_eq!(mutator.read().meta.generation, generation);
    assert_eq!(
        mutator
            .read()
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::StaleEntry
    );
    assert!(matches!(
        mutator.read().validate_node_candidates(&candidates),
        Err(CandidateSetError::StaleEntry)
    ));
    assert_eq!(
        mutator
            .read()
            .union_candidates(&edge_candidates, &edge_candidates)
            .unwrap_err(),
        CandidateSetError::StaleEntry
    );
    assert!(matches!(
        mutator.read().validate_edge_candidates(&edge_candidates),
        Err(CandidateSetError::StaleEntry)
    ));
}

#[test]
fn same_mutator_reset_rejects_candidate_before_generation_bump() {
    let shared = populated_shared(GraphId::new(61));
    let generation = shared.read().meta.generation;
    let mut txn = shared.begin_write();
    let mut mutator = txn.mutator();
    let candidates = mutator.read().live_node_candidates().unwrap();

    mutator.factory_reset().unwrap();

    assert_eq!(mutator.read().meta.generation, generation);
    assert_eq!(
        mutator
            .read()
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
}

#[test]
fn repeated_mutator_acquisition_conservatively_remints_workspace() {
    let shared = populated_shared(GraphId::new(62));
    let mut txn = shared.begin_write();
    let candidates = {
        let mutator = txn.mutator();
        mutator.read().live_node_candidates().unwrap()
    };

    let second = txn.mutator();
    assert_eq!(
        second
            .read()
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
}

#[test]
fn detached_shared_attachment_remints_layout() {
    let graph = identity_graph(3);
    let candidates = graph.live_node_candidates().unwrap();
    let shared = SharedGraph::try_from_graph(graph.clone()).unwrap();
    let attached = shared.read();
    assert_eq!(attached.graph_id(), graph.graph_id());
    assert_eq!(attached.meta.generation, graph.meta.generation);
    assert!(!attached.shares_layout_with(&graph));
    assert!(!candidates.shares_physical_layout_with(&attached));
    assert!(!candidates.shares_workspace_binding_with(&attached));
    assert_eq!(
        attached
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
}

#[test]
fn compaction_remints_without_changing_generation_or_surviving_ids() {
    let shared = populated_shared(GraphId::new(52));
    let mut txn = shared.begin_write();
    txn.mutator().delete_edge(EdgeId::new(1)).unwrap();
    txn.commit().unwrap();
    let before = shared.read();
    let candidates = before.live_node_candidates().unwrap();
    let expected_ids = candidates.iter().collect::<Vec<_>>();
    let generation = before.meta.generation;

    shared.compact().unwrap();
    let after = shared.read();
    let fresh = after.live_node_candidates().unwrap();
    assert_eq!(after.meta.generation, generation);
    assert_eq!(fresh.iter().collect::<Vec<_>>(), expected_ids);
    assert!(!before.shares_layout_with(&after));
    assert!(!candidates.shares_physical_layout_with(&after));
    assert!(!candidates.shares_workspace_binding_with(&after));
    assert_eq!(
        after
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
}

#[test]
fn factory_reset_remints_before_generation_changes() {
    let shared = populated_shared(GraphId::new(53));
    let before = shared.read();
    let candidates = before.live_node_candidates().unwrap();
    let mut txn = shared.begin_write();
    txn.mutator().factory_reset().unwrap();
    assert_eq!(txn.read().meta.generation, before.meta.generation);
    assert!(!txn.read().shares_layout_with(&before));
    assert!(!candidates.shares_physical_layout_with(txn.read()));
    assert!(!candidates.shares_workspace_binding_with(txn.read()));
    assert_eq!(
        txn.read()
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
    txn.commit().unwrap();
}

#[test]
fn non_remapping_rebuild_preserves_layout_and_generation() {
    let shared = populated_shared(GraphId::new(54));
    let before = shared.read();
    let candidates = before.live_node_candidates().unwrap();
    let generation = before.meta.generation;
    shared.rebuild_vector_indexes().unwrap();
    let after = shared.read();
    assert_eq!(after.meta.generation, generation);
    assert!(after.shares_layout_with(&before));
    assert!(candidates.shares_physical_layout_with(&after));
    assert!(candidates.shares_workspace_binding_with(&after));
    let _ = after.union_candidates(&candidates, &candidates).unwrap();
}

fn recovery_dir(name: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "selene-candidate-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn recovery_remints_independent_layout_and_rebuilds_typed_maps() {
    let dir = recovery_dir("remint");
    let graph_id = GraphId::new(55);
    let shared = SharedGraph::builder(graph_id)
        .with_wal(dir.join(crate::DEFAULT_WAL_FILE_NAME), WalConfig::default())
        .unwrap()
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    txn.mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();
    let before = shared.read();
    let candidates = before.live_node_candidates().unwrap();
    let generation = before.meta.generation;
    drop(before);
    drop(shared);

    let recovered = SharedGraph::recover(&dir, graph_id).unwrap();
    let after = recovered.read();
    assert_eq!(after.meta.generation, generation);
    assert!(!candidates.shares_physical_layout_with(&after));
    assert!(!candidates.shares_workspace_binding_with(&after));
    assert_eq!(
        after
            .live_node_candidates()
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![NodeId::new(1)]
    );
    assert_eq!(
        after
            .union_candidates(&candidates, &candidates)
            .unwrap_err(),
        CandidateSetError::LayoutMismatch
    );
    after.assert_indexes_consistent().unwrap();
    drop(after);
    drop(recovered);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn validated_candidates_drive_mapping_endpoint_delete_and_consistency_paths() {
    let shared = populated_shared(GraphId::new(56));
    let before = shared.read();
    let nodes = before.live_node_candidates().unwrap();
    let edges = before.live_edge_candidates().unwrap();
    assert_eq!(
        nodes.iter().collect::<Vec<_>>(),
        vec![NodeId::new(1), NodeId::new(2)]
    );
    assert_eq!(edges.iter().collect::<Vec<_>>(), vec![EdgeId::new(1)]);
    let valid_nodes = before.validate_node_candidates(&nodes).unwrap();
    let valid_edges = before.validate_edge_candidates(&edges).unwrap();
    assert_eq!(valid_nodes.len(), 2);
    assert_eq!(valid_edges.len(), 1);
    for node in valid_nodes.as_slice() {
        assert!(node.properties().is_ok());
    }
    for edge in valid_edges.as_slice() {
        assert_eq!(edge.endpoints().unwrap(), (NodeId::new(1), NodeId::new(2)));
    }
    assert_eq!(
        before.edge_endpoints(EdgeId::new(1)),
        Some((NodeId::new(1), NodeId::new(2)))
    );
    drop(before);

    let mut txn = shared.begin_write();
    txn.mutator().delete_node(NodeId::new(1)).unwrap();
    txn.commit().unwrap();
    let after = shared.read();
    assert_eq!(
        after
            .live_node_candidates()
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![NodeId::new(2)]
    );
    assert!(after.live_edge_candidates().unwrap().is_empty());
    after.assert_indexes_consistent().unwrap();
}
