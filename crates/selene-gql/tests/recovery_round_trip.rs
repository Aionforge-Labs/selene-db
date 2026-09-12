//! Runtime-produced changes consumed by native format-2 replay.

#[path = "../../selene-graph/tests/format2_support/mod.rs"]
mod format2_support;

use selene_core::{Change, GraphId, LabelSet, NodeId, PropertyMap, Value, db_string};
use selene_gql::{
    Binding, BindingTable, BindingTableSchema, EmptyProcedureRegistry, ExecutionPlan,
    ExecutorError, TxContext, analyze, execute_pattern, execute_pipeline, parse, plan,
};
use selene_graph::{CommitOutcome, SharedGraph};

fn planned(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn seed_table() -> BindingTable {
    BindingTable::new(
        BindingTableSchema {
            columns: Vec::new(),
        },
        vec![Binding::empty()],
    )
}

fn run_write(
    graph: &SharedGraph,
    plan: &ExecutionPlan,
) -> Result<(BindingTable, CommitOutcome), ExecutorError> {
    let snapshot = graph.read();
    let mut txn = graph.begin_write();
    let result = {
        let mut ctx = TxContext::write(
            snapshot,
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            &mut txn,
            graph.index_providers(),
        );
        let input = if let Some(pattern) = &plan.pattern_plan {
            execute_pattern(pattern, &ctx)?
        } else {
            seed_table()
        };
        execute_pipeline(&plan.pipeline, input, &mut ctx)
    };
    match result {
        Ok(table) => {
            let outcome = txn.commit().expect("write commits");
            Ok((table, outcome))
        }
        Err(error) => {
            txn.rollback();
            Err(error)
        }
    }
}

fn node_ref(table: &BindingTable, column: &str) -> NodeId {
    let index = table
        .schema()
        .columns
        .iter()
        .position(|col| col.name.clone().is_some_and(|name| name.as_str() == column))
        .expect("column exists");
    match table.rows()[0].get(index).expect("row value exists") {
        Value::NodeRef(id) => *id,
        other => panic!("expected node ref for {column}, got {other:?}"),
    }
}

#[test]
fn format2_replay_via_runtime_mutation_pipeline() {
    let graph_id = GraphId::new(9308);
    let graph = SharedGraph::new(graph_id);
    let plan =
        planned("INSERT (a:Person {name: 'alice'})-[:KNOWS]->(b:Person {name: 'bob'}) RETURN a, b");

    let (table, outcome) = run_write(&graph, &plan).expect("runtime write executes");
    let alice = node_ref(&table, "a");
    let bob = node_ref(&table, "b");
    let recovered = format2_support::replay(graph_id, outcome.changes.clone()).unwrap();
    let snapshot = recovered.read();
    let person = db_string("Person").unwrap();
    let knows = db_string("KNOWS").unwrap();
    let name = db_string("name").unwrap();

    assert!(matches!(
        outcome.changes.as_slice(),
        [
            Change::NodeCreated { .. },
            Change::NodeCreated { .. },
            Change::EdgeCreated { .. }
        ]
    ));
    assert_eq!(snapshot.node_count(), 2);
    assert_eq!(snapshot.edge_count(), 1);
    assert_eq!(
        snapshot.node_labels(alice),
        Some(&LabelSet::single(person.clone()))
    );
    assert_eq!(snapshot.node_labels(bob), Some(&LabelSet::single(person)));
    assert_eq!(
        snapshot
            .node_properties(alice)
            .and_then(|props| props.get(&name)),
        Some(&Value::String(db_string("alice").unwrap()))
    );
    assert_eq!(
        snapshot
            .node_properties(bob)
            .and_then(|props| props.get(&name)),
        Some(&Value::String(db_string("bob").unwrap()))
    );
    assert_eq!(
        snapshot.edge_label(selene_core::EdgeId::new(1)),
        Some(&knows)
    );
    assert_eq!(
        snapshot.edge_endpoints(selene_core::EdgeId::new(1)),
        Some((alice, bob))
    );
    assert_eq!(
        snapshot.edge_properties(selene_core::EdgeId::new(1)),
        Some(&PropertyMap::new())
    );
}
