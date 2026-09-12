use super::*;

#[test]
fn builder_constructs_empty_graph() {
    let shared = SharedGraph::builder(GraphId::new(1)).build().unwrap();
    assert_eq!(shared.read().meta.graph_id, GraphId::new(1));
    assert!(shared.providers.is_empty());
}

#[test]
fn publication_failure_does_not_publish_in_memory_state() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(db_string("durable.rollback").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
    }

    let mut sealed = txn.seal(None, None).unwrap();
    sealed.fail_publish = true;
    assert!(matches!(
        shared.submit_sealed_for_test(sealed),
        Err(GraphError::IndeterminateOutcome { reason })
            if reason.contains("synthetic memory publication failure")
    ));
    assert_eq!(shared.read().node_count(), 0);
}

#[test]
fn builder_bound_to_constructs_closed_graph() {
    let graph_type = sample_type();
    let shared = SharedGraph::builder(GraphId::new(1))
        .bound_to(graph_type.clone())
        .unwrap()
        .build()
        .unwrap();
    assert!(shared.is_closed());
    assert_eq!(shared.graph_type().as_deref(), Some(&graph_type));
}

#[test]
fn builder_bound_to_rejects_invalid_type() {
    let mut graph_type = sample_type();
    graph_type.node_types[0].key_labels = LabelSet::new();
    assert!(matches!(
        SharedGraph::builder(GraphId::new(1)).bound_to(graph_type),
        Err(GraphError::Inconsistent { reason }) if reason.contains("empty label set")
    ));
}

#[test]
fn builder_with_two_providers_preserves_registration_order() {
    let first = Arc::new(TestProvider::new(ProviderTag(*b"ONE1")));
    let second = Arc::new(TestProvider::new(ProviderTag(*b"TWO2")));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(first)
        .with_provider(second)
        .build()
        .unwrap();
    assert_eq!(shared.providers[0].provider_tag(), ProviderTag(*b"ONE1"));
    assert_eq!(shared.providers[1].provider_tag(), ProviderTag(*b"TWO2"));
}

#[test]
fn builder_rejects_duplicate_provider_tags() {
    let result = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(TestProvider::new(ProviderTag(*b"DUPL"))))
        .with_provider(Arc::new(TestProvider::new(ProviderTag(*b"DUPL"))))
        .build();
    let err = match result {
        Ok(_) => panic!("duplicate provider tags should be rejected"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        GraphError::Provider(ProviderError::Inconsistent { reason })
            if reason.contains("duplicate provider tag DUPL")
    ));
}

#[test]
fn from_graph_with_providers_validates_uniqueness() {
    let graph = SeleneGraph::new(GraphId::new(1));
    let result = SharedGraph::from_graph_with_providers(
        graph,
        vec![
            Arc::new(TestProvider::new(ProviderTag(*b"SAME"))),
            Arc::new(TestProvider::new(ProviderTag(*b"SAME"))),
        ],
    );
    let err = match result {
        Ok(_) => panic!("duplicate provider tags should be rejected"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        GraphError::Provider(ProviderError::Inconsistent { .. })
    ));
}

#[test]
fn public_memory_constructors_do_not_register_persistence_providers() {
    let from_new = SharedGraph::new(GraphId::new(1));
    let from_graph = SharedGraph::from_graph(SeleneGraph::new(GraphId::new(2)));
    assert!(from_new.index_providers().is_empty());
    assert!(from_graph.index_providers().is_empty());
}

#[test]
fn observer_tag_has_no_implicit_persistence_registration() {
    let result = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(TestProvider::new(ProviderTag(*b"CORE"))))
        .build();
    assert!(result.is_ok());
}

#[test]
fn index_provider_by_tag_returns_registered_provider() {
    let provider = Arc::new(TestProvider::new(ProviderTag(*b"FIND")));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(provider)
        .build()
        .unwrap();
    assert_eq!(
        shared
            .index_provider_by_tag(ProviderTag(*b"FIND"))
            .unwrap()
            .provider_tag(),
        ProviderTag(*b"FIND")
    );
}

#[test]
fn index_provider_by_tag_returns_none_for_unknown_tag() {
    let shared = SharedGraph::builder(GraphId::new(1)).build().unwrap();
    assert!(
        shared
            .index_provider_by_tag(ProviderTag(*b"MISS"))
            .is_none()
    );
}
