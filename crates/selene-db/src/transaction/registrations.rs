use selene_catalog::CatalogObjectKind;

use super::fixture;

#[test]
fn ready_index_names_are_graph_scoped_and_duplicate_failure_is_atomic() {
    use crate::{CreatePolicy, ErrorKind, ObjectPath};
    let (database, _, first) = fixture();
    let second = ObjectPath::regular("selene", "authority", "second").unwrap();
    database
        .catalog()
        .create_graph(&second, None, CreatePolicy::Strict)
        .unwrap();
    for path in [&first, &second] {
        database
            .session(path)
            .unwrap()
            .execute("CALL selene.create_vector_index('Doc', 'embedding', 3, 'flat', 'shared')")
            .unwrap();
    }
    let before = database.catalog().snapshot();
    assert_eq!(
        before.declarations(&first).unwrap()[0].name.display(),
        "shared"
    );
    assert_eq!(
        before.declarations(&second).unwrap()[0].name.display(),
        "shared"
    );
    let error = database
        .session(&first)
        .unwrap()
        .execute("CALL selene.create_vector_index('Doc', 'other', 3, 'flat', 'shared')")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::CatalogObjectAlreadyExists);
    assert!(before.shares_state_with(&database.catalog().snapshot()));
}

#[test]
fn facade_only_dependency_inputs_restrict_shared_owner_removal() {
    use crate::*;
    let (database, _, first) = fixture();
    let second = ObjectPath::regular("selene", "authority", "dependent").unwrap();
    database
        .catalog()
        .create_graph(&second, None, CreatePolicy::Strict)
        .unwrap();
    let name = PathSegment::regular("lookup").unwrap();
    let definition = DeclarationDefinition::Index(IndexDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Inactive),
        target: PropertyTarget {
            element: ElementKind::Node,
            label: "Document".into(),
            properties: vec!["age".into()],
        },
        configuration: IndexConfiguration::Property(vec![ScalarIndexKind::I64]),
    });
    let CreateOutcome::Created(shared) = database
        .catalog()
        .declare(&first, &name, definition, CreatePolicy::Strict)
        .unwrap()
    else {
        panic!("created")
    };
    let mut metadata = DeclarationMetadata::new(DeclarationState::Inactive);
    metadata.dependencies.push(shared.dependency().unwrap());
    database
        .catalog()
        .declare(
            &second,
            &name,
            DeclarationDefinition::Native(NativeDeclaration {
                metadata,
                binding: NativeBinding::Projection(NativeProjection {
                    node_labels: vec![],
                    edge_labels: vec![],
                    weight_property: None,
                }),
            }),
            CreatePolicy::Strict,
        )
        .unwrap();
    let before = database.catalog().snapshot();
    assert_eq!(
        database
            .catalog()
            .drop_graph(&first, DropPolicy::Strict)
            .unwrap_err()
            .kind(),
        ErrorKind::CatalogRestrictViolation
    );
    assert!(before.shares_state_with(&database.catalog().snapshot()));
    database
        .catalog()
        .drop_graph(&second, DropPolicy::Strict)
        .unwrap();
    database
        .catalog()
        .drop_graph(&first, DropPolicy::Strict)
        .unwrap();
}

#[test]
fn frozen_native_inventory_matches_runtime_and_read_calls_do_not_publish() {
    use crate::{DeclarationDefinition, NativeBinding, NativeDefault, NativeEffect, NativeType};
    use selene_gql::ProcedureRegistry;
    let (database, _, path) = fixture();
    let before = database.catalog().snapshot();
    let declarations = before.native_procedures();
    let registry = &database.catalog().inner;
    assert_eq!(
        declarations.len(),
        registry.procedures.iter_handles().count()
    );
    for (_, metadata) in registry.procedures.iter_handles() {
        assert!(declarations.iter().any(|declaration| {
            let DeclarationDefinition::Native(native) = &declaration.definition else {
                return false;
            };
            let NativeBinding::Procedure(procedure) = &native.binding else {
                return false;
            };
            procedure.description == metadata.description
                && procedure.parameters.len() == metadata.signature.parameters.len()
                && procedure.outputs.len() == metadata.output_schema.columns.len()
        }));
    }
    let rrf = declarations
        .iter()
        .find(|declaration| declaration.name.display() == "selene.reciprocal_rank_fusion")
        .unwrap();
    let DeclarationDefinition::Native(native) = &rrf.definition else {
        panic!("native")
    };
    let NativeBinding::Procedure(procedure) = &native.binding else {
        panic!("procedure")
    };
    assert_eq!(procedure.effect, NativeEffect::GraphRead);
    assert_eq!(procedure.parameters[2].field.ty, NativeType::Float64);
    assert_eq!(
        procedure.parameters[2].default,
        Some(NativeDefault::Integer(60))
    );
    assert_eq!(procedure.outputs[0].name, "node_id");
    database
        .session(&path)
        .unwrap()
        .execute("CALL selene.health()")
        .unwrap();
    assert!(before.shares_state_with(&database.catalog().snapshot()));
    let logical = before.logical_catalog().unwrap();
    assert_eq!(
        logical.descriptors().len(),
        before.state.catalog.descriptors().count()
    );
    assert_eq!(
        logical.reconstruct().unwrap().descriptors().count(),
        logical.descriptors().len()
    );
}

#[test]
fn unsupported_native_and_expression_activation_publish_nothing() {
    use crate::*;
    let (database, _, path) = fixture();
    let before = database.catalog().snapshot();
    let definition = DeclarationDefinition::Native(NativeDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Ready),
        binding: NativeBinding::Projection(NativeProjection {
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        }),
    });
    let error = database
        .catalog()
        .declare(
            &path,
            &PathSegment::regular("projection").unwrap(),
            definition,
            CreatePolicy::Strict,
        )
        .unwrap_err();
    assert_eq!(error.gqlstatus(), Some(GqlStatus::FEATURE_NOT_SUPPORTED));
    assert!(
        IndexDeclaration::for_expression(ReservedIndexExpression::Property("age".into())).is_err()
    );
    assert!(before.shares_state_with(&database.catalog().snapshot()));
}

#[test]
fn index_rollback_failure_and_acknowledgement_preserve_one_outer_publication() {
    use crate::{ErrorKind, catalog::FailurePoint};
    let (database, _, path) = fixture();
    let session = database.session(&path).unwrap();
    let before = database.catalog().snapshot();
    session.execute("START TRANSACTION").unwrap();
    session
        .execute("CALL selene.create_index('Person', 'age', 'i64')")
        .unwrap();
    assert!(before.shares_state_with(&database.catalog().snapshot()));
    session.execute("ROLLBACK").unwrap();
    assert!(before.shares_state_with(&database.catalog().snapshot()));
    session.execute("START TRANSACTION").unwrap();
    session
        .execute("CALL selene.create_index('Person', 'age', 'i64')")
        .unwrap();
    assert!(
        session
            .execute("CALL selene.create_index('Person', 'age', 'i64')")
            .is_err()
    );
    session.execute("ROLLBACK").unwrap();
    assert!(before.shares_state_with(&database.catalog().snapshot()));
    *database.catalog().inner.failure.lock() = Some(FailurePoint::BeforePublication);
    assert_eq!(
        session
            .execute("CALL selene.create_index('Person', 'age', 'i64')")
            .unwrap_err()
            .kind(),
        ErrorKind::MutationCanceled
    );
    assert!(before.shares_state_with(&database.catalog().snapshot()));
    assert_eq!(
        before.state.high_water,
        database.catalog().snapshot().state.high_water
    );
    *database.catalog().inner.failure.lock() = Some(FailurePoint::AfterPublicationAcknowledgement);
    assert_eq!(
        session
            .execute("CALL selene.create_index('Person', 'age', 'i64')")
            .unwrap_err()
            .kind(),
        ErrorKind::MutationIndeterminate
    );
    let after = database.catalog().snapshot();
    assert_eq!(after.declarations(&path).unwrap().len(), 1);
    assert_eq!(after.state.high_water.index, 1);
    let graph = after.state.graphs.values().next().unwrap().graph.read();
    assert!(
        graph
            .property_index_for(
                &selene_core::db_string("Person").unwrap(),
                &selene_core::db_string("age").unwrap()
            )
            .is_some()
    );
}

#[test]
fn pure_data_publication_preserves_declarations_and_drift_declines_planner() {
    use selene_core::db_string;
    use selene_gql::plan::optimize::{IndexCatalog, IndexTarget, LiveIndexCatalog};
    let (database, _, path) = fixture();
    let session = database.session(&path).unwrap();
    session
        .execute("CALL selene.create_index('Person', 'age', 'i64')")
        .unwrap();
    let before = database.catalog().snapshot();
    session
        .execute("INSERT (:Person {age: 42}), (:Person {age: 42.0})")
        .unwrap();
    let after = database.catalog().snapshot();
    assert_eq!(after.generation(), before.generation());
    assert_eq!(
        after.declarations(&path).unwrap(),
        before.declarations(&path).unwrap()
    );
    assert!(after.logical_catalog_changes_from(&before).is_empty());
    let graph = after.state.graphs.values().next().unwrap().graph.read();
    let planner = LiveIndexCatalog::new(graph.clone());
    assert!(
        planner
            .typed_index(
                IndexTarget::Node,
                db_string("Person").unwrap(),
                db_string("age").unwrap()
            )
            .is_none()
    );
    assert_eq!(
        session
            .execute("MATCH (n:Person) WHERE n.age = 42 RETURN n.age")
            .unwrap()
            .row_count(),
        Some(2)
    );
    session
        .execute("CALL selene.drop_index('Person', 'age')")
        .unwrap();
    assert_eq!(
        session
            .execute("MATCH (n:Person) WHERE n.age = 42 RETURN n.age")
            .unwrap()
            .row_count(),
        Some(2)
    );
}

#[test]
fn supported_vector_and_text_index_calls_have_matching_declarations() {
    let (database, _, path) = fixture();
    let session = database.session(&path).unwrap();
    session
        .execute("CALL selene.create_vector_index('Doc', 'embedding', 3, 'flat')")
        .unwrap();
    session
        .execute("CALL selene.create_text_index('Doc', 'body')")
        .unwrap();
    let snapshot = database.catalog().snapshot();
    let graph = snapshot.state.graphs.values().next().unwrap().graph.read();
    for descriptor in snapshot.state.catalog.descriptors() {
        if let selene_catalog::CatalogPayload::Index(index) = descriptor.payload() {
            assert!(graph.matches_index_declaration(index));
        }
    }
    assert_eq!(snapshot.declarations(&path).unwrap().len(), 2);
    session
        .execute("CALL selene.drop_vector_index('Doc', 'embedding')")
        .unwrap();
    session
        .execute("CALL selene.drop_text_index('Doc', 'body')")
        .unwrap();
    assert!(
        database
            .catalog()
            .snapshot()
            .declarations(&path)
            .unwrap()
            .is_empty()
    );
    assert_eq!(snapshot.declarations(&path).unwrap().len(), 2);
}

#[test]
fn inactive_declarations_are_owner_scoped_fresh_on_replace_and_cleaned_on_owner_drop() {
    use crate::*;
    let (database, _, first) = fixture();
    let second = ObjectPath::regular("selene", "authority", "other").unwrap();
    database
        .catalog()
        .create_graph(&second, None, CreatePolicy::Strict)
        .unwrap();
    let definition = DeclarationDefinition::Native(NativeDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Inactive),
        binding: NativeBinding::Projection(NativeProjection {
            node_labels: vec!["CaseSensitiveLabel".into()],
            edge_labels: vec![],
            weight_property: None,
        }),
    });
    let name = PathSegment::regular("projection").unwrap();
    let first_decl = database
        .catalog()
        .declare(&first, &name, definition.clone(), CreatePolicy::Strict)
        .unwrap();
    database
        .catalog()
        .declare(&second, &name, definition.clone(), CreatePolicy::Strict)
        .unwrap();
    assert_eq!(
        database
            .catalog()
            .declare(&first, &name, definition.clone(), CreatePolicy::Strict)
            .unwrap_err()
            .kind(),
        ErrorKind::CatalogObjectAlreadyExists
    );
    let old = database.catalog().snapshot();
    let replaced = database
        .catalog()
        .declare(&first, &name, definition, CreatePolicy::OrReplace)
        .unwrap();
    let (CreateOutcome::Created(first_decl), CreateOutcome::Replaced { created, .. }) =
        (first_decl, replaced)
    else {
        panic!("lifecycle outcomes")
    };
    assert_ne!(first_decl.id, created.id);
    database
        .catalog()
        .drop_graph(&first, DropPolicy::Strict)
        .unwrap();
    assert_eq!(
        database
            .catalog()
            .snapshot()
            .declarations(&second)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(old.declarations(&first).unwrap().len(), 1);
}

#[test]
fn selected_unique_rule_is_declared_and_remains_enforced_after_index_drop() {
    use crate::{CreatePolicy, GraphTypeDefinition, NodeTypeDefinition, ObjectPath, PathSegment};
    let (database, _, path) = fixture();
    let ty = ObjectPath::regular("selene", "authority", "closed").unwrap();
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
        .create_graph(&path, Some(&ty), CreatePolicy::OrReplace)
        .unwrap();
    let session = database.session(&path).unwrap();
    session
        .execute("CREATE NODE TYPE :Sensor (serial :: STRING UNIQUE, age :: INT64)")
        .unwrap();
    let state = database.catalog().snapshot();
    assert_eq!(
        state
            .state
            .catalog
            .descriptors()
            .filter(|d| d.kind() == CatalogObjectKind::Constraint)
            .count(),
        1
    );
    session.execute("INSERT (:Sensor {serial: 'A'})").unwrap();
    assert!(session.execute("INSERT (:Sensor {serial: 'A'})").is_err());
    session
        .execute("CALL selene.create_index('Sensor', 'serial', 'string')")
        .unwrap();
    session
        .execute("CALL selene.drop_index('Sensor', 'serial')")
        .unwrap();
    session
        .execute("CREATE INDEX serial_age ON :Sensor(serial, age)")
        .unwrap();
    let composite_state = database.catalog().snapshot();
    let graph = composite_state
        .state
        .graphs
        .values()
        .next()
        .unwrap()
        .graph
        .read();
    let composite = composite_state
        .state
        .catalog
        .descriptors()
        .find_map(|descriptor| match descriptor.payload() {
            selene_catalog::CatalogPayload::Index(index) => Some(index),
            _ => None,
        })
        .unwrap();
    assert_eq!(composite.target.properties, ["serial", "age"]);
    assert!(graph.matches_index_declaration(composite));
    assert!(session.execute("INSERT (:Sensor {serial: 'A'})").is_err());
    session
        .execute("INSERT (:Sensor {serial: NULL}), (:Sensor), (:Sensor {serial: NULL})")
        .unwrap();
}

#[test]
fn failed_declaration_is_not_a_planner_access_path() {
    use selene_catalog::{CatalogDescriptor, CatalogPayload, CatalogTransaction, DeclarationState};
    use selene_core::db_string;
    use selene_gql::plan::optimize::{IndexCatalog, IndexTarget, LiveIndexCatalog};
    use std::sync::Arc;

    let (database, _, path) = fixture();
    let session = database.session(&path).unwrap();
    session
        .execute("CALL selene.create_index('Person', 'age', 'i64')")
        .unwrap();
    let state = database.catalog().inner.state.load_full();
    let descriptor = state
        .catalog
        .descriptors()
        .find(|d| d.kind() == CatalogObjectKind::Index)
        .unwrap();
    let mut payload = descriptor.payload().clone();
    let CatalogPayload::Index(index) = &mut payload else {
        panic!("index payload")
    };
    index.metadata.state = DeclarationState::Failed;
    let mut transaction = CatalogTransaction::new(&state.catalog).unwrap();
    transaction.remove(descriptor.id());
    transaction
        .insert(
            CatalogDescriptor::new(
                descriptor.id(),
                descriptor.kind(),
                descriptor.name().clone(),
                descriptor.parent(),
                transaction.generation(),
                descriptor.creation().clone(),
                payload,
            )
            .unwrap(),
        )
        .unwrap();
    let catalog = transaction.build().unwrap();
    let mut graph = state
        .graphs
        .values()
        .next()
        .unwrap()
        .graph
        .read()
        .as_ref()
        .clone();
    graph.bind_catalog(&catalog).unwrap();
    let planner = LiveIndexCatalog::new(Arc::new(graph));
    assert!(
        planner
            .typed_index(
                IndexTarget::Node,
                db_string("Person").unwrap(),
                db_string("age").unwrap()
            )
            .is_none()
    );
}

#[test]
fn native_index_creation_publishes_a_catalog_declaration() {
    let (database, _, path) = fixture();
    let session = database.session(&path).unwrap();
    session.execute("INSERT (:Person {age: 42})").unwrap();
    let before = database.catalog().snapshot();
    session
        .execute("CALL selene.create_index('Person', 'age', 'i64')")
        .unwrap();
    let after = database.catalog().snapshot();
    assert_eq!(
        after
            .state
            .catalog
            .descriptors()
            .filter(|descriptor| descriptor.kind() == CatalogObjectKind::Index)
            .count(),
        1,
        "a usable facade index must have an authoritative declaration"
    );
    assert!(after.generation() > before.generation());
    assert_eq!(
        before
            .state
            .catalog
            .descriptors()
            .filter(|descriptor| descriptor.kind() == CatalogObjectKind::Index)
            .count(),
        0
    );
}
