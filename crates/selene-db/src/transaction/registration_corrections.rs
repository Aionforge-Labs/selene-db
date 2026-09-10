//! Real facade regressions for the pre-delivery A/B correction.

use super::fixture;
use crate::*;

fn index(state: DeclarationState, kind: ScalarIndexKind) -> DeclarationDefinition {
    DeclarationDefinition::Index(IndexDeclaration {
        metadata: DeclarationMetadata::new(state),
        target: PropertyTarget {
            element: ElementKind::Node,
            label: "Person".into(),
            properties: vec!["age".into()],
        },
        configuration: IndexConfiguration::Property(vec![kind]),
    })
}

fn native(metadata: DeclarationMetadata) -> DeclarationDefinition {
    DeclarationDefinition::Native(NativeDeclaration {
        metadata,
        binding: NativeBinding::Projection(NativeProjection {
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        }),
    })
}

#[test]
fn every_create_policy_rejects_cross_kind_without_publication_or_allocation() {
    let definitions = [
        index(DeclarationState::Inactive, ScalarIndexKind::I64),
        native(DeclarationMetadata::new(DeclarationState::Inactive)),
        DeclarationDefinition::Constraint(ConstraintDeclaration {
            metadata: DeclarationMetadata::new(DeclarationState::Inactive),
            target: PropertyTarget {
                element: ElementKind::Node,
                label: "Person".into(),
                properties: vec!["age".into()],
            },
            declaring_type: "Person".into(),
            kind: ConstraintKind::Unique,
            backing_index: None,
        }),
    ];
    for (existing_kind, existing) in definitions.iter().enumerate() {
        for (requested_kind, requested) in definitions.iter().enumerate() {
            if existing_kind == requested_kind {
                continue;
            }
            for policy in [
                CreatePolicy::Strict,
                CreatePolicy::IfNotExists,
                CreatePolicy::OrReplace,
            ] {
                let (database, _, path) = fixture();
                let name = PathSegment::regular("shared").unwrap();
                database
                    .catalog()
                    .declare(&path, &name, existing.clone(), CreatePolicy::Strict)
                    .unwrap();
                let before = database.catalog().snapshot();
                let error = database
                    .catalog()
                    .declare(&path, &name, requested.clone(), policy)
                    .unwrap_err();
                assert_eq!(
                    error.kind(),
                    ErrorKind::CatalogObjectWrongKind,
                    "{existing_kind} -> {requested_kind}, {policy:?}"
                );
                assert_eq!(error.gqlstatus(), Some(GqlStatus::INVALID_REFERENCE));
                let after = database.catalog().snapshot();
                assert!(before.shares_state_with(&after));
                assert_eq!(before.state.high_water, after.state.high_water);
            }
        }
    }
}

#[test]
fn same_kind_policies_preserve_noop_and_fresh_replacement_behavior() {
    let (database, _, path) = fixture();
    let name = PathSegment::regular("future").unwrap();
    let definition = index(DeclarationState::Inactive, ScalarIndexKind::I64);
    let CreateOutcome::Created(first) = database
        .catalog()
        .declare(&path, &name, definition.clone(), CreatePolicy::Strict)
        .unwrap()
    else {
        panic!("created")
    };
    let before = database.catalog().snapshot();
    assert!(matches!(
        database
            .catalog()
            .declare(&path, &name, definition.clone(), CreatePolicy::IfNotExists)
            .unwrap(),
        CreateOutcome::AlreadyExists(_)
    ));
    assert!(before.shares_state_with(&database.catalog().snapshot()));
    let CreateOutcome::Replaced { dropped, created } = database
        .catalog()
        .declare(&path, &name, definition, CreatePolicy::OrReplace)
        .unwrap()
    else {
        panic!("replaced")
    };
    assert_eq!(dropped.id, first.id);
    assert_ne!(created.id, first.id);
}

#[test]
fn native_drop_removes_only_its_bound_id_and_preserves_future_dependants() {
    for dependent in [false, true] {
        let (database, _, path) = fixture();
        let session = database.session(&path).unwrap();
        session
            .execute("CALL selene.create_index('Person', 'age', 'i64')")
            .unwrap();
        let name = PathSegment::regular("future").unwrap();
        let CreateOutcome::Created(future) = database
            .catalog()
            .declare(
                &path,
                &name,
                index(DeclarationState::Inactive, ScalarIndexKind::F64),
                CreatePolicy::Strict,
            )
            .unwrap()
        else {
            panic!("created")
        };
        if dependent {
            let mut metadata = DeclarationMetadata::new(DeclarationState::Inactive);
            metadata.dependencies.push(future.dependency().unwrap());
            database
                .catalog()
                .declare(
                    &path,
                    &PathSegment::regular("dependant").unwrap(),
                    native(metadata),
                    CreatePolicy::Strict,
                )
                .unwrap();
        }
        session.execute("START TRANSACTION").unwrap();
        session
            .execute("CALL selene.drop_index('Person', 'age')")
            .unwrap();
        session.execute("COMMIT").unwrap();
        let declarations = database.catalog().snapshot().declarations(&path).unwrap();
        assert_eq!(declarations.len(), if dependent { 2 } else { 1 });
        assert!(declarations.contains(&future));
        assert!(
            database
                .catalog()
                .snapshot()
                .state
                .graphs
                .values()
                .next()
                .unwrap()
                .graph
                .read()
                .property_index
                .is_empty()
        );
    }
}

#[test]
fn actual_backing_dependant_restricts_native_drop_with_gqlstatus() {
    let (database, _, path) = fixture();
    let session = database.session(&path).unwrap();
    session
        .execute("CALL selene.create_index('Person', 'age', 'i64')")
        .unwrap();
    let backing = database
        .catalog()
        .snapshot()
        .declarations(&path)
        .unwrap()
        .remove(0);
    let mut metadata = DeclarationMetadata::new(DeclarationState::Inactive);
    metadata.dependencies.push(backing.dependency().unwrap());
    database
        .catalog()
        .declare(
            &path,
            &PathSegment::regular("dependent").unwrap(),
            native(metadata),
            CreatePolicy::Strict,
        )
        .unwrap();
    let before = database.catalog().snapshot();
    let error = session
        .execute("CALL selene.drop_index('Person', 'age')")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::CatalogRestrictViolation);
    assert_eq!(error.gqlstatus(), Some(GqlStatus::DEPENDENT_OBJECT_ERROR));
    assert!(before.shares_state_with(&database.catalog().snapshot()));
}

#[test]
fn inactive_alternative_with_identical_target_and_config_is_not_a_binding() {
    for state in [
        DeclarationState::Inactive,
        DeclarationState::Building,
        DeclarationState::Failed,
    ] {
        let (database, _, path) = fixture();
        let session = database.session(&path).unwrap();
        session
            .execute("CALL selene.create_index('Person', 'age', 'i64')")
            .unwrap();
        let CreateOutcome::Created(future) = database
            .catalog()
            .declare(
                &path,
                &PathSegment::regular("future").unwrap(),
                index(state, ScalarIndexKind::I64),
                CreatePolicy::Strict,
            )
            .unwrap()
        else {
            panic!("created")
        };
        session
            .execute("CALL selene.drop_index('Person', 'age')")
            .unwrap();
        assert_eq!(
            database.catalog().snapshot().declarations(&path).unwrap(),
            [future]
        );
    }
}
