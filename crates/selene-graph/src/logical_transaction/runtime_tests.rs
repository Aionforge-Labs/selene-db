use super::*;

#[test]
fn reconstructed_scalar_edge_and_retained_ineligible_backing_have_real_contents() {
    let seed = seed();
    let mut tx = constrained(&seed);
    let records = tx.catalog.apply(&seed.catalog).unwrap();
    let mut descriptors = records.descriptors().to_vec();
    // Independent semantic fixture: add a weight-bearing edge and one edge index.
    tx.graphs[0].definition.as_mut().unwrap().edges[0]
        .1
        .properties
        .push(selene_core::PropertyDef {
            name: db_string("weight").unwrap(),
            value_type: selene_core::ValueType::predefined(selene_core::PredefinedValueType::Int),
            nullable: true,
            default: None,
            immutable: false,
            unique: false,
            record_fields: None,
        });
    if let Change::EdgeCreated { properties, .. } = tx.graphs[0].changes.last_mut().unwrap() {
        properties
            .set(db_string("weight").unwrap(), Value::Int(7))
            .unwrap();
    }
    descriptors.push(
        CatalogDescriptor::index(
            IndexId::new(3).unwrap(),
            CatalogName::regular("edge_weight").unwrap(),
            CatalogParent::Graph(selene_catalog::GraphId::new(1).unwrap()),
            generation(2),
            CreationMetadata::new(generation(2), None),
            IndexDeclaration {
                metadata: DeclarationMetadata::new(DeclarationState::Ready),
                target: PropertyTarget {
                    element: ElementKind::Edge,
                    label: "LINK".into(),
                    properties: vec!["weight".into()],
                },
                configuration: IndexConfiguration::Property(vec![SchemaPropertyIndexKind::I64]),
            },
        )
        .unwrap(),
    );
    // Graph two retains a non-ready backing; reconstruction must not activate it.
    let old = descriptors
        .iter_mut()
        .find(|d| d.id() == CatalogObjectId::Index(IndexId::new(2).unwrap()))
        .unwrap();
    let CatalogPayload::Index(mut index) = old.payload().clone() else {
        panic!("index");
    };
    index.metadata.state = DeclarationState::Inactive;
    *old = CatalogDescriptor::index(
        IndexId::new(2).unwrap(),
        old.name().clone(),
        old.parent(),
        old.generation(),
        old.creation().clone(),
        index,
    )
    .unwrap();
    let mut water = records.high_water().clone();
    water.insert(selene_catalog::CatalogObjectKind::Index, 3);
    let records = CatalogLogicalRecords::new(generation(2), water, descriptors).unwrap();
    tx.catalog = CatalogDelta::between(&seed.catalog.reconstruct().unwrap(), &records).unwrap();
    tx.graphs[0].backing_indexes.push(3);
    let state = apply(&seed, &tx).unwrap();
    let runtime = state.materialize(Limits::default()).unwrap();
    assert_eq!(runtime.rebuilt_indexes, 3);
    let first = runtime.graphs[&GraphId::new(1)].read();
    assert_eq!(
        first
            .nodes_with_property_eq(
                &db_string("L").unwrap(),
                &db_string("v").unwrap(),
                &Value::Int(2)
            )
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        first
            .edges_with_property_eq(
                &db_string("LINK").unwrap(),
                &db_string("weight").unwrap(),
                &Value::Int(7)
            )
            .unwrap()
            .len(),
        1
    );
    assert!(
        first
            .edges_with_property_eq(
                &db_string("LINK").unwrap(),
                &db_string("weight").unwrap(),
                &Value::Int(8)
            )
            .unwrap()
            .is_empty()
    );
    let second = runtime.graphs[&GraphId::new(2)].read();
    assert_eq!(second.property_index.len(), 1);
    assert_eq!(
        second.property_index[&(db_string("L").unwrap(), db_string("v").unwrap())]
            .lookup_eq(&Value::Int(1))
            .unwrap()
            .len(),
        1
    );
    assert!(
        second
            .nodes_with_property_eq(
                &db_string("L").unwrap(),
                &db_string("v").unwrap(),
                &Value::Int(1)
            )
            .is_none()
    );
    let images = [first.as_ref(), second.as_ref()];
    let bytes = encode_checkpoint(
        &state.catalog,
        &Default::default(),
        &images,
        Limits::default(),
    )
    .unwrap();
    let reopened = ReplayState::from_checkpoint(&bytes, Limits::default())
        .unwrap()
        .materialize(Limits::default())
        .unwrap();
    assert_eq!(
        reopened.graphs[&GraphId::new(1)]
            .read()
            .edges_with_property_eq(
                &db_string("LINK").unwrap(),
                &db_string("weight").unwrap(),
                &Value::Int(7)
            )
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        state.materialize(Limits {
            allocation: 1024,
            ..Limits::default()
        }),
        Err(E::Limit)
    ));
    let mut missing = state.clone();
    missing
        .backing_indexes
        .insert(GraphId::new(1), Arc::from([1]));
    assert!(missing.materialize(Limits::default()).is_err());
    let mut bad_optional = state.clone();
    bad_optional
        .backing_indexes
        .insert(GraphId::new(2), Arc::from([99]));
    assert!(bad_optional.materialize(Limits::default()).is_err());
}
