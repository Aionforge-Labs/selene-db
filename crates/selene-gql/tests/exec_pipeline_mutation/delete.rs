use super::*;

#[test]
fn detach_delete_node_cascades_incident_edges() {
    let graph = graph_with_edge();
    let plan = planned("MATCH (n:Victim) DETACH DELETE n FINISH");

    let (_, _) = run_write(&graph, &plan).expect("write executes");

    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 1);
    assert_eq!(snapshot.edge_count(), 0);
}

#[test]
fn bare_delete_node_with_incident_edges_returns_g1001() {
    let graph = graph_with_edge();
    let plan = planned("MATCH (n:Victim) DELETE n FINISH");

    let err = run_write(&graph, &plan).expect_err("strict delete errors");

    assert!(matches!(
        err,
        ExecutorError::DependentObjectStillExists { .. }
    ));
    assert_eq!(err.gqlstatus(), GqlStatus::DEPENDENT_OBJECT_STILL_EXISTS);
}

#[test]
fn delete_edge_removes_edge_only() {
    let graph = graph_with_edge();
    let plan = planned("MATCH ()-[e:REL]->() DELETE e FINISH");

    let (_, _) = run_write(&graph, &plan).expect("write executes");

    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 2);
    assert_eq!(snapshot.edge_count(), 0);
}

#[test]
fn repeated_delete_of_same_binding_is_noop_after_first_delete() {
    let graph = graph_with_person("Alice");
    let plan = planned("MATCH (n:Person) DETACH DELETE n DETACH DELETE n FINISH");

    let (_, outcome) = run_write(&graph, &plan).expect("write executes");

    assert_eq!(outcome.changes.len(), 1);
    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn set_after_delete_of_same_binding_errors_atomically() {
    let graph = graph_with_person("Alice");
    let plan = planned("MATCH (n:Person) DETACH DELETE n SET n.age = 31 FINISH");

    let err = run_write(&graph, &plan).expect_err("stale binding update rejects");

    assert!(matches!(
        err,
        ExecutorError::DataException {
            subclass: selene_gql::DataExceptionSubclass::InvalidReferenceValue,
            ..
        }
    ));
    let snapshot = graph.read();
    assert!(snapshot.is_node_alive(NodeId::new(1)));
    assert_eq!(
        snapshot
            .node_properties(NodeId::new(1))
            .and_then(|props| props.get(&db_string("age"))),
        Some(&Value::Int(30))
    );
}

#[test]
fn bare_delete_node_and_its_edge_succeeds_as_one_delete_set() {
    let graph = graph_with_edge();
    let plan = planned("MATCH (a:Victim)-[e:REL]->(b:Other) DELETE a, e FINISH");

    let (_, _) = run_write(&graph, &plan).expect("write executes");

    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 1);
    assert_eq!(snapshot.edge_count(), 0);
    assert!(snapshot.node_properties(NodeId::new(2)).is_some());
}

#[test]
fn duplicate_delete_edge_rows_are_deduplicated() {
    let graph = graph_with_extra_incident_edges();
    let plan = planned("MATCH (a:Victim)-[e:REL]->(:Other), (a)-[:HINT]->(:Hint) DELETE e FINISH");

    let (_, _) = run_write(&graph, &plan).expect("write executes");

    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 4);
    assert_eq!(snapshot.edge_count(), 2);
    assert!(snapshot.edge_properties(EdgeId::new(1)).is_none());
}

#[test]
fn bare_delete_node_and_some_edges_rejects_outside_incident_edge_atomically() {
    let graph = graph_with_extra_incident_edges();
    let plan = planned("MATCH (a:Victim)-[e:REL]->(:Other) DELETE a, e FINISH");

    let err = run_write(&graph, &plan).expect_err("outside incident edge rejects");

    assert!(matches!(
        err,
        ExecutorError::DependentObjectStillExists { .. }
    ));
    assert_eq!(err.gqlstatus(), GqlStatus::DEPENDENT_OBJECT_STILL_EXISTS);
    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 4);
    assert_eq!(snapshot.edge_count(), 3);
}

#[test]
fn delete_path_target_rejects_at_analysis() {
    let statement = parse("MATCH p = (a)-[:REL]->(b) DELETE p FINISH").expect("parses");

    let err = analyze(statement, &EmptyProcedureRegistry, None).expect_err("path delete rejects");

    assert!(matches!(
        err,
        AnalysisError::InvalidReference { ref message, .. }
            if message.contains("DELETE target must be a node or edge binding")
    ));
    assert_eq!(err.gqlstatus(), GqlStatus::INVALID_REFERENCE);
}
