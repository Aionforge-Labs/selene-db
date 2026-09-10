//! BRIEF-36 mutation pipeline executor tests.

mod exec_common;

use selene_core::{Change, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, Value};
use selene_gql::{
    AnalysisError, AnalyzedType, Binding, BindingTable, BindingTableColumn, BindingTableSchema,
    EdgeDirection, EmptyProcedureRegistry, ExecutionPlan, ExecutorError, GqlStatus, GqlType,
    MutationOp, PipelineOp, TxContext, analyze, execute_pattern, execute_pipeline, parse, plan,
};
use selene_graph::{CommitOutcome, SharedGraph};

use exec_common::{column_values, db_string, props};

fn planned(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn seed_table() -> BindingTable {
    BindingTable::new(
        BindingTableSchema { columns: vec![] },
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

fn empty_graph() -> SharedGraph {
    SharedGraph::new(GraphId::new(3600))
}

fn graph_with_person(name: &str) -> SharedGraph {
    let graph = empty_graph();
    {
        let person = db_string("Person");
        let name_key = db_string("name");
        let age = db_string("age");
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(person),
                props([
                    (name_key, Value::String(db_string(name))),
                    (age, Value::Int(30)),
                ]),
            )
            .expect("person inserts");
        txn.commit().expect("fixture commits");
    }
    graph
}

fn graph_with_edge() -> SharedGraph {
    let graph = empty_graph();
    {
        let victim = db_string("Victim");
        let other = db_string("Other");
        let rel = db_string("REL");
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let a = mutator
            .create_node(LabelSet::single(victim), PropertyMap::new())
            .expect("node inserts");
        let b = mutator
            .create_node(LabelSet::single(other), PropertyMap::new())
            .expect("node inserts");
        mutator
            .create_edge(rel, a, b, PropertyMap::new())
            .expect("edge inserts");
        txn.commit().expect("fixture commits");
    }
    graph
}

fn graph_with_extra_incident_edges() -> SharedGraph {
    let graph = empty_graph();
    {
        let victim = db_string("Victim");
        let other = db_string("Other");
        let hint = db_string("Hint");
        let rel = db_string("REL");
        let extra = db_string("HINT");
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let a = mutator
            .create_node(LabelSet::single(victim), PropertyMap::new())
            .expect("victim inserts");
        let b = mutator
            .create_node(LabelSet::single(other), PropertyMap::new())
            .expect("other inserts");
        let c = mutator
            .create_node(LabelSet::single(hint.clone()), PropertyMap::new())
            .expect("hint inserts");
        let d = mutator
            .create_node(LabelSet::single(hint), PropertyMap::new())
            .expect("hint inserts");
        mutator
            .create_edge(rel, a, b, PropertyMap::new())
            .expect("rel inserts");
        mutator
            .create_edge(extra.clone(), a, c, PropertyMap::new())
            .expect("extra inserts");
        mutator
            .create_edge(extra, a, d, PropertyMap::new())
            .expect("extra inserts");
        txn.commit().expect("fixture commits");
    }
    graph
}

fn first_node(table: &BindingTable, column: &str) -> NodeId {
    match column_values(table, column).first().expect("row exists") {
        Value::NodeRef(id) => *id,
        other => panic!("expected node ref, got {other:?}"),
    }
}

#[path = "exec_pipeline_mutation/delete.rs"]
mod delete;
#[path = "exec_pipeline_mutation/errors.rs"]
mod errors;
#[path = "exec_pipeline_mutation/insert.rs"]
mod insert;
#[path = "exec_pipeline_mutation/set_remove.rs"]
mod set_remove;
