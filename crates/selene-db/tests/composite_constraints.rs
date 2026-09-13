//! F05-PR05 public activation, final-state mutation and reopen contract.

use selene_db::{
    ConstraintDeclaration, ConstraintKind, CreatePolicy, Database, DeclarationDefinition,
    DeclarationMetadata, DeclarationState, EdgeTypeDefinition, ElementKind, GraphTypeDefinition,
    NodeTypeDefinition, ObjectPath, PathSegment, PropertyDefinition, PropertyTarget, SchemaPath,
    Session, Type,
};

fn name(text: &str) -> PathSegment {
    PathSegment::regular(text).unwrap()
}

fn fixture(db: &Database) -> (ObjectPath, Session) {
    let node = NodeTypeDefinition::new(name("Item"), vec![name("Item")])
        .unwrap()
        .with_property(PropertyDefinition::new(name("a"), Type::STRING).unwrap())
        .with_property(PropertyDefinition::new(name("b"), Type::INT64).unwrap());
    let edge = EdgeTypeDefinition::new(name("Link"), name("LINK"), name("Item"), name("Item"))
        .with_property(PropertyDefinition::new(name("a"), Type::STRING).unwrap())
        .with_property(PropertyDefinition::new(name("b"), Type::INT64).unwrap());
    let shape = GraphTypeDefinition::builder()
        .with_node_type(node)
        .with_edge_type(edge)
        .build()
        .unwrap();
    let ty = ObjectPath::regular("selene", "constraints", "shape").unwrap();
    let path = ObjectPath::regular("selene", "constraints", "data").unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "constraints").unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    db.catalog()
        .create_graph_type(&ty, shape, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, Some(&ty), CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    (path, session)
}

fn rule(element: ElementKind, kind: ConstraintKind, properties: &[&str]) -> ConstraintDeclaration {
    ConstraintDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Inactive),
        target: PropertyTarget {
            element,
            label: if element == ElementKind::Node {
                "Item"
            } else {
                "LINK"
            }
            .into(),
            properties: properties.iter().map(|s| s.to_string()).collect(),
        },
        declaring_type: if element == ElementKind::Node {
            "Item"
        } else {
            "Link"
        }
        .into(),
        kind,
        backing_index: None,
    }
}

#[test]
fn composite_activation_fails_atomically_then_publishes_complete_backing() {
    let db = Database::builder().build();
    let (path, session) = fixture(&db);
    session
        .execute("INSERT (:Item {a: 'x', b: 1}), (:Item {a: 'x', b: 1})")
        .unwrap();
    let before = db.catalog().snapshot();
    let declaration = rule(
        ElementKind::Node,
        ConstraintKind::CompositeUnique,
        &["a", "b"],
    );
    assert!(
        db.catalog()
            .create_constraint(&path, &name("tuple"), declaration.clone())
            .is_err()
    );
    assert!(db.catalog().snapshot().shares_state_with(&before));
    session.execute("MATCH (n:Item) DELETE n").unwrap();
    let result = db
        .catalog()
        .create_constraint(&path, &name("tuple"), declaration)
        .unwrap();
    let DeclarationDefinition::Constraint(active) = result.definition else {
        panic!("constraint");
    };
    assert_eq!(active.metadata.state, DeclarationState::Ready);
    assert!(active.backing_index.is_some());
    session
        .execute("INSERT (:Item {a: 'x', b: 1}), (:Item {a: 'x', b: 2})")
        .unwrap();
    let before = db.catalog().snapshot();
    let error = session
        .execute("INSERT (:Item {a: 'z', b: 3}), (:Item {a: 'z', b: 3})")
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "G2000");
    assert!(db.catalog().snapshot().shares_state_with(&before));
    session.execute("INSERT (:Item {a: 'z', b: 3})").unwrap();
}

#[test]
fn composite_swaps_reuse_rollback_and_reopen_preserve_final_state() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path()).unwrap();
    let (path, session) = fixture(&db);
    db.catalog()
        .create_constraint(
            &path,
            &name("tuple"),
            rule(ElementKind::Node, ConstraintKind::Key, &["a", "b"]),
        )
        .unwrap();
    session
        .execute("INSERT (:Item {a: 'x', b: 1}), (:Item {a: 'x', b: 2})")
        .unwrap();
    // Both old values exist when the first batch row is visited.
    session.execute("MATCH (n:Item) SET n.b = 3 - n.b").unwrap();
    session.execute("START TRANSACTION").unwrap();
    session
        .execute("MATCH (n:Item) WHERE n.b = 1 DELETE n")
        .unwrap();
    session.execute("INSERT (:Item {a: 'x', b: 1})").unwrap();
    session.execute("ROLLBACK").unwrap();
    assert!(session.execute("INSERT (:Item {a: 'x', b: 1})").is_err());
    assert!(session.execute("INSERT (:Item {a: 'x'})").is_err());
    session
        .execute("MATCH (n:Item) WHERE n.b = 1 DELETE n")
        .unwrap();
    session.execute("INSERT (:Item {a: 'x', b: 1})").unwrap();
    drop(session);
    drop(db);
    let db = Database::open(directory.path()).unwrap();
    let session = db.session(&path).unwrap();
    assert!(session.execute("INSERT (:Item {a: 'x', b: 1})").is_err());
    assert_eq!(
        session
            .execute("MATCH (n:Item) RETURN n")
            .unwrap()
            .row_count(),
        Some(2)
    );
    db.checkpoint().unwrap();
    drop(session);
    drop(db);
    let db = Database::open(directory.path()).unwrap();
    assert!(
        db.session(&path)
            .unwrap()
            .execute("INSERT (:Item {a: 'x', b: 2})")
            .is_err()
    );
}

#[test]
fn unique_missing_null_node_edge_and_graph_domains_are_separate() {
    let db = Database::builder().build();
    let (path, session) = fixture(&db);
    for element in [ElementKind::Node, ElementKind::Edge] {
        db.catalog()
            .create_constraint(
                &path,
                &name(if element == ElementKind::Node {
                    "node_tuple"
                } else {
                    "edge_tuple"
                }),
                rule(element, ConstraintKind::CompositeUnique, &["a", "b"]),
            )
            .unwrap();
    }
    session
        .execute("INSERT (:Item {a: 'x'}), (:Item {a: 'x', b: NULL}), (:Item {a: 'x'})")
        .unwrap();
    session
        .execute("INSERT (:Item {a: 'x', b: 1})-[:LINK {a: 'x', b: 1}]->(:Item {a: 'x', b: 2})")
        .unwrap();
    assert!(
        session
            .execute("MATCH (n:Item) WHERE n.b = 1 INSERT (n)-[:LINK {a: 'x', b: 1}]->(n)")
            .is_err()
    );
    let other = ObjectPath::regular("selene", "constraints", "other").unwrap();
    let ty = ObjectPath::regular("selene", "constraints", "shape").unwrap();
    db.catalog()
        .create_graph(&other, Some(&ty), CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_constraint(
            &other,
            &name("node_tuple"),
            rule(ElementKind::Node, ConstraintKind::Key, &["a", "b"]),
        )
        .unwrap();
    db.session(&other)
        .unwrap()
        .execute("INSERT (:Item {a: 'x', b: 1})")
        .unwrap();
}

#[test]
fn late_batch_duplicate_cannot_publish_partial_constraint_backing() {
    let db = Database::builder().build();
    let (path, session) = fixture(&db);
    // Use incomplete UNIQUE tuples as seeds so the same declared type supplies
    // more than two mutation batches without adding a second test-only type.
    session.execute("INSERT (:Item {b: 0})").unwrap();
    for shift in 0..11 {
        session
            .execute(&format!(
                "MATCH (n:Item) INSERT (:Item {{b: n.b + {}}})",
                1 << shift
            ))
            .unwrap();
    }
    session.execute("INSERT (:Item {b: 0})").unwrap();
    db.catalog()
        .create_constraint(
            &path,
            &name("tuple"),
            rule(
                ElementKind::Node,
                ConstraintKind::CompositeUnique,
                &["a", "b"],
            ),
        )
        .unwrap();
    let before = db.catalog().snapshot();
    assert!(
        session
            .execute("MATCH (n:Item) SET n.a = 'complete'")
            .is_err()
    );
    assert!(db.catalog().snapshot().shares_state_with(&before));
    session
        .execute("INSERT (:Item {a: 'complete', b: 0})")
        .unwrap();
}

#[test]
fn graph_and_graph_type_replacement_do_not_inherit_retired_constraints() {
    let db = Database::builder().build();
    let (path, old_session) = fixture(&db);
    db.catalog()
        .create_constraint(
            &path,
            &name("key"),
            rule(ElementKind::Node, ConstraintKind::Key, &["a", "b"]),
        )
        .unwrap();
    let old = db.catalog().snapshot();
    let old_id = old.resolve_graph(&path).unwrap().id;
    // Replacing the empty graph retires its complete constraint/index ownership.
    db.catalog()
        .create_graph(&path, None, CreatePolicy::OrReplace)
        .unwrap();
    let ty = ObjectPath::regular("selene", "constraints", "shape").unwrap();
    let node = NodeTypeDefinition::new(name("Item"), vec![name("Item")])
        .unwrap()
        .with_property(PropertyDefinition::new(name("a"), Type::STRING).unwrap())
        .with_property(PropertyDefinition::new(name("b"), Type::INT64).unwrap());
    db.catalog()
        .create_graph_type(
            &ty,
            GraphTypeDefinition::builder()
                .with_node_type(node)
                .build()
                .unwrap(),
            CreatePolicy::OrReplace,
        )
        .unwrap();
    db.catalog()
        .create_graph(&path, Some(&ty), CreatePolicy::OrReplace)
        .unwrap();
    assert_ne!(
        db.catalog().snapshot().resolve_graph(&path).unwrap().id,
        old_id
    );
    assert!(
        db.catalog()
            .snapshot()
            .declarations(&path)
            .unwrap()
            .is_empty()
    );
    assert!(!old.declarations(&path).unwrap().is_empty());
    assert!(
        old_session
            .execute("INSERT (:Item {a: 'x', b: 1})")
            .is_err()
    );
    db.session(&path)
        .unwrap()
        .execute("INSERT (:Item {a: 'x', b: 1}), (:Item {a: 'x', b: 1})")
        .unwrap();
}
