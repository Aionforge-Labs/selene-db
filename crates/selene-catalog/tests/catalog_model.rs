//! Catalog identity, naming, descriptor, and immutable-snapshot contracts.

use proptest::prelude::*;
use selene_catalog::{
    BindingTableId, CatalogDescriptor, CatalogError, CatalogGeneration, CatalogId, CatalogName,
    CatalogObjectId, CatalogObjectKind, CatalogParent, CatalogPayload, CatalogSnapshotBuilder,
    CatalogTransaction, ConstraintId, CreationMetadata, DirectoryId, GraphId, GraphTypeId, IndexId,
    ProcedureId, SchemaId,
};

fn generation(raw: u64) -> CatalogGeneration {
    CatalogGeneration::new(raw).expect("test generation is nonzero")
}

fn creation(raw: u64) -> CreationMetadata {
    CreationMetadata::new(generation(raw), Some("fixture-principal".to_owned()))
}

fn name(value: &str) -> CatalogName {
    CatalogName::regular(value).expect("fixture name is a regular identifier")
}

fn builder(snapshot_generation: u64) -> CatalogSnapshotBuilder {
    let catalog = CatalogDescriptor::catalog(
        CatalogId::new(1).unwrap(),
        name("selene"),
        generation(1),
        creation(1),
    )
    .unwrap();
    let root = CatalogDescriptor::root_directory(
        DirectoryId::new(1).unwrap(),
        CatalogId::new(1).unwrap(),
        generation(1),
        creation(1),
    )
    .unwrap();
    CatalogSnapshotBuilder::new(generation(snapshot_generation), catalog, root).unwrap()
}

fn schema(id: u64, value: &str, descriptor_generation: u64) -> CatalogDescriptor {
    CatalogDescriptor::schema(
        SchemaId::new(id).unwrap(),
        name(value),
        DirectoryId::new(1).unwrap(),
        generation(descriptor_generation),
        creation(1),
    )
    .unwrap()
}

fn graph(id: u64, schema_id: u64, value: &str, descriptor_generation: u64) -> CatalogDescriptor {
    CatalogDescriptor::graph(
        GraphId::new(id).unwrap(),
        name(value),
        SchemaId::new(schema_id).unwrap(),
        generation(descriptor_generation),
        creation(descriptor_generation),
        None,
    )
    .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn arbitrary_nonzero_catalog_ids_round_trip_and_remain_kind_separated(
        raw in 1_u64..=u64::MAX,
    ) {
        macro_rules! round_trip_id {
            ($type:ident, $variant:ident) => {{
                let id = $type::new(raw).unwrap();
                prop_assert_eq!(id.get(), raw);
                let encoded = serde_json::to_string(&id).unwrap();
                prop_assert_eq!(serde_json::from_str::<$type>(&encoded).unwrap(), id);
                CatalogObjectId::$variant(id)
            }};
        }

        let ids = [
            round_trip_id!(CatalogId, Catalog),
            round_trip_id!(DirectoryId, Directory),
            round_trip_id!(SchemaId, Schema),
            round_trip_id!(GraphId, Graph),
            round_trip_id!(GraphTypeId, GraphType),
            round_trip_id!(BindingTableId, BindingTable),
            round_trip_id!(ProcedureId, Procedure),
            round_trip_id!(IndexId, Index),
            round_trip_id!(ConstraintId, Constraint),
        ];
        for (index, id) in ids.iter().enumerate() {
            let encoded = serde_json::to_string(id).unwrap();
            prop_assert_eq!(
                serde_json::from_str::<CatalogObjectId>(&encoded).unwrap(),
                *id,
            );
            for other in &ids[index + 1..] {
                prop_assert_ne!(id, other);
            }
        }
    }

    #[test]
    fn generated_regular_names_preserve_nfc_dictionary_identity(
        stem in "[A-Za-z_][A-Za-z0-9_]{0,31}",
    ) {
        let regular = CatalogName::regular(stem.clone()).unwrap();
        prop_assert_eq!(regular.display(), stem.as_str());
        prop_assert_eq!(regular.canonical(), stem.as_str());

        let mut lookup_builder = builder(1);
        lookup_builder.insert(schema(1, "public", 1)).unwrap();
        lookup_builder.insert(graph(1, 1, &stem, 1)).unwrap();
        let snapshot = lookup_builder.build().unwrap();
        let query = CatalogName::regular(stem.clone()).unwrap();
        prop_assert_eq!(
            snapshot.schema_object(SchemaId::new(1).unwrap(), &query).unwrap().id(),
            CatalogObjectId::Graph(GraphId::new(1).unwrap()),
        );

        let composed_spelling = format!("{stem}é");
        let decomposed_spelling = format!("{stem}e\u{301}");
        let composed = CatalogName::regular(composed_spelling).unwrap();
        let decomposed = CatalogName::regular(decomposed_spelling.clone()).unwrap();
        prop_assert_eq!(&composed, &decomposed);
        prop_assert_eq!(composed.canonical(), decomposed.canonical());
        prop_assert_ne!(composed.display(), decomposed.display());
        let encoded = serde_json::to_string(&decomposed).unwrap();
        let decoded = serde_json::from_str::<CatalogName>(&encoded).unwrap();
        prop_assert_eq!(decoded.display(), decomposed_spelling);
        prop_assert_eq!(decoded.canonical(), composed.canonical());

        let mut duplicate_builder = builder(1);
        duplicate_builder.insert(schema(1, "public", 1)).unwrap();
        duplicate_builder
            .insert(
                CatalogDescriptor::graph(
                    GraphId::new(1).unwrap(),
                    composed,
                    SchemaId::new(1).unwrap(),
                    generation(1),
                    creation(1),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        duplicate_builder
            .insert(
                CatalogDescriptor::binding_table(
                    BindingTableId::new(1).unwrap(),
                    decomposed,
                    SchemaId::new(1).unwrap(),
                    generation(1),
                    creation(1),
                )
                .unwrap(),
            )
            .unwrap();
        prop_assert!(matches!(
            duplicate_builder.build(),
            Err(CatalogError::DuplicateCanonicalName { .. })
        ), "NFC-equivalent names did not conflict");
    }
}

#[test]
fn catalog_ids_reject_zero_round_trip_and_remain_kind_safe() {
    macro_rules! check_id {
        ($type:ident, $kind:ident) => {{
            assert!(matches!(
                $type::new(0),
                Err(CatalogError::ZeroIdentifier {
                    kind: CatalogObjectKind::$kind
                })
            ));
            let id = $type::new(17).unwrap();
            assert_eq!(id.get(), 17);
            let encoded = serde_json::to_string(&id).unwrap();
            assert_eq!(serde_json::from_str::<$type>(&encoded).unwrap(), id);
            assert!(serde_json::from_str::<$type>("0").is_err());
        }};
    }

    check_id!(CatalogId, Catalog);
    check_id!(DirectoryId, Directory);
    check_id!(SchemaId, Schema);
    check_id!(GraphId, Graph);
    check_id!(GraphTypeId, GraphType);
    check_id!(BindingTableId, BindingTable);
    check_id!(ProcedureId, Procedure);
    check_id!(IndexId, Index);
    check_id!(ConstraintId, Constraint);

    let graph = CatalogObjectId::Graph(GraphId::new(17).unwrap());
    let graph_type = CatalogObjectId::GraphType(GraphTypeId::new(17).unwrap());
    assert_ne!(graph, graph_type);
    assert_eq!(graph.kind(), CatalogObjectKind::Graph);
    assert_eq!(graph_type.kind(), CatalogObjectKind::GraphType);
}

#[test]
fn generation_rejects_zero_and_checks_increment_overflow() {
    assert!(matches!(
        CatalogGeneration::new(0),
        Err(CatalogError::ZeroGeneration)
    ));
    assert_eq!(generation(8).next().unwrap().get(), 9);
    assert!(matches!(
        generation(u64::MAX).next(),
        Err(CatalogError::GenerationOverflow { current }) if current == u64::MAX
    ));

    let encoded = serde_json::to_string(&generation(9)).unwrap();
    assert_eq!(
        serde_json::from_str::<CatalogGeneration>(&encoded).unwrap(),
        generation(9)
    );
    assert!(serde_json::from_str::<CatalogGeneration>("0").is_err());
}

#[test]
fn unicode_versions_and_regular_identifier_profile_are_exact() {
    assert_eq!(selene_catalog::CATALOG_UNICODE_VERSION, (17, 0, 0));
    assert_eq!(unicode_ident::UNICODE_VERSION, (17, 0, 0));
    assert_eq!(unicode_normalization::UNICODE_VERSION, (17, 0, 0));

    for accepted in ["_", "_name2", "Person", "Δelta", "東京"] {
        assert!(CatalogName::regular(accepted).is_ok(), "{accepted}");
    }
    for rejected in ["2name", "has-dash", "has space"] {
        assert!(CatalogName::regular(rejected).is_err(), "{rejected}");
    }
    assert_eq!(
        CatalogName::delimited("display name").unwrap().display(),
        "display name"
    );
    assert!(matches!(
        CatalogName::regular(""),
        Err(CatalogError::EmptyIdentifier)
    ));
    assert!(matches!(
        CatalogName::delimited(""),
        Err(CatalogError::EmptyIdentifier)
    ));
    for constructor in [CatalogName::regular, CatalogName::delimited] {
        assert!(matches!(
            constructor("private\u{e000}name"),
            Err(CatalogError::PrivateUseCharacter {
                character: '\u{e000}'
            })
        ));
    }
}

#[test]
fn canonical_names_use_nfc_without_case_or_compatibility_folding() {
    let composed = CatalogName::regular("Café").unwrap();
    let decomposed = CatalogName::regular("Cafe\u{301}").unwrap();
    assert_eq!(composed, decomposed);
    assert_eq!(decomposed.canonical(), "Café");
    assert_eq!(decomposed.display(), "Cafe\u{301}");

    assert_ne!(name("Person"), name("person"));
    assert_ne!(
        CatalogName::delimited("①").unwrap(),
        CatalogName::delimited("1").unwrap()
    );

    let mut ordered = [name("éclair"), name("alpha"), name("Zulu"), name("zulu")];
    ordered.sort();
    assert_eq!(
        ordered.map(|value| value.canonical().to_owned()),
        ["Zulu", "alpha", "zulu", "éclair"]
    );
}

#[test]
fn descriptor_validation_checks_kind_payload_parent_and_user_names() {
    let graph_id = CatalogObjectId::Graph(GraphId::new(7).unwrap());
    let graph_payload = CatalogPayload::Graph { graph_type: None };
    assert!(matches!(
        CatalogDescriptor::new(
            graph_id,
            CatalogObjectKind::Procedure,
            name("g"),
            CatalogParent::Schema(SchemaId::new(1).unwrap()),
            generation(1),
            creation(1),
            graph_payload.clone(),
        ),
        Err(CatalogError::DescriptorKindMismatch { .. })
    ));
    assert!(matches!(
        CatalogDescriptor::new(
            graph_id,
            CatalogObjectKind::Graph,
            name("g"),
            CatalogParent::Schema(SchemaId::new(1).unwrap()),
            generation(1),
            creation(1),
            CatalogPayload::BindingTable,
        ),
        Err(CatalogError::DescriptorKindMismatch { .. })
    ));
    assert!(matches!(
        CatalogDescriptor::new(
            graph_id,
            CatalogObjectKind::Graph,
            name("g"),
            CatalogParent::Directory(DirectoryId::new(1).unwrap()),
            generation(1),
            creation(1),
            graph_payload,
        ),
        Err(CatalogError::InvalidParentKind { .. })
    ));

    let root = CatalogDescriptor::root_directory(
        DirectoryId::new(1).unwrap(),
        CatalogId::new(1).unwrap(),
        generation(1),
        creation(1),
    )
    .unwrap();
    assert_eq!(root.name().canonical(), "");
    assert!(root.name().is_synthetic_root());
    assert!(
        CatalogDescriptor::schema(
            SchemaId::new(1).unwrap(),
            root.name().clone(),
            DirectoryId::new(1).unwrap(),
            generation(1),
            creation(1),
        )
        .is_err()
    );
}

#[test]
fn descriptors_round_trip_without_collapsing_identity_generation_or_display() {
    let descriptor = CatalogDescriptor::graph_type(
        GraphTypeId::new(5).unwrap(),
        CatalogName::regular("Cafe\u{301}").unwrap(),
        SchemaId::new(3).unwrap(),
        generation(4),
        CreationMetadata::new(generation(2), Some("owner-a".to_owned())),
    )
    .unwrap();
    let json = serde_json::to_string(&descriptor).unwrap();
    assert!(!json.contains("SharedGraph"));
    assert!(!json.contains("RowIndex"));
    assert_eq!(
        serde_json::from_str::<CatalogDescriptor>(&json).unwrap(),
        descriptor
    );

    let canonical_display = CatalogDescriptor::graph_type(
        GraphTypeId::new(5).unwrap(),
        CatalogName::regular("Café").unwrap(),
        SchemaId::new(3).unwrap(),
        generation(4),
        CreationMetadata::new(generation(2), Some("owner-a".to_owned())),
    )
    .unwrap();
    assert_eq!(descriptor.name(), canonical_display.name());
    assert_ne!(descriptor, canonical_display);
    assert!(descriptor.same_identity(&canonical_display));

    let newer = CatalogDescriptor::graph_type(
        GraphTypeId::new(5).unwrap(),
        CatalogName::regular("Cafe\u{301}").unwrap(),
        SchemaId::new(3).unwrap(),
        generation(5),
        CreationMetadata::new(generation(2), Some("owner-a".to_owned())),
    )
    .unwrap();
    assert_ne!(descriptor, newer);
    assert!(descriptor.same_identity(&newer));
}

#[test]
fn snapshot_rejects_duplicate_ids_and_shared_namespace_conflicts() {
    let mut duplicate_id = builder(1);
    duplicate_id.insert(schema(1, "public", 1)).unwrap();
    let duplicate = duplicate_id.insert(schema(1, "other", 1)).unwrap_err();
    assert!(matches!(
        duplicate,
        CatalogError::DuplicateIdentifier { .. }
    ));
    let preserved = duplicate_id.build().unwrap();
    assert!(preserved.schema(&name("public")).is_some());
    assert!(preserved.schema(&name("other")).is_none());

    let mut same_kind = builder(1);
    same_kind.insert(schema(1, "public", 1)).unwrap();
    same_kind.insert(graph(1, 1, "item", 1)).unwrap();
    same_kind.insert(graph(2, 1, "item", 1)).unwrap();
    assert!(matches!(
        same_kind.build(),
        Err(CatalogError::DuplicateCanonicalName { .. })
    ));

    let mut cross_kind = builder(1);
    cross_kind.insert(schema(1, "public", 1)).unwrap();
    cross_kind.insert(graph(1, 1, "shared", 1)).unwrap();
    cross_kind
        .insert(
            CatalogDescriptor::binding_table(
                BindingTableId::new(1).unwrap(),
                name("shared"),
                SchemaId::new(1).unwrap(),
                generation(1),
                creation(1),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        cross_kind.build(),
        Err(CatalogError::DuplicateCanonicalName { .. })
    ));

    let mut root_namespace = builder(1);
    root_namespace.insert(schema(1, "Café", 1)).unwrap();
    root_namespace.insert(schema(2, "Cafe\u{301}", 1)).unwrap();
    assert!(matches!(
        root_namespace.build(),
        Err(CatalogError::DuplicateCanonicalName { .. })
    ));

    let mut separate_schemas = builder(1);
    separate_schemas.insert(schema(1, "one", 1)).unwrap();
    separate_schemas.insert(schema(2, "two", 1)).unwrap();
    separate_schemas.insert(graph(1, 1, "shared", 1)).unwrap();
    separate_schemas.insert(graph(2, 2, "shared", 1)).unwrap();
    assert!(separate_schemas.build().is_ok());
}

#[test]
fn snapshot_validates_parents_payload_references_and_flat_root() {
    let mut missing_parent = builder(1);
    missing_parent.insert(graph(1, 99, "g", 1)).unwrap();
    assert!(matches!(
        missing_parent.build(),
        Err(CatalogError::MissingParent { .. })
    ));

    let mut missing_graph_type = builder(1);
    missing_graph_type.insert(schema(1, "public", 1)).unwrap();
    missing_graph_type
        .insert(
            CatalogDescriptor::graph(
                GraphId::new(1).unwrap(),
                name("g"),
                SchemaId::new(1).unwrap(),
                generation(1),
                creation(1),
                Some(GraphTypeId::new(77).unwrap()),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        missing_graph_type.build(),
        Err(CatalogError::MissingPayloadReference { .. })
    ));

    let mut cross_schema = builder(1);
    cross_schema.insert(schema(1, "one", 1)).unwrap();
    cross_schema.insert(schema(2, "two", 1)).unwrap();
    cross_schema
        .insert(
            CatalogDescriptor::graph_type(
                GraphTypeId::new(1).unwrap(),
                name("type_one"),
                SchemaId::new(1).unwrap(),
                generation(1),
                creation(1),
            )
            .unwrap(),
        )
        .unwrap();
    cross_schema
        .insert(
            CatalogDescriptor::graph(
                GraphId::new(1).unwrap(),
                name("g"),
                SchemaId::new(2).unwrap(),
                generation(1),
                creation(1),
                Some(GraphTypeId::new(1).unwrap()),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        cross_schema.build(),
        Err(CatalogError::CrossSchemaPayloadReference { .. })
    ));

    let mut flat = builder(1);
    let error = flat
        .insert_child_directory(
            DirectoryId::new(2).unwrap(),
            DirectoryId::new(1).unwrap(),
            name("nested"),
            creation(1),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        CatalogError::UnsupportedDirectoryDepth { maximum_depth: 0 }
    ));
}

#[test]
fn snapshot_clone_reads_and_iteration_are_immutable_and_deterministic() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<selene_catalog::CatalogSnapshot>();

    let mut first_builder = builder(1);
    first_builder.insert(schema(2, "zeta", 1)).unwrap();
    first_builder.insert(schema(1, "alpha", 1)).unwrap();
    first_builder.insert(graph(2, 1, "zulu", 1)).unwrap();
    first_builder.insert(graph(1, 1, "alpha", 1)).unwrap();
    let first = first_builder.build().unwrap();
    let clone = first.clone();
    assert!(first.shares_state_with(&clone));
    assert_eq!(first.generation(), generation(1));

    let schema_names = first
        .schemas()
        .map(|descriptor| descriptor.name().canonical().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(schema_names, ["alpha", "zeta"]);
    let object_names = first
        .schema_objects(SchemaId::new(1).unwrap())
        .unwrap()
        .map(|descriptor| descriptor.name().canonical().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(object_names, ["alpha", "zulu"]);
    assert_eq!(
        first
            .schema_object(SchemaId::new(1).unwrap(), &name("alpha"))
            .unwrap()
            .id(),
        CatalogObjectId::Graph(GraphId::new(1).unwrap())
    );

    let mut second_builder = builder(2);
    second_builder.insert(schema(1, "alpha", 1)).unwrap();
    second_builder.insert(schema(2, "zeta", 1)).unwrap();
    second_builder.insert(graph(1, 1, "alpha", 1)).unwrap();
    second_builder.insert(graph(2, 1, "zulu", 1)).unwrap();
    second_builder.insert(graph(3, 1, "new_graph", 2)).unwrap();
    let second = second_builder.build().unwrap();

    assert!(!first.shares_state_with(&second));
    assert!(
        first
            .schema_object(SchemaId::new(1).unwrap(), &name("new_graph"))
            .is_none()
    );
    assert!(
        second
            .schema_object(SchemaId::new(1).unwrap(), &name("new_graph"))
            .is_some()
    );
}

#[test]
fn snapshot_checks_descriptor_generations_and_reports_reproducible_memory_accounting() {
    let mut future = builder(2);
    future.insert(schema(1, "future", 3)).unwrap();
    assert!(matches!(
        future.build(),
        Err(CatalogError::DescriptorGenerationAfterSnapshot { .. })
    ));

    let invalid_creation = CatalogDescriptor::graph(
        GraphId::new(1).unwrap(),
        name("invalid"),
        SchemaId::new(1).unwrap(),
        generation(1),
        creation(2),
        None,
    );
    assert!(matches!(
        invalid_creation,
        Err(CatalogError::CreationGenerationAfterDescriptor { .. })
    ));

    let mut valid = builder(1);
    valid.insert(schema(1, "public", 1)).unwrap();
    valid.insert(graph(1, 1, "default", 1)).unwrap();
    let snapshot = valid.build().unwrap();
    let memory = snapshot.memory_accounting();
    assert_eq!(memory.descriptor_count(), 4);
    assert_eq!(memory.dictionary_entry_count(), 2);
    assert!(memory.descriptor_bytes() > 0);
    assert!(memory.dictionary_bytes() > 0);
}

#[test]
fn transaction_stages_insert_and_drop_without_mutating_its_base() {
    let mut base_builder = builder(1);
    base_builder.insert(schema(1, "public", 1)).unwrap();
    base_builder.insert(graph(1, 1, "first", 1)).unwrap();
    let base = base_builder.build().unwrap();

    let mut transaction = CatalogTransaction::new(&base).unwrap();
    assert_eq!(transaction.generation(), generation(2));
    assert!(
        transaction
            .remove(CatalogObjectId::Graph(GraphId::new(1).unwrap()))
            .is_some()
    );
    transaction.insert(graph(2, 1, "second", 2)).unwrap();
    let next = transaction.build().unwrap();

    assert!(
        base.schema_object(SchemaId::new(1).unwrap(), &name("first"))
            .is_some()
    );
    assert!(
        base.schema_object(SchemaId::new(1).unwrap(), &name("second"))
            .is_none()
    );
    assert!(
        next.schema_object(SchemaId::new(1).unwrap(), &name("first"))
            .is_none()
    );
    assert!(
        next.schema_object(SchemaId::new(1).unwrap(), &name("second"))
            .is_some()
    );
    assert!(!base.shares_state_with(&next));
}

#[test]
fn transaction_rejects_duplicate_id_instead_of_rebinding_identity() {
    let mut base_builder = builder(1);
    base_builder.insert(schema(1, "public", 1)).unwrap();
    base_builder.insert(graph(1, 1, "first", 1)).unwrap();
    let base = base_builder.build().unwrap();

    let mut transaction = CatalogTransaction::new(&base).unwrap();
    let error = transaction
        .insert(graph(1, 1, "replacement", 2))
        .unwrap_err();
    assert!(matches!(
        error,
        CatalogError::DuplicateIdentifier {
            id: CatalogObjectId::Graph(id)
        } if id == GraphId::new(1).unwrap()
    ));
    let next = transaction.build().unwrap();
    assert!(
        next.schema_object(SchemaId::new(1).unwrap(), &name("first"))
            .is_some()
    );
    assert!(
        next.schema_object(SchemaId::new(1).unwrap(), &name("replacement"))
            .is_none()
    );
}
