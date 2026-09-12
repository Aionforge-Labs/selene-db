//! Durable declaration identity and callable code are resolved together.

use selene_catalog::*;
use selene_gql::{
    BuiltinProcedureRegistry, ProcedureRegistry,
    analyze::{analyze_catalog, catalog::CatalogEnvironment},
    parse,
};

fn snapshot(registry: &BuiltinProcedureRegistry) -> CatalogSnapshot {
    let generation = CatalogGeneration::new(1).unwrap();
    let creation = CreationMetadata::new(generation, None);
    let catalog_id = CatalogId::new(1).unwrap();
    let directory = DirectoryId::new(1).unwrap();
    let schema = SchemaId::new(1).unwrap();
    let mut builder = CatalogSnapshotBuilder::new(
        generation,
        CatalogDescriptor::catalog(
            catalog_id,
            CatalogName::regular("selene").unwrap(),
            generation,
            creation.clone(),
        )
        .unwrap(),
        CatalogDescriptor::root_directory(directory, catalog_id, generation, creation.clone())
            .unwrap(),
    )
    .unwrap();
    builder
        .insert(
            CatalogDescriptor::schema(
                schema,
                CatalogName::regular("s").unwrap(),
                directory,
                generation,
                creation.clone(),
            )
            .unwrap(),
        )
        .unwrap();
    builder
        .insert(
            CatalogDescriptor::graph(
                GraphId::new(1).unwrap(),
                CatalogName::regular("g").unwrap(),
                schema,
                generation,
                creation,
                None,
            )
            .unwrap(),
        )
        .unwrap();
    for declaration in registry.declarations() {
        builder.insert(declaration.clone()).unwrap();
    }
    builder.build().unwrap()
}

fn analyze(
    catalog: CatalogSnapshot,
    registry: &BuiltinProcedureRegistry,
) -> Result<selene_gql::AnalyzedStatement, selene_gql::AnalysisError> {
    analyze_catalog(
        parse("CALL algo.wcc('p') YIELD node_id").unwrap(),
        registry,
        CatalogEnvironment::new(
            catalog,
            SchemaId::new(1).unwrap(),
            GraphId::new(1).unwrap(),
            None,
        ),
    )
}

#[test]
fn named_resolution_carries_stable_identity_and_removal_invalidates_only_dependents() {
    let registry = BuiltinProcedureRegistry::new();
    let original = snapshot(&registry);
    let analyzed = analyze(original.clone(), &registry).unwrap();
    let name = [
        selene_core::db_string("algo").unwrap(),
        selene_core::db_string("wcc").unwrap(),
    ];
    let declaration = registry.lookup(&name).unwrap().declaration.unwrap();
    let resolution = analyzed.catalog.as_ref().unwrap();
    assert!(resolution.objects().contains(&declaration));
    assert!(resolution.is_current(&original));
    let mut changed = CatalogTransaction::new(&original).unwrap();
    changed.remove(declaration.id());
    let changed = changed.build().unwrap();
    assert!(!resolution.is_current(&changed));
    assert!(analyze(changed.clone(), &registry).is_err());
    assert!(registry.validate_catalog(&changed).is_err());
}

#[test]
fn unsupported_or_changed_durable_bindings_fail_reattachment_and_analysis() {
    let registry = BuiltinProcedureRegistry::new();
    let original = snapshot(&registry);
    let expected = registry
        .declarations()
        .find(|d| d.name().display() == "algo.wcc")
        .unwrap();
    for mode in 0..3 {
        let mut changed = CatalogTransaction::new(&original).unwrap();
        let CatalogPayload::Procedure(mut native) = expected.payload().clone() else {
            panic!("procedure")
        };
        let NativeBinding::Procedure(procedure) = &mut native.binding else {
            panic!("binding")
        };
        match mode {
            0 => procedure.binding = vec!["not_installed".into()],
            1 => procedure.effect = NativeEffect::SchemaWrite,
            _ => native.metadata.state = DeclarationState::Inactive,
        }
        changed.remove(expected.id());
        changed
            .insert(
                CatalogDescriptor::procedure(
                    ProcedureId::new(expected.id().get()).unwrap(),
                    expected.name().clone(),
                    expected.parent(),
                    changed.generation(),
                    CreationMetadata::new(changed.generation(), None),
                    native,
                )
                .unwrap(),
            )
            .unwrap();
        let changed = changed.build().unwrap();
        assert!(
            registry.validate_catalog(&changed).is_err(),
            "reopen admission must reject unsupported activation"
        );
        assert!(
            analyze(changed, &registry).is_err(),
            "name alone must not activate code"
        );
    }
}
