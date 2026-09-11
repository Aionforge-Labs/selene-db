//! Public Rust schema construction is independent of the GQL catalog subset.
use selene_db::{
    CreatePolicy, Database, EdgeTypeDefinition, GraphTypeDefinition, NodeTypeDefinition,
    ObjectPath, PathSegment, PropertyDefinition, SchemaPath, Type, Value,
};

fn name(text: &str) -> PathSegment {
    PathSegment::regular(text).unwrap()
}

#[test]
fn public_schema_defaults_uniqueness_immutability_and_mixed_endpoints() {
    let node = NodeTypeDefinition::new(name("Item"), vec![name("Item")])
        .unwrap()
        .with_property(
            PropertyDefinition::new(name("serial"), Type::INT64.with_nullability(false))
                .unwrap()
                .unique()
                .immutable(),
        )
        .with_property(
            PropertyDefinition::new(name("active"), Type::BOOLEAN)
                .unwrap()
                .with_default(Value::Bool(true))
                .unwrap(),
        );
    let edge = EdgeTypeDefinition::new(name("Link"), name("LINK"), name("Item"), name("Item"))
        .with_property(
            PropertyDefinition::new(name("weight"), Type::INT64)
                .unwrap()
                .with_default(Value::Int(7))
                .unwrap(),
        );
    let definition = GraphTypeDefinition::builder()
        .with_node_type(node)
        .with_edge_type(edge)
        .build()
        .unwrap();
    let db = Database::builder().build();
    let schema = SchemaPath::regular("selene", "test").unwrap();
    let ty = ObjectPath::regular("selene", "test", "Shape").unwrap();
    let path = ObjectPath::regular("selene", "test", "data").unwrap();
    db.catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph_type(&ty, definition, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, Some(&ty), CreatePolicy::Strict)
        .unwrap();
    let s = db.session(&path).unwrap();
    s.execute("INSERT (:Item {serial: 1})~[:LINK]~(:Item {serial: 2})")
        .unwrap();
    assert_eq!(
        s.execute("MATCH (n:Item) WHERE n.active = TRUE RETURN n")
            .unwrap()
            .row_count(),
        Some(2)
    );
    assert_eq!(
        s.execute("MATCH ()~[e:LINK]~() WHERE e.weight = 7 RETURN e")
            .unwrap()
            .row_count(),
        Some(2)
    );
    for q in [
        "INSERT (:Item)",
        "INSERT (:Item {serial: 1})",
        "MATCH (n) SET n.serial = 4",
    ] {
        assert_eq!(
            s.execute(q).unwrap_err().gqlstatus().unwrap().as_str(),
            "G2000",
            "{q}"
        );
    }
}

#[test]
fn recursive_native_defaults_are_validated_without_container_only_admission() {
    use selene_db::{ExecutionOutcome, Record, TypeKind};
    let db = Database::builder().build();
    let schema = SchemaPath::regular("selene", "values").unwrap();
    let path = ObjectPath::regular("selene", "values", "data").unwrap();
    db.catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let s = db.session(&path).unwrap();
    s.execute("INSERT (:Witness)").unwrap();
    let query = |source: &str| {
        let ExecutionOutcome::Rows { result, .. } = s.execute(&format!("RETURN {source}")).unwrap()
        else {
            panic!("rows");
        };
        result.rows()[0].values()[0].clone()
    };
    let Value::String(field) = query("'field'") else {
        panic!("string");
    };
    let ty = Type::record([(field.clone(), Type::list(Type::VECTOR, None).unwrap())]).unwrap();
    assert!(
        PropertyDefinition::new(name("record"), ty.clone())
            .unwrap()
            .with_default(query("RECORD{field: [CAST([1, 0] AS VECTOR)]}"))
            .is_ok()
    );
    assert!(
        PropertyDefinition::new(name("record"), ty)
            .unwrap()
            .with_default(query("RECORD{field: [[1, 0]]}"))
            .is_err()
    );
    assert!(
        PropertyDefinition::new(name("json"), Type::JSON)
            .unwrap()
            .with_default(query("CAST('{\"b\":2,\"a\":1}' AS JSON)"))
            .is_ok()
    );
    assert!(
        PropertyDefinition::new(name("json"), Type::JSON)
            .unwrap()
            .with_default(query("'not json'"))
            .is_err()
    );
    assert!(
        PropertyDefinition::new(name("v"), Type::VECTOR)
            .unwrap()
            .with_default(Value::List(vec![Value::Int(1)]))
            .is_err()
    );
    let reference = Value::NodeRef(s.node_reference(selene_db::NodeId::new(1)).unwrap());
    let record = Value::Record(Box::new(Record::Open(vec![(
        field.clone(),
        Value::List(vec![reference]),
    )])));
    assert!(
        PropertyDefinition::new(
            name("open_record"),
            Type::new(TypeKind::Record(None), true).unwrap()
        )
        .unwrap()
        .with_default(record)
        .is_err()
    );
    let duplicate = Value::Record(Box::new(Record::Open(vec![
        (field.clone(), Value::Int(1)),
        (field, Value::Int(2)),
    ])));
    assert!(
        PropertyDefinition::new(
            name("record"),
            Type::new(TypeKind::Record(None), true).unwrap()
        )
        .unwrap()
        .with_default(duplicate)
        .is_err()
    );
    let mut deep = Type::INT64;
    for _ in 0..64 {
        deep = Type::list(deep, None).unwrap();
    }
    assert!(PropertyDefinition::new(name("deep"), deep).is_err());
    let node = NodeTypeDefinition::new(name("Item"), vec![name("Item")])
        .unwrap()
        .with_property(
            PropertyDefinition::new(PathSegment::delimited("é").unwrap(), Type::INT64).unwrap(),
        )
        .with_property(
            PropertyDefinition::new(PathSegment::delimited("e\u{301}").unwrap(), Type::INT64)
                .unwrap(),
        );
    assert!(
        GraphTypeDefinition::builder()
            .with_node_type(node)
            .build()
            .is_err()
    );
}

#[test]
fn public_schema_rejects_invalid_defaults_types_duplicates_and_endpoints() {
    assert!(PropertyDefinition::new(name("ref"), Type::NODE).is_err());
    assert!(
        PropertyDefinition::new(name("a"), Type::INT64)
            .unwrap()
            .with_default(Value::Bool(true))
            .is_err()
    );
    assert!(
        PropertyDefinition::new(name("a"), Type::INT64.with_nullability(false))
            .unwrap()
            .with_default(Value::Null)
            .is_err()
    );
    assert!(PropertyDefinition::new(name("a"), Type::list(Type::INT64, Some(2)).unwrap()).is_err());
    let node = NodeTypeDefinition::new(name("Item"), vec![name("Item")]).unwrap();
    assert!(
        GraphTypeDefinition::builder()
            .with_node_type(node.clone())
            .with_node_type(node.clone())
            .build()
            .is_err()
    );
    assert!(
        GraphTypeDefinition::builder()
            .with_node_type(node.clone())
            .with_edge_type(EdgeTypeDefinition::new(
                name("Link"),
                name("LINK"),
                name("Absent"),
                name("Item")
            ))
            .build()
            .is_err()
    );
    let property = PropertyDefinition::new(name("value"), Type::INT64).unwrap();
    assert!(
        GraphTypeDefinition::builder()
            .with_node_type(node.with_property(property.clone()).with_property(property))
            .build()
            .is_err()
    );
    let list = PropertyDefinition::new(name("a"), Type::list(Type::INT64, None).unwrap()).unwrap();
    assert!(
        list.with_default(Value::List(vec![Value::Bool(true)]))
            .is_err()
    );
}
