use super::*;
use crate::{GraphTypeDefinition, NodeTypeDefinition, PathSegment};

fn bound(database: &Database, graph: &ObjectPath) {
    let ty = ObjectPath::regular("selene", "durable", "BaseShape").unwrap();
    let definition = GraphTypeDefinition::builder()
        .with_node_type(
            NodeTypeDefinition::new(
                PathSegment::regular("Base").unwrap(),
                vec![PathSegment::regular("Base").unwrap()],
            )
            .unwrap(),
        )
        .build()
        .unwrap();
    database
        .catalog()
        .create_graph_type(&ty, definition, CreatePolicy::Strict)
        .unwrap();
    database
        .catalog()
        .create_graph(graph, Some(&ty), CreatePolicy::OrReplace)
        .unwrap();
}

#[test]
fn both_modes_reject_data_outside_named_type_without_removing_unused_instance_types() {
    for durable in [false, true] {
        let (_temp, dir, disk, seed, path) = fixture();
        let database = if durable {
            disk
        } else {
            let memory = Database::builder().build();
            memory
                .catalog()
                .create_schema(&path.schema_path(), CreatePolicy::Strict)
                .unwrap();
            memory
                .catalog()
                .create_graph(&path, None, CreatePolicy::Strict)
                .unwrap();
            memory
        };
        bound(&database, &path);
        let session = database.session(&path).unwrap();
        session
            .execute("CREATE NODE TYPE :Sensor (serial :: STRING UNIQUE)")
            .unwrap();
        session.execute("CREATE NODE TYPE :LocalOnly ()").unwrap();
        let named = database.inner.state.load_full().graph_types.clone();
        session.execute("INSERT (:Base)").unwrap();
        let before = database.inner.state.load_full();
        let error = session
            .execute("INSERT (:Sensor {serial: 'A'})")
            .unwrap_err();
        assert_eq!(error.gqlstatus().unwrap().as_str(), "G2000");
        assert!(Arc::ptr_eq(&before, &database.inner.state.load_full()));
        assert_eq!(named, database.inner.state.load_full().graph_types);
        session
            .start_transaction(TransactionAccessMode::ReadWrite)
            .unwrap();
        session.execute("INSERT (:Base)").unwrap();
        assert_eq!(
            session
                .execute("INSERT (:Sensor)")
                .unwrap_err()
                .gqlstatus()
                .unwrap()
                .as_str(),
            "G2000"
        );
        session.rollback_transaction().unwrap();
        assert!(Arc::ptr_eq(&before, &database.inner.state.load_full()));
        if durable {
            let (state, _) = replay(&dir, seed);
            let id = database
                .catalog()
                .snapshot()
                .resolve_graph(&path)
                .unwrap()
                .id
                .get();
            assert_eq!(state.graph_summary(GraphId::new(id)).unwrap().0, 1);
        }
    }
}

#[test]
fn named_required_properties_endpoints_and_immutable_operations_survive_local_relaxation() {
    for durable in [false, true] {
        let (_temp, dir, disk, seed, path) = fixture();
        let database = if durable {
            disk
        } else {
            let memory = Database::builder().build();
            memory
                .catalog()
                .create_schema(&path.schema_path(), CreatePolicy::Strict)
                .unwrap();
            memory
                .catalog()
                .create_graph(&path, None, CreatePolicy::Strict)
                .unwrap();
            memory
        };
        let ty = ObjectPath::regular("selene", "durable", "Rich").unwrap();
        crate::transaction::test_schema::bind_schema(
            &database,
            &path,
            &ty,
            &[
                "CREATE NODE TYPE :Sensor (serial :: STRING NOT NULL IMMUTABLE, age :: INT64)",
                "CREATE EDGE TYPE :LINK (FROM :Sensor TO :Sensor, weight :: INT64)",
            ],
        );
        let session = database.session(&path).unwrap();
        // Relax the instance only. Named property/endpoint/operation rules remain.
        session.execute("DROP EDGE TYPE :LINK").unwrap();
        session.execute("DROP NODE TYPE :Sensor").unwrap();
        session
            .execute("CREATE NODE TYPE :Sensor (serial :: STRING, age :: INT64)")
            .unwrap();
        session
            .execute("CREATE EDGE TYPE :LINK (weight :: INT64)")
            .unwrap();
        session.execute("INSERT (a:Sensor {serial: 'A', age: 1})-[:LINK {weight: 2}]->(b:Sensor {serial: 'B', age: 2})").unwrap();
        session.execute("MATCH (n:Sensor) SET n.age = 3").unwrap();
        session
            .execute("MATCH ()-[e:LINK]->() SET e.weight = 4")
            .unwrap();
        // The current endpoint-only type accepts both intrinsic directions;
        // preserve mixed-edge support rather than inventing a directed-only type.
        session
            .execute("INSERT (:Sensor {serial: 'C'})~[:LINK]~(:Sensor {serial: 'D'})")
            .unwrap();
        let before = database.inner.state.load_full();
        for source in [
            "INSERT (:Sensor)",
            "INSERT (:Sensor {serial: 42})",
            "INSERT (:Base)-[:LINK]->(:Sensor {serial: 'C'})",
            "MATCH (n:Sensor) SET n.serial = 'C'",
        ] {
            assert_eq!(
                session
                    .execute(source)
                    .unwrap_err()
                    .gqlstatus()
                    .unwrap()
                    .as_str(),
                "G2000",
                "{source}"
            );
            assert!(Arc::ptr_eq(&before, &database.inner.state.load_full()));
        }
        if durable {
            let (state, _) = replay(&dir, seed);
            let id = database
                .catalog()
                .snapshot()
                .resolve_graph(&path)
                .unwrap()
                .id
                .get();
            assert_eq!(state.graph_summary(GraphId::new(id)).unwrap().0, 4);
            assert_eq!(state.graph_summary(GraphId::new(id)).unwrap().1, 2);
        }
    }
}

#[test]
fn changed_named_definition_revalidates_retained_graph_and_preserves_semantic_cause() {
    use selene_catalog::{CatalogDescriptor, CatalogObjectId, CatalogTransaction};
    for durable in [false, true] {
        let (_temp, _dir, disk, _seed, path) = fixture();
        let database = if durable {
            disk
        } else {
            let memory = Database::builder().build();
            memory
                .catalog()
                .create_schema(&path.schema_path(), CreatePolicy::Strict)
                .unwrap();
            memory
                .catalog()
                .create_graph(&path, None, CreatePolicy::Strict)
                .unwrap();
            memory
        };
        bound(&database, &path);
        database
            .session(&path)
            .unwrap()
            .execute("INSERT (:Base)")
            .unwrap();
        let before = database.inner.state.load_full();
        let error = database.inner.with_mutation_reservation(|reservation| {
            let mut draft = DatabaseDraft::new(&before, &reservation);
            let id = *draft.graph_types.first_key_value().unwrap().0;
            let named = Arc::make_mut(draft.graph_types.get_mut(&id).unwrap());
            named.node_types[0].key_labels =
                selene_core::LabelSet::from_iter([selene_core::db_string("Other").unwrap()]);
            let old = draft
                .catalog
                .descriptor(CatalogObjectId::GraphType(id))
                .unwrap();
            let mut change = CatalogTransaction::new(&draft.catalog).unwrap();
            change.remove(old.id());
            change
                .insert(
                    CatalogDescriptor::new(
                        old.id(),
                        old.kind(),
                        old.name().clone(),
                        old.parent(),
                        change.generation(),
                        old.creation().clone(),
                        old.payload().clone(),
                    )
                    .unwrap(),
                )
                .unwrap();
            draft.catalog = change.build().unwrap();
            draft.mark_modified();
            database
                .inner
                .publish_database_draft(reservation, draft)
                .unwrap_err()
        });
        assert!(Arc::ptr_eq(&before, &database.inner.state.load_full()));
        if durable {
            assert_eq!(error.gqlstatus().unwrap().as_str(), "40N01");
            assert_eq!(
                error.durable_commit_outcome().unwrap().state,
                DurableCommitState::Canceled
            );
            let mut source = std::error::Error::source(&error);
            let mut semantic = false;
            while let Some(cause) = source {
                if let Some(cause) = cause.downcast_ref::<Error>() {
                    semantic |= cause.gqlstatus().is_some_and(|s| s.as_str() == "G2000");
                }
                source = cause.source();
            }
            assert!(semantic);
            let diagnostic = crate::DiagnosticBundle::from_error_and_engine_statuses(&error, &[]);
            assert_eq!(diagnostic.primary().status().as_str(), "40N01");
            assert!(
                diagnostic
                    .primary()
                    .causes()
                    .iter()
                    .any(|cause| cause.status().as_str() == "G2000")
            );
        } else {
            assert_eq!(error.gqlstatus().unwrap().as_str(), "G2000");
        }
    }
}
