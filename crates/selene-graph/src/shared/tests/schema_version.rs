use super::*;

#[test]
fn shared_graph_schema_version_initial_zero() {
    let shared = SharedGraph::new(GraphId::new(101));
    assert_eq!(shared.schema_version(), 0);
}

#[test]
fn schema_changed_commit_bumps_version() {
    let graph_type = GraphTypeDef {
        name: db_string("schema.version.type").unwrap(),
        node_types: Vec::new(),
        edge_types: Vec::new(),
    };
    let shared = SharedGraph::builder(GraphId::new(102))
        .bound_to(graph_type)
        .unwrap()
        .build()
        .unwrap();
    let label = db_string("SchemaVersioned").unwrap();
    let mut txn = shared.begin_write();
    txn.mutator()
        .create_node_type(
            label.clone(),
            LabelSet::single(label),
            Vec::new(),
            crate::ValidationMode::Strict,
        )
        .expect("schema mutation succeeds");

    txn.commit().expect("schema commit succeeds");

    assert_eq!(shared.schema_version(), 1);
}

#[test]
fn data_changed_commit_does_not_bump_version() {
    let shared = SharedGraph::new(GraphId::new(103));
    let mut txn = shared.begin_write();
    txn.mutator()
        .create_node(
            LabelSet::single(db_string("schema.version.data").unwrap()),
            PropertyMap::new(),
        )
        .expect("data mutation succeeds");

    txn.commit().expect("data commit succeeds");

    assert_eq!(shared.schema_version(), 0);
}

#[test]
fn direct_create_property_index_bumps_schema_version() {
    let shared = SharedGraph::new(GraphId::new(105));
    shared
        .create_property_index(
            db_string("Person").unwrap(),
            db_string("age").unwrap(),
            TypedIndexKind::I64,
        )
        .expect("create index succeeds");

    assert_eq!(shared.schema_version(), 1);
}

#[test]
fn direct_drop_property_index_bumps_schema_version_when_present() {
    let shared = SharedGraph::new(GraphId::new(106));
    let label = db_string("Person").unwrap();
    let property = db_string("age").unwrap();
    shared
        .create_property_index(label.clone(), property.clone(), TypedIndexKind::I64)
        .expect("create index succeeds");

    shared
        .drop_property_index(label, property)
        .expect("drop index succeeds");

    assert_eq!(shared.schema_version(), 2);
}

#[test]
fn direct_drop_property_index_idempotent_does_not_bump() {
    let shared = SharedGraph::new(GraphId::new(107));
    shared
        .drop_property_index(db_string("Person").unwrap(), db_string("age").unwrap())
        .expect("absent drop succeeds");

    assert_eq!(shared.schema_version(), 0);
}

#[test]
fn schema_version_bump_implies_snapshot_already_reflects_change() {
    // GRAPH-12: store-before-schema-bump ordering. `publish_appended` stores the
    // new snapshot via `snapshot.store(..)` and only THEN bumps `schema_version`
    // (`fetch_add` strictly after the store). So once the bumped epoch is
    // observable, the bumping commit's snapshot MUST already be observable too —
    // a planner that re-plans on `schema_version()==N+1` can never load a
    // pre-change snapshot. Reverse ordering (bump-before-store) would let this
    // assertion observe epoch 1 with the index still absent.
    let shared = SharedGraph::new(GraphId::new(120));
    let label = db_string("Order").unwrap();
    let property = db_string("age").unwrap();
    assert_eq!(shared.schema_version(), 0);
    assert!(
        shared
            .read()
            .property_index_for(&label, &property)
            .is_none()
    );

    shared
        .create_property_index(label.clone(), property.clone(), TypedIndexKind::I64)
        .expect("index create");

    // The schema epoch advanced AND the snapshot that caused it is the published
    // one — the index is present in the same snapshot whose publish bumped the
    // epoch. (publish_appended: store, then fetch_add — never the reverse.)
    assert_eq!(shared.schema_version(), 1);
    assert!(
        shared
            .read()
            .property_index_for(&label, &property)
            .is_some(),
        "schema_version()==1 must imply the bumping commit's snapshot is visible",
    );
}

#[test]
fn concurrent_reader_never_sees_bumped_epoch_without_the_change() {
    // GRAPH-12 reader-loop: while a writer performs a sequence of schema-bumping
    // index creations, a reader spins reading (schema_version, snapshot) and
    // asserts the store-before-bump invariant on every observation: the number of
    // property indexes present in the snapshot is always >= the bumped epoch it
    // co-observes is consistent with. Concretely, each create_property_index bumps
    // the epoch by exactly 1 AND adds exactly 1 index, both published in one
    // store, so an observer that reads epoch E must see at least E indexes in the
    // SAME snapshot it loads right after. A bump-before-store regression would let
    // the reader catch epoch E with < E indexes present.
    let shared = Arc::new(SharedGraph::new(GraphId::new(121)));
    let label = db_string("ConcurrentOrder").unwrap();
    const CREATES: u64 = 32;

    thread::scope(|scope| {
        let reader_graph = Arc::clone(&shared);
        let reader = scope.spawn(move || {
            for _ in 0..20_000 {
                // Load the epoch FIRST, then the snapshot: if the epoch were
                // bumped before the store, this ordering would expose a snapshot
                // missing the change for the observed epoch.
                let epoch = reader_graph.schema_version();
                let snapshot = reader_graph.read();
                let present = snapshot.property_index_count() as u64;
                assert!(
                    present >= epoch,
                    "observed epoch {epoch} but snapshot has only {present} indexes \
                     — store-before-schema-bump violated",
                );
            }
        });
        for i in 0..CREATES {
            shared
                .create_property_index(
                    label.clone(),
                    db_string(format!("prop.{i}").as_str()).unwrap(),
                    TypedIndexKind::I64,
                )
                .expect("index create");
        }
        reader.join().unwrap();
    });

    assert_eq!(shared.schema_version(), CREATES);
    assert_eq!(shared.read().property_index_count() as u64, CREATES);
}

#[test]
fn failed_commit_does_not_bump_schema_version() {
    let shared = SharedGraph::new(GraphId::new(108));
    let mut txn = shared.begin_write();
    txn.mutator().schema_change(SchemaChange::GraphCreated {
        id: GraphId::new(109),
        name: db_string("failed.schema.commit").unwrap(),
        graph_type: None,
    });

    let mut sealed = txn.seal(None, None).unwrap();
    sealed.fail_publish = true;
    assert!(matches!(
        shared.submit_sealed_for_test(sealed),
        Err(GraphError::IndeterminateOutcome { reason })
            if reason.contains("synthetic memory publication failure")
    ));
    assert_eq!(shared.schema_version(), 0);
}
