use super::*;
use crate::{GraphTypeDefinition, NodeTypeDefinition, PropertyDefinition, SchemaPath, Type};

#[test]
fn failed_constraint_publication_and_failed_write_preserve_backing() {
    let db = crate::Database::builder().build();
    let name = |s| PathSegment::regular(s).unwrap();
    let path = ObjectPath::regular("selene", "failure", "data").unwrap();
    let ty = ObjectPath::regular("selene", "failure", "shape").unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "failure").unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    let shape = GraphTypeDefinition::builder()
        .with_node_type(
            NodeTypeDefinition::new(name("Item"), vec![name("Item")])
                .unwrap()
                .with_property(PropertyDefinition::new(name("k"), Type::INT64).unwrap()),
        )
        .build()
        .unwrap();
    db.catalog()
        .create_graph_type(&ty, shape, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, Some(&ty), CreatePolicy::Strict)
        .unwrap();
    let rule = ConstraintDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Inactive),
        target: PropertyTarget {
            element: ElementKind::Node,
            label: "Item".into(),
            properties: vec!["k".into()],
        },
        declaring_type: "Item".into(),
        kind: ConstraintKind::Key,
        backing_index: None,
    };
    let before = db.catalog().snapshot();
    *db.inner.failure.lock() = Some(crate::catalog::FailurePoint::BeforePublication);
    assert!(
        db.catalog()
            .create_constraint(&path, &name("key"), rule.clone())
            .is_err()
    );
    assert!(db.catalog().snapshot().shares_state_with(&before));
    let session = db.session(&path).unwrap();
    session
        .execute("INSERT (:Item {k: 1}), (:Item {k: 1})")
        .unwrap();
    session.execute("MATCH (n:Item) DELETE n").unwrap();
    db.catalog()
        .create_constraint(&path, &name("key"), rule)
        .unwrap();
    session.execute("INSERT (:Item {k: 1})").unwrap();
    let before = db.catalog().snapshot();
    *db.inner.failure.lock() = Some(crate::catalog::FailurePoint::BeforePublication);
    assert!(session.execute("MATCH (n:Item) SET n.k = 2").is_err());
    assert!(db.catalog().snapshot().shares_state_with(&before));
    assert!(session.execute("INSERT (:Item {k: 1})").is_err());
    session.execute("INSERT (:Item {k: 2})").unwrap();
    // The lower execution host receives the same admitted catalog/index state,
    // not a facade-only policy hook. Its ordinary native write funnel enforces it.
    let state = db.inner.state.load_full();
    let graph = state.graphs.values().next().unwrap().graph.read();
    let native = selene_graph::SharedGraph::try_from_graph(graph.as_ref().clone()).unwrap();
    let mut host = selene_gql::Session::new(&native);
    assert!(
        host.execute_source("INSERT (:Item {k: 1})", &db.inner.procedures)
            .is_err()
    );
    host.execute_source("INSERT (:Item {k: 3})", &db.inner.procedures)
        .unwrap();
    assert_eq!(native.read().node_count(), 3);
    assert_eq!(graph.node_count(), 2);
}
