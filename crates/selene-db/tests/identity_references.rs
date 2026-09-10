//! Facade identity and runtime-reference ownership/liveness coverage.

use std::{collections::HashSet, hash::Hash};

use selene_db::{
    CreatePolicy, Database, EdgeId, EdgeRef, Error, ErrorKind, GraphRef, NodeId, NodeRef,
    ObjectPath, SchemaPath, Value,
};

fn graph_path(schema: &str, graph: &str) -> ObjectPath {
    ObjectPath::regular("selene", schema, graph).unwrap()
}

fn database_with_graphs(schema_name: &str, graph_names: &[&str]) -> Database {
    let database = Database::builder().build();
    let catalog = database.catalog();
    catalog
        .create_schema(
            &SchemaPath::regular("selene", schema_name).unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    for name in graph_names {
        catalog
            .create_graph(&graph_path(schema_name, name), None, CreatePolicy::Strict)
            .unwrap();
    }
    database
}

fn insert_link(session: &selene_db::Session) {
    session
        .execute("INSERT (:AnchorA)-[:LINK]->(:AnchorB) FINISH")
        .unwrap();
}

fn assert_invalid_reference(error: Error, detail: &str) {
    assert_eq!(error.kind(), ErrorKind::RuntimeInvalidReference);
    assert_eq!(error.gqlstatus().unwrap().as_str(), "42002");
    assert!(error.message().contains(detail), "{}", error.message());
}

fn assert_public_handle<T: Copy + Eq + Hash>(value: T) -> T {
    value
}

fn assert_deleted_reference(error: Error) {
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22G11");
}

#[test]
fn identities_are_instance_scoped_and_references_ignore_generation() {
    let database = database_with_graphs("identity_live", &["main"]);
    let cloned = database.clone();
    assert_eq!(database.id(), cloned.id());
    assert_ne!(
        database.id(),
        Database::builder().build().id(),
        "independent builds must never share a process-local identity"
    );

    let path = graph_path("identity_live", "main");
    let session = cloned.session(&path).unwrap();
    assert_eq!(session.database_id(), database.id());
    let initial_generation = session.graph_generation().unwrap();
    let graph_reference = assert_public_handle(database.graph_reference(&path).unwrap());
    assert_eq!(graph_reference, session.graph_reference().unwrap());

    insert_link(&session);
    let node = assert_public_handle(session.node_reference(NodeId::new(1)).unwrap());
    let edge = assert_public_handle(session.edge_reference(EdgeId::new(1)).unwrap());
    let copied_node = node;
    let copied_edge = edge;
    let mut references = HashSet::new();
    references.insert(node);
    references.insert(copied_node);
    assert_eq!(references.len(), 1);

    session.execute("INSERT (:ExtraNode) FINISH").unwrap();
    let later_generation = session.graph_generation().unwrap();
    assert!(later_generation > initial_generation);
    assert_eq!(session.graph_reference().unwrap(), graph_reference);
    assert_eq!(
        session.resolve_node_reference(copied_node).unwrap(),
        NodeId::new(1)
    );
    assert_eq!(
        session.resolve_edge_reference(copied_edge).unwrap(),
        EdgeId::new(1)
    );
}

#[test]
fn cross_database_and_wrong_selected_graph_references_return_42002() {
    let first = database_with_graphs("identity_scope", &["first", "second"]);
    let first_path = graph_path("identity_scope", "first");
    let second_path = graph_path("identity_scope", "second");
    let first_session = first.session(&first_path).unwrap();
    let second_session = first.session(&second_path).unwrap();
    first_session.execute("INSERT (:ScopeOne) FINISH").unwrap();
    second_session.execute("INSERT (:ScopeTwo) FINISH").unwrap();

    let graph = first_session.graph_reference().unwrap();
    let node = first_session.node_reference(NodeId::new(1)).unwrap();
    assert_invalid_reference(
        second_session.resolve_graph_reference(graph).unwrap_err(),
        "another graph",
    );
    assert_invalid_reference(
        second_session.resolve_node_reference(node).unwrap_err(),
        "another graph",
    );

    drop(first_session);
    drop(second_session);
    drop(first);

    let other = database_with_graphs("identity_scope", &["first"]);
    let other_session = other.session(&first_path).unwrap();
    other_session.execute("INSERT (:Other) FINISH").unwrap();
    assert_invalid_reference(
        other.resolve_graph_reference(graph).unwrap_err(),
        "another database instance",
    );
    assert_invalid_reference(
        other_session.resolve_node_reference(node).unwrap_err(),
        "another database instance",
    );
}

#[test]
fn deleted_dropped_and_recreated_references_never_retarget() {
    let database = database_with_graphs("identity_stale", &["main"]);
    let path = graph_path("identity_stale", "main");
    let session = database.session(&path).unwrap();
    insert_link(&session);
    let graph = session.graph_reference().unwrap();
    let node = session.node_reference(NodeId::new(1)).unwrap();
    let edge = session.edge_reference(EdgeId::new(1)).unwrap();

    session
        .execute("MATCH ()-[e:LINK]->() DELETE e FINISH")
        .unwrap();
    assert_deleted_reference(session.resolve_edge_reference(edge).unwrap_err());
    session.execute("MATCH (n) DELETE n FINISH").unwrap();
    assert_deleted_reference(session.resolve_node_reference(node).unwrap_err());
    assert_deleted_reference(session.node_reference(NodeId::new(999)).unwrap_err());

    database
        .catalog()
        .drop_graph(&path, selene_db::DropPolicy::Strict)
        .unwrap();
    assert_invalid_reference(
        database.resolve_graph_reference(graph).unwrap_err(),
        "no longer live",
    );
    assert_invalid_reference(
        session.resolve_node_reference(node).unwrap_err(),
        "no longer live",
    );

    database
        .catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let replacement = database.session(&path).unwrap();
    let replacement_graph = replacement.graph_reference().unwrap();
    assert_ne!(replacement_graph, graph);
    assert_invalid_reference(
        replacement.resolve_node_reference(node).unwrap_err(),
        "another graph",
    );
    assert_invalid_reference(
        database.resolve_graph_reference(graph).unwrap_err(),
        "no longer live",
    );
}

#[test]
fn value_references_carry_validated_facade_ownership() {
    let database = database_with_graphs("identity_bridge", &["main"]);
    let session = database
        .session(&graph_path("identity_bridge", "main"))
        .unwrap();
    session.execute("INSERT (:Bridge) FINISH").unwrap();

    let value = Value::NodeRef(session.node_reference(NodeId::new(1)).unwrap());
    let Value::NodeRef(facade) = value else {
        unreachable!()
    };
    assert_eq!(facade.node_id(), NodeId::new(1));
    assert_eq!(facade.database_id(), database.id());
    assert_deleted_reference(session.node_reference(NodeId::new(99)).unwrap_err());
}

#[test]
fn analyzer_undefined_reference_remains_42n03() {
    let database = database_with_graphs("identity_analyzer", &["main"]);
    let session = database
        .session(&graph_path("identity_analyzer", "main"))
        .unwrap();
    let error = session
        .execute("MATCH (a) CALL () { RETURN a AS n LIMIT 1 } YIELD n RETURN n")
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "42N03");
}

// Public signatures prove the handle families are distinct facade types rather
// than aliases for one raw integer domain.
const _: fn(GraphRef) -> GraphRef = assert_public_handle::<GraphRef>;
const _: fn(NodeRef) -> NodeRef = assert_public_handle::<NodeRef>;
const _: fn(EdgeRef) -> EdgeRef = assert_public_handle::<EdgeRef>;
