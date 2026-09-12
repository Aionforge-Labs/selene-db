use crate::{CandidateStateSpec, IndexProvider, MaintainedCandidateStateProvider, SharedGraph};
use selene_core::{CancellationChecker, GraphId, LabelSet, PropertyMap, db_string};

#[test]
fn typed_text_candidates_reject_foreign_empty_and_old_generation() {
    let graph = SharedGraph::new(GraphId::new(409));
    let foreign = SharedGraph::new(GraphId::new(410));
    let label = db_string("Memory").unwrap();
    let body = db_string("body").unwrap();
    graph
        .create_text_index(label.clone(), body.clone())
        .unwrap();
    let old = graph.read();
    let candidates = old.live_node_candidates().unwrap();
    let other = foreign.read().live_node_candidates().unwrap();
    assert!(
        old.score_text_candidates_checked(
            &label,
            &body,
            "memory",
            &other,
            0,
            CancellationChecker::disabled()
        )
        .is_err()
    );
    let mut tx = graph.begin_write();
    tx.mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    tx.commit().unwrap();
    assert!(
        graph
            .read()
            .score_text_candidates_checked(
                &label,
                &body,
                "memory",
                &candidates,
                0,
                CancellationChecker::disabled()
            )
            .is_err()
    );
    assert!(
        old.score_text_candidates_checked(
            &label,
            &body,
            "memory",
            &candidates,
            0,
            CancellationChecker::disabled()
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn rebuilt_provider_cannot_bind_equal_generation_in_another_graph() {
    let first = SharedGraph::new(GraphId::new(411));
    let second = SharedGraph::new(GraphId::new(412));
    let name = db_string("current").unwrap();
    let provider = MaintainedCandidateStateProvider::from_graph(
        [CandidateStateSpec::new(name.clone())],
        &first.read(),
    )
    .unwrap();
    assert_eq!(first.read().meta.generation, second.read().meta.generation);
    assert!(provider.node_candidate_set(&name, &second.read()).is_err());
    assert!(
        provider
            .node_candidate_set(&name, &first.read())
            .unwrap()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn provider_boundary_rejects_foreign_typed_results_before_raw_id_conversion() {
    struct Foreign(crate::CandidateSet<crate::Node>);
    impl IndexProvider for Foreign {
        fn provider_tag(&self) -> crate::ProviderTag {
            crate::ProviderTag(*b"TEST")
        }
        fn on_change(&self, _: &selene_core::Change) -> Result<(), crate::ProviderError> {
            Ok(())
        }
        fn node_candidate_set(
            &self,
            _: &selene_core::DbString,
            _: &crate::SeleneGraph,
        ) -> Result<Option<crate::CandidateSet<crate::Node>>, crate::ProviderError> {
            Ok(Some(self.0.clone()))
        }
    }
    let first = SharedGraph::new(GraphId::new(413));
    let second = SharedGraph::new(GraphId::new(414));
    let provider = Foreign(first.read().live_node_candidates().unwrap());
    let error = second
        .read()
        .maintained_node_candidates(&provider, &db_string("current").unwrap())
        .unwrap_err();
    assert!(matches!(error, crate::ProviderError::Inconsistent { .. }));
}
