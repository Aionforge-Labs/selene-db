//! Request preflight coverage for ownership-bearing query references.

use selene_core::{EdgeDirection, db_string};
use selene_db::{
    CreatePolicy, Database, EdgeId, GeneralParameter, NodeId, ObjectPath, Record, Request,
    RequestOutcome, RequestParams, SchemaPath, Type, TypeKind, Value, ValuePathSegment,
};

fn fixture(name: &str) -> selene_db::Session {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let schema = SchemaPath::regular("selene", name).unwrap();
    let graph = ObjectPath::regular("selene", name, "main").unwrap();
    catalog
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    catalog
        .create_graph(&graph, None, CreatePolicy::Strict)
        .unwrap();
    database.session(&graph).unwrap()
}

fn execute_parameter(session: &selene_db::Session, ty: Type, value: Value) -> RequestOutcome {
    let mut params = RequestParams::new();
    params
        .insert("value", GeneralParameter::new(ty, value).unwrap())
        .unwrap();
    session.execute_request(Request::with_params("RETURN $value", params))
}

fn assert_foreign(outcome: &RequestOutcome) {
    let error = outcome.error().expect("foreign reference rejected");
    assert_eq!(error.gqlstatus().unwrap().as_str(), "42002");
    assert!(error.message().contains("another database"));
}

#[test]
fn graph_and_node_values_retain_their_ownership_domain() {
    let session = fixture("reference_nodes");
    let other = fixture("reference_foreign");
    let graph_ty = Type::new(TypeKind::GraphRef, true).unwrap();
    let graph = Value::GraphRef(session.graph_reference().unwrap());
    assert!(
        execute_parameter(&session, graph_ty.clone(), graph.clone())
            .error()
            .is_none()
    );
    assert_foreign(&execute_parameter(&other, graph_ty, graph));
    session.execute("INSERT (:Live) FINISH").unwrap();
    other.execute("INSERT (:Live) FINISH").unwrap();
    let node = Value::NodeRef(session.node_reference(NodeId::new(1)).unwrap());
    assert!(
        execute_parameter(&session, Type::NODE, node.clone())
            .error()
            .is_none()
    );
    assert_foreign(&execute_parameter(&other, Type::NODE, node.clone()));
    session.execute("MATCH (n:Live) DELETE n FINISH").unwrap();
    assert!(
        execute_parameter(&session, Type::NODE, node)
            .error()
            .is_none()
    );
}

#[test]
fn edge_and_path_values_preserve_ownership_and_remain_copyable_after_deletion() {
    let session = fixture("reference_paths");
    let other = fixture("reference_foreign_paths");
    session.execute("INSERT (:A)-[:STEP]->(:B) FINISH").unwrap();
    other.execute("INSERT (:A)-[:STEP]->(:B) FINISH").unwrap();
    let start = session.node_reference(NodeId::new(1)).unwrap();
    let end = session.node_reference(NodeId::new(2)).unwrap();
    let edge = session.edge_reference(EdgeId::new(1)).unwrap();
    let step = ValuePathSegment::new(edge, EdgeDirection::Outgoing, end);
    let path = Value::Path(Box::new(
        session.path_reference(start, vec![step.clone()]).unwrap(),
    ));
    assert!(
        execute_parameter(&session, Type::PATH, path.clone())
            .error()
            .is_none()
    );
    assert_foreign(&execute_parameter(&other, Type::PATH, path.clone()));
    assert_foreign(&execute_parameter(&other, Type::EDGE, Value::EdgeRef(edge)));
    let error = session.path_reference(end, vec![step.clone()]).unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "42002");
    assert!(error.message().contains("not connected"));
    session
        .execute("MATCH ()-[e:STEP]->() DELETE e FINISH")
        .unwrap();
    assert!(
        execute_parameter(&session, Type::EDGE, Value::EdgeRef(edge))
            .error()
            .is_none()
    );
    assert!(
        execute_parameter(&session, Type::PATH, path)
            .error()
            .is_none()
    );
    assert_eq!(
        session
            .path_reference(start, vec![step])
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "22G11"
    );
}

#[test]
fn nested_foreign_references_cannot_hide_in_untyped_containers() {
    let session = fixture("reference_nested");
    let other = fixture("reference_nested_foreign");
    session.execute("INSERT (:Live) FINISH").unwrap();
    other.execute("INSERT (:Live) FINISH").unwrap();
    let reference = Value::NodeRef(other.node_reference(NodeId::new(1)).unwrap());
    for (ty, value) in [
        (
            Type::list(Type::list(Type::NODE, None).unwrap(), None).unwrap(),
            Value::List(vec![Value::List(vec![reference.clone()])]),
        ),
        (
            Type::new(selene_db::TypeKind::Record(None), true).unwrap(),
            Value::Record(Box::new(Record::Open(vec![(
                db_string("nested").unwrap(),
                reference,
            )]))),
        ),
    ] {
        assert_foreign(&execute_parameter(&session, ty, value));
    }
}
