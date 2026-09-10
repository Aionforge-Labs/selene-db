//! Comparable UNIQUE domains for publicly constructible legacy descriptors.
use super::*;

fn duration(text: &str) -> Value {
    Value::Duration(Box::new(text.parse().unwrap()))
}

fn definition(kind: PropertyValueType) -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("unique.domain"),
        node_types: vec![crate::NodeTypeDef {
            name: db_string("Item"),
            key_labels: LabelSet::single(db_string("Item")),
            properties: vec![PropertyTypeDef {
                name: db_string("key"),
                value_type: kind,
                list_element_type: None,
                required: false,
                default: None,
                immutable: false,
                unique: true,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: vec![],
    }
}

fn insert(graph: &SharedGraph, value: Value) -> crate::GraphResult<()> {
    let mut tx = graph.begin_write();
    tx.mutator()
        .create_node(LabelSet::single(db_string("Item")), prop("key", value))?;
    tx.commit().map(|_| ())
}

fn assert_incomparable(error: GraphError) {
    assert_eq!(error.gqlstatus(), "22G04", "{error}");
}

#[test]
fn generic_duration_unique_requires_one_nonzero_unit_group_incrementally() {
    for (first, other) in [("P1M", "PT1H"), ("PT1H", "P1M")] {
        let graph = SharedGraph::builder(GraphId::new(1501))
            .bound_to(definition(PropertyValueType::Duration))
            .unwrap()
            .build()
            .unwrap();
        insert(&graph, duration("PT0S")).unwrap();
        insert(&graph, duration(first)).unwrap();
        let generation = graph.read().meta.generation;
        assert_incomparable(insert(&graph, duration(other)).unwrap_err());
        assert_eq!(graph.read().node_count(), 2);
        assert_eq!(graph.read().meta.generation, generation);
    }
}

#[test]
fn generic_duration_unique_rejects_a_mixed_value_and_preserves_equivalent_duplicates() {
    let graph = SharedGraph::builder(GraphId::new(1502))
        .bound_to(definition(PropertyValueType::Duration))
        .unwrap()
        .build()
        .unwrap();
    assert_incomparable(insert(&graph, duration("P1MT1H")).unwrap_err());
    insert(&graph, duration("PT1H")).unwrap();
    assert!(matches!(
        insert(&graph, duration("PT60M")),
        Err(GraphError::TypeViolation(
            TypeViolation::UniquePropertyDuplicate { .. }
        ))
    ));
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn complete_state_and_recursive_unique_domains_reject_incomparable_values() {
    for kind in [
        PropertyValueType::Duration,
        PropertyValueType::List,
        PropertyValueType::Record,
    ] {
        let wrap = |value| match kind {
            PropertyValueType::List => Value::List(vec![value]),
            PropertyValueType::Record => Value::Record(Box::new(selene_core::Record::Open(
                vec![(db_string("nested"), Value::List(vec![value]))].into(),
            ))),
            _ => value,
        };
        let graph = SharedGraph::new(GraphId::new(1503));
        insert(&graph, wrap(duration("P1M"))).unwrap();
        insert(&graph, wrap(duration("PT1H"))).unwrap();
        let error = validate_entity_state(&graph.read(), &definition(kind)).unwrap_err();
        assert_incomparable(error.into());
        let typed = SharedGraph::builder(GraphId::new(1504))
            .bound_to(definition(kind))
            .unwrap()
            .build()
            .unwrap();
        insert(&typed, wrap(duration("P1M"))).unwrap();
        assert_incomparable(insert(&typed, wrap(duration("PT1H"))).unwrap_err());
        assert_eq!(typed.read().node_count(), 1);
    }
}

#[test]
fn unique_domains_are_scoped_by_entity_kind_type_and_property() {
    let mut schema = definition(PropertyValueType::Duration);
    let mut secondary = schema.node_types[0].properties[0].clone();
    secondary.name = db_string("secondary");
    schema.node_types[0].properties.push(secondary);
    let mut other = schema.node_types[0].clone();
    other.name = db_string("Other");
    other.key_labels = LabelSet::single(db_string("Other"));
    schema.node_types.push(other);
    schema.edge_types.push(crate::EdgeTypeDef {
        name: db_string("Link"),
        label: db_string("Link"),
        source_node_type: EdgeEndpointDef::NodeType(0),
        target_node_type: EdgeEndpointDef::NodeType(0),
        properties: vec![schema.node_types[0].properties[0].clone()],
        validation_mode: ValidationMode::Strict,
    });
    for incremental in [false, true] {
        let graph = if incremental {
            SharedGraph::builder(GraphId::new(1505))
                .bound_to(schema.clone())
                .unwrap()
                .build()
                .unwrap()
        } else {
            SharedGraph::new(GraphId::new(1506))
        };
        let mut tx = graph.begin_write();
        let mut mutation = tx.mutator();
        let item = mutation
            .create_node(
                LabelSet::single(db_string("Item")),
                PropertyMap::from_pairs([
                    (db_string("key"), duration("P1M")),
                    (db_string("secondary"), duration("PT1H")),
                ])
                .unwrap(),
            )
            .unwrap();
        mutation
            .create_node(
                LabelSet::single(db_string("Other")),
                prop("key", duration("PT1H")),
            )
            .unwrap();
        mutation
            .create_edge(db_string("Link"), item, item, prop("key", duration("PT1H")))
            .unwrap();
        tx.commit().unwrap();
        validate_entity_state(&graph.read(), &schema).unwrap();
        let generation = graph.read().meta.generation;
        let mut tx = graph.begin_write();
        tx.mutator()
            .create_edge(db_string("Link"), item, item, prop("key", duration("P1M")))
            .unwrap();
        if incremental {
            assert_incomparable(tx.commit().unwrap_err());
            assert_eq!(graph.read().edge_count(), 1);
            assert_eq!(graph.read().meta.generation, generation);
        } else {
            tx.commit().unwrap();
            assert_incomparable(
                validate_entity_state(&graph.read(), &schema)
                    .unwrap_err()
                    .into(),
            );
        }
    }
}
