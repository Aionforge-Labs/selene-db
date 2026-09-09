use super::*;

#[test]
fn insert_node_with_label_creates_node_in_graph() {
    let graph = empty_graph();
    let plan = planned("INSERT (n:Person) RETURN n");

    let (table, outcome) = run_write(&graph, &plan).expect("write executes");

    let node = first_node(&table, "n");
    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 1);
    assert!(
        snapshot
            .node_labels(node)
            .unwrap()
            .contains(&db_string("Person"))
    );
    assert!(matches!(
        outcome.changes.as_slice(),
        [Change::NodeCreated { .. }]
    ));
}

#[test]
fn insert_node_with_label_conjunction_creates_multi_label_node() {
    let graph = empty_graph();
    let plan = planned("INSERT (n:Person&Customer {name: 'Alice'}) RETURN n");

    let (table, outcome) = run_write(&graph, &plan).expect("write executes");

    let node = first_node(&table, "n");
    let snapshot = graph.read();
    let labels = snapshot.node_labels(node).expect("node labels exist");
    assert_eq!(labels.len(), 2);
    assert!(labels.contains(&db_string("Customer")));
    assert!(labels.contains(&db_string("Person")));
    assert!(matches!(
        outcome.changes.as_slice(),
        [Change::NodeCreated { labels, .. }]
            if labels == &LabelSet::from_iter([db_string("Customer"), db_string("Person")])
    ));
}

#[test]
fn insert_node_extends_row_with_new_node_id_at_planner_assigned_column() {
    let graph = graph_with_person("Alice");
    let plan = planned("MATCH (p:Person) INSERT (n:Person) RETURN p, n");

    let (table, _) = run_write(&graph, &plan).expect("write executes");

    assert_eq!(
        table.schema().columns[0].name.clone().unwrap().as_str(),
        "p"
    );
    assert_eq!(
        table.schema().columns[1].name.clone().unwrap().as_str(),
        "n"
    );
    assert!(matches!(table.rows()[0].get(0), Some(Value::NodeRef(_))));
    assert!(matches!(table.rows()[0].get(1), Some(Value::NodeRef(_))));
}

#[test]
fn insert_node_with_property_initializers_evaluates_per_row() {
    let graph = graph_with_person("Alice");
    let plan = planned("MATCH (p:Person) INSERT (n:\"Copy\" {name: p.name}) RETURN n.name AS name");

    let (table, _) = run_write(&graph, &plan).expect("write executes");

    assert_eq!(
        column_values(&table, "name"),
        [Value::String(db_string("Alice"))]
    );
}

#[test]
fn insert_edge_between_two_matched_bindings_creates_edge() {
    let graph = empty_graph();
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::single(db_string("A")), PropertyMap::new())
            .expect("node inserts");
        mutator
            .create_node(LabelSet::single(db_string("B")), PropertyMap::new())
            .expect("node inserts");
        txn.commit().expect("fixture commits");
    }
    let plan = planned("MATCH (a:A), (b:B) INSERT (a)-[:REL]->(b) RETURN a, b");

    let (_, _) = run_write(&graph, &plan).expect("write executes");

    assert_eq!(graph.read().edge_count(), 1);
}

#[test]
fn insert_edge_without_label_reports_edge_label_minimum_error() {
    let graph = empty_graph();
    let plan = planned("INSERT (:A)-[]->(:B) RETURN 1 AS ok");

    let err = run_write(&graph, &plan).expect_err("unlabeled INSERT edge rejects");

    assert_eq!(
        err.gqlstatus(),
        GqlStatus::EDGE_LABELS_BELOW_SUPPORTED_MINIMUM
    );
    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 0);
    assert_eq!(snapshot.edge_count(), 0);
}

#[test]
fn insert_edge_label_conjunction_reports_edge_label_maximum_error() {
    let graph = empty_graph();
    let plan = planned("INSERT (:A)-[:REL&ALT]->(:B) RETURN 1 AS ok");

    let err = run_write(&graph, &plan).expect_err("multi-label INSERT edge rejects");

    assert_eq!(
        err.gqlstatus(),
        GqlStatus::EDGE_LABELS_EXCEED_SUPPORTED_MAXIMUM
    );
    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 0);
    assert_eq!(snapshot.edge_count(), 0);
}

#[test]
fn forged_union_insert_edge_is_rejected_at_runtime() {
    let graph = empty_graph();
    let mut plan = planned("INSERT (:A)-[:REL]->(:B) RETURN 1 AS ok");
    let edge = plan
        .pipeline
        .iter_mut()
        .find_map(|op| match op {
            PipelineOp::Mutation(MutationOp::InsertEdge { direction, .. }) => Some(direction),
            _ => None,
        })
        .expect("plan inserts an edge");
    *edge = EdgeDirection::Any;

    let err = run_write(&graph, &plan).expect_err("union INSERT edge rejects");

    assert!(matches!(
        err,
        ExecutorError::FeatureNotSupportedYet {
            feature: "INSERT edge orientation union",
            ..
        }
    ));
    assert_eq!(graph.read().edge_count(), 0);
}

#[test]
fn chain_insert_with_anonymous_middle_node_links_correctly() {
    let graph = empty_graph();
    let plan = planned("INSERT (a:A)-[:R]->(:M)-[:S]->(b:B) RETURN a, b");

    let (table, _) = run_write(&graph, &plan).expect("write executes");

    let a = first_node(&table, "a");
    let b = first_node(&table, "b");
    let snapshot = graph.read();
    let middle = snapshot
        .outgoing_edges(a)
        .and_then(|edges| edges.iter().next())
        .map(|edge| edge.neighbor)
        .expect("a has outgoing edge");
    assert_ne!(middle, b);
    assert_eq!(
        snapshot
            .outgoing_edges(middle)
            .and_then(|edges| edges.iter().next())
            .map(|edge| edge.neighbor),
        Some(b)
    );
}

#[test]
fn multi_row_insert_preserves_input_row_order_in_new_id_column() {
    let fixture = exec_common::ExecFixture::build();
    let plan = planned("MATCH (p:Person) INSERT (n:\"Copy\" {name: p.name}) RETURN n");

    let (table, _) = run_write(&fixture.graph, &plan).expect("write executes");
    let ids = column_values(&table, "n")
        .into_iter()
        .map(|value| match value {
            Value::NodeRef(id) => id.get(),
            other => panic!("expected node ref, got {other:?}"),
        })
        .collect::<Vec<_>>();

    assert_eq!(ids.len(), 3);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn insert_with_label_disjunction_returns_feature_not_in_v1_1() {
    let graph = empty_graph();
    let plan = planned("INSERT (n:Person|Company) RETURN n");

    let err = run_write(&graph, &plan).expect_err("label expression errors");

    assert!(matches!(
        err,
        ExecutorError::FeatureNotSupportedYet {
            feature: "INSERT label expression form",
            ..
        }
    ));
}
