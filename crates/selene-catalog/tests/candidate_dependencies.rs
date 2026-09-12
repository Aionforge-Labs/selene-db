//! Active constraints cannot silently lose a declared candidate provider dependency.
use selene_catalog::*;

#[test]
fn active_constraint_requires_ready_provider_and_restricts_removal() {
    let gen_at = |n| CatalogGeneration::new(n).unwrap();
    let name = |s| CatalogName::regular(s).unwrap();
    let created = || CreationMetadata::new(gen_at(1), None);
    let catalog = CatalogId::new(1).unwrap();
    let root = DirectoryId::new(1).unwrap();
    let schema = SchemaId::new(1).unwrap();
    let graph = GraphId::new(1).unwrap();
    for state in [
        DeclarationState::Ready,
        DeclarationState::Inactive,
        DeclarationState::Failed,
    ] {
        let mut builder = CatalogSnapshotBuilder::new(
            gen_at(1),
            CatalogDescriptor::catalog(catalog, name("selene"), gen_at(1), created()).unwrap(),
            CatalogDescriptor::root_directory(root, catalog, gen_at(1), created()).unwrap(),
        )
        .unwrap();
        builder
            .insert(
                CatalogDescriptor::schema(schema, name("test"), root, gen_at(1), created())
                    .unwrap(),
            )
            .unwrap();
        builder
            .insert(
                CatalogDescriptor::graph(graph, name("g"), schema, gen_at(1), created(), None)
                    .unwrap(),
            )
            .unwrap();
        let provider = ProcedureId::new(1).unwrap();
        builder
            .insert(
                CatalogDescriptor::procedure(
                    provider,
                    name("current"),
                    CatalogParent::Graph(graph),
                    gen_at(1),
                    created(),
                    NativeDeclaration {
                        metadata: DeclarationMetadata::new(state),
                        binding: NativeBinding::CandidateState(NativeCandidateState {
                            required_label: Some("Doc".into()),
                            require_outgoing: vec![],
                            require_incoming: vec![],
                            exclude_outgoing: vec![],
                            exclude_incoming: vec![],
                        }),
                    },
                )
                .unwrap(),
            )
            .unwrap();
        let mut metadata = DeclarationMetadata::new(DeclarationState::Ready);
        metadata.dependencies.push(DeclarationDependency {
            id: CatalogObjectId::Procedure(provider),
            generation: gen_at(1),
        });
        builder
            .insert(
                CatalogDescriptor::constraint(
                    ConstraintId::new(1).unwrap(),
                    name("unique"),
                    CatalogParent::Graph(graph),
                    gen_at(1),
                    created(),
                    ConstraintDeclaration {
                        metadata,
                        target: PropertyTarget {
                            element: ElementKind::Node,
                            label: "Doc".into(),
                            properties: vec!["key".into()],
                        },
                        declaring_type: "Doc".into(),
                        kind: ConstraintKind::Unique,
                        backing_index: None,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        let result = builder.build();
        if state == DeclarationState::Ready {
            let snapshot = result.unwrap();
            let mut tx = CatalogTransaction::new(&snapshot).unwrap();
            tx.remove(CatalogObjectId::Procedure(provider));
            assert!(matches!(
                tx.build(),
                Err(CatalogError::InvalidDependency { .. })
            ));
        } else {
            assert!(
                matches!(
                    result,
                    Err(CatalogError::InvalidDependency {
                        reason: "dependency_not_ready",
                        ..
                    })
                ),
                "inactive/failed dependency must reject active constraints"
            );
        }
    }
}
