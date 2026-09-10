//! Independent logical declaration, dependency, and decoding contracts.

use proptest::prelude::*;
use selene_catalog::*;
use selene_core::SchemaPropertyIndexKind;
use std::collections::BTreeMap;

fn gen_at(value: u64) -> CatalogGeneration {
    CatalogGeneration::new(value).unwrap()
}
fn name(value: &str) -> CatalogName {
    CatalogName::regular(value).unwrap()
}
fn created() -> CreationMetadata {
    CreationMetadata::new(gen_at(1), None)
}
fn base() -> CatalogSnapshot {
    let catalog = CatalogId::new(1).unwrap();
    let root = DirectoryId::new(1).unwrap();
    let schema = SchemaId::new(1).unwrap();
    let mut builder = CatalogSnapshotBuilder::new(
        gen_at(1),
        CatalogDescriptor::catalog(catalog, name("selene"), gen_at(1), created()).unwrap(),
        CatalogDescriptor::root_directory(root, catalog, gen_at(1), created()).unwrap(),
    )
    .unwrap();
    builder
        .insert(
            CatalogDescriptor::schema(schema, name("test"), root, gen_at(1), created()).unwrap(),
        )
        .unwrap();
    for id in 1..=2 {
        builder
            .insert(
                CatalogDescriptor::graph(
                    GraphId::new(id).unwrap(),
                    name(&format!("g{id}")),
                    schema,
                    gen_at(1),
                    created(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
    }
    builder.build().unwrap()
}
fn index(raw: u64, owner: u64, dependencies: Vec<DeclarationDependency>) -> CatalogDescriptor {
    let mut metadata = DeclarationMetadata::new(DeclarationState::Inactive);
    metadata.dependencies = dependencies;
    CatalogDescriptor::index(
        IndexId::new(raw).unwrap(),
        name("lookup"),
        CatalogParent::Graph(GraphId::new(owner).unwrap()),
        gen_at(2),
        CreationMetadata::new(gen_at(2), None),
        IndexDeclaration {
            metadata,
            target: PropertyTarget {
                element: ElementKind::Node,
                label: "CaseSensitiveLabel".into(),
                properties: vec!["CaseSensitiveKey".into()],
            },
            configuration: IndexConfiguration::Property(vec![SchemaPropertyIndexKind::I64]),
        },
    )
    .unwrap()
}
fn dependency(id: u64) -> DeclarationDependency {
    DeclarationDependency {
        id: CatalogObjectId::Index(IndexId::new(id).unwrap()),
        generation: gen_at(2),
    }
}

#[test]
fn native_type_depth_is_bounded_during_postcard_decode() {
    // Independent postcard fixture: NativeType::List has discriminant 15,
    // NativeType::Boolean has discriminant 2. Each newtype contains its child.
    let mut at_limit = vec![15; 64];
    at_limit.push(2);
    assert!(postcard::from_bytes::<NativeType>(&at_limit).is_ok());
    let mut over_limit = vec![15; 65];
    over_limit.push(2);
    assert!(postcard::from_bytes::<NativeType>(&over_limit).is_err());
    let mut hostile = vec![15; 10_000];
    hostile.push(2);
    assert!(postcard::from_bytes::<NativeType>(&hostile).is_err());
    let accepted: NativeType = postcard::from_bytes(&at_limit).unwrap();
    assert_eq!(postcard::to_allocvec(&accepted).unwrap(), at_limit);
}

#[test]
fn backing_index_requires_exact_owner_target_and_scalar_kind() {
    let base = base();
    let mut transaction = CatalogTransaction::new(&base).unwrap();
    let descriptor = index(1, 1, vec![]);
    let CatalogPayload::Index(index) = descriptor.payload() else {
        unreachable!()
    };
    let mut metadata = DeclarationMetadata::new(DeclarationState::Inactive);
    metadata.dependencies = vec![dependency(1)];
    let constraint = ConstraintDeclaration {
        metadata,
        target: index.target.clone(),
        declaring_type: "Person".into(),
        kind: ConstraintKind::Unique,
        backing_index: Some(IndexId::new(1).unwrap()),
    };
    transaction
        .insert(
            CatalogDescriptor::constraint(
                ConstraintId::new(1).unwrap(),
                name("unique_rule"),
                CatalogParent::Graph(GraphId::new(2).unwrap()),
                gen_at(2),
                created(),
                constraint,
            )
            .unwrap(),
        )
        .unwrap();
    transaction.insert(descriptor).unwrap();
    assert!(matches!(
        transaction.build(),
        Err(CatalogError::InvalidDependency {
            reason: "wrong_backing_owner_or_target",
            ..
        })
    ));
}

#[test]
fn names_are_owner_scoped_and_shared_dependency_deletion_is_restrict() {
    let base = base();
    let mut transaction = CatalogTransaction::new(&base).unwrap();
    transaction.insert(index(1, 1, vec![])).unwrap();
    transaction
        .insert(index(2, 2, vec![dependency(1)]))
        .unwrap();
    let snapshot = transaction.build().unwrap();
    assert_eq!(
        snapshot
            .declarations(CatalogObjectId::Graph(GraphId::new(1).unwrap()))
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .declarations(CatalogObjectId::Graph(GraphId::new(2).unwrap()))
            .count(),
        1
    );
    let clone = snapshot.clone();
    assert!(clone.shares_state_with(&snapshot));
    assert!(std::ptr::eq(
        clone.descriptor(dependency(1).id).unwrap(),
        snapshot.descriptor(dependency(1).id).unwrap()
    ));
    let mut drop_owner = CatalogTransaction::new(&snapshot).unwrap();
    drop_owner.remove_owner(CatalogObjectId::Graph(GraphId::new(1).unwrap()));
    assert!(matches!(
        drop_owner.build(),
        Err(CatalogError::InvalidDependency {
            reason: "missing_dependency",
            ..
        })
    ));
    let mut drop_own = CatalogTransaction::new(&snapshot).unwrap();
    drop_own.remove_owner(CatalogObjectId::Graph(GraphId::new(2).unwrap()));
    let dropped = drop_own.build().unwrap();
    assert!(dropped.descriptor(dependency(2).id).is_none());
    assert!(snapshot.descriptor(dependency(2).id).is_some());
}

#[test]
fn duplicate_dangling_cycle_and_missing_owner_are_typed_failures() {
    let base = base();
    let mut duplicate = CatalogTransaction::new(&base).unwrap();
    duplicate.insert(index(1, 1, vec![])).unwrap();
    duplicate.insert(index(2, 1, vec![])).unwrap();
    assert!(matches!(
        duplicate.build(),
        Err(CatalogError::DuplicateCanonicalName { .. })
    ));
    let mut dangling = CatalogTransaction::new(&base).unwrap();
    dangling.insert(index(1, 1, vec![dependency(99)])).unwrap();
    assert!(matches!(
        dangling.build(),
        Err(CatalogError::InvalidDependency {
            reason: "missing_dependency",
            ..
        })
    ));
    let mut cyclic = CatalogTransaction::new(&base).unwrap();
    cyclic.insert(index(1, 1, vec![dependency(2)])).unwrap();
    cyclic.insert(index(2, 2, vec![dependency(1)])).unwrap();
    assert!(matches!(
        cyclic.build(),
        Err(CatalogError::InvalidDependency {
            reason: "dependency_cycle",
            ..
        })
    ));
    let mut missing_owner = CatalogTransaction::new(&base).unwrap();
    missing_owner.insert(index(1, 99, vec![])).unwrap();
    assert!(matches!(
        missing_owner.build(),
        Err(CatalogError::MissingParent { .. })
    ));
}

#[test]
fn logical_reconstruction_checks_high_water_and_whole_dependency_graph() {
    let base = base();
    let mut transaction = CatalogTransaction::new(&base).unwrap();
    transaction.insert(index(1, 1, vec![])).unwrap();
    let snapshot = transaction.build().unwrap();
    let water = snapshot
        .descriptors()
        .fold(BTreeMap::new(), |mut water, descriptor| {
            let entry = water.entry(descriptor.kind()).or_insert(0);
            *entry = (*entry).max(descriptor.id().get());
            water
        });
    let records = CatalogLogicalRecords::new(
        snapshot.generation(),
        water,
        snapshot.descriptors().cloned().collect(),
    )
    .unwrap();
    let bytes = postcard::to_allocvec(&records).unwrap();
    let decoded: CatalogLogicalRecords = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, records);
    assert_eq!(
        decoded
            .reconstruct()
            .unwrap()
            .descriptors()
            .collect::<Vec<_>>(),
        snapshot.descriptors().collect::<Vec<_>>()
    );
    assert!(matches!(
        snapshot.logical_changes_from(&base).as_slice(),
        [CatalogLogicalChange::Created(_)]
    ));
    let mut wire = serde_json::to_value(&records).unwrap();
    wire["high_water"]["index"] = 0.into();
    assert!(serde_json::from_value::<CatalogLogicalRecords>(wire).is_err());
    let mut wire = serde_json::to_value(index(1, 1, vec![])).unwrap();
    wire["payload"]["index"]["target"]["properties"] = serde_json::json!(["x", "x"]);
    assert!(serde_json::from_value::<CatalogDescriptor>(wire).is_err());
}

#[test]
fn independent_constraint_and_profile_inputs_cannot_claim_activation() {
    let base_index = index(1, 1, vec![]);
    let CatalogPayload::Index(index) = base_index.payload() else {
        unreachable!()
    };
    let constraint = ConstraintDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Ready),
        target: index.target.clone(),
        declaring_type: "Person".into(),
        kind: ConstraintKind::Key,
        backing_index: None,
    };
    let error = CatalogDescriptor::constraint(
        ConstraintId::new(1).unwrap(),
        name("key"),
        CatalogParent::Graph(GraphId::new(1).unwrap()),
        gen_at(2),
        created(),
        constraint,
    )
    .unwrap_err();
    assert_eq!(
        error,
        CatalogError::InvalidDeclaration {
            reason: "unsupported_constraint_activation"
        }
    );
    let mut wire = serde_json::to_value(&base_index).unwrap();
    wire["payload"]["index"]["metadata"]["profile"]["hash"] = "forged".into();
    assert!(serde_json::from_value::<CatalogDescriptor>(wire).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn bounded_descriptor_bytes_never_bypass_validation(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(descriptor) = postcard::from_bytes::<CatalogDescriptor>(&bytes) {
            let roundtrip = postcard::to_allocvec(&descriptor).unwrap();
            prop_assert_eq!(postcard::from_bytes::<CatalogDescriptor>(&roundtrip).unwrap(), descriptor);
        }
    }
    #[test]
    fn analyzed_keys_are_preserved_without_catalog_case_rules(key in "[a-zA-Z][a-zA-Z0-9_]{0,31}") {
        let descriptor = index(1, 1, vec![]);
        let mut wire = serde_json::to_value(&descriptor).unwrap();
        wire["payload"]["index"]["target"]["properties"] = serde_json::json!([key]);
        let decoded: CatalogDescriptor = serde_json::from_value(wire).unwrap();
        let CatalogPayload::Index(index) = decoded.payload() else { unreachable!() };
        prop_assert_eq!(&index.target.properties[0], &key);
    }
}
