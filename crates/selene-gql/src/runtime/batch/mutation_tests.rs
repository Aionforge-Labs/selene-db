//! Physical mutation boundaries, metadata, cancellation, and statement barriers.

use selene_core::{CancellationToken, GraphId, Value};
use selene_graph::SharedGraph;

use crate::{
    EmptyProcedureRegistry, ExecutionPlan, PipelineOp,
    runtime::{Binding, BindingTable, ExecutorError, Session, TxContext},
};

use super::{fixtures::kernel_column, mutation::PhysicalMutation, policy::BatchPolicy};

fn planned(source: &str) -> ExecutionPlan {
    let ast = crate::parse(source).unwrap();
    let analyzed = crate::analyze(ast, &EmptyProcedureRegistry, None).unwrap();
    crate::plan(&analyzed, &EmptyProcedureRegistry).unwrap()
}

fn input(count: usize) -> BindingTable {
    BindingTable::new(
        crate::BindingTableSchema {
            columns: vec![kernel_column("k")],
        },
        (0..count)
            .map(|k| Binding::new([Value::Int(k as i64)]))
            .collect(),
    )
}

#[test]
fn bounded_mutations_share_transaction_and_preserve_anonymous_endpoints() {
    let plan = planned("INSERT (:A {k: 1})-[:E]->(:B)");
    for size in [1, 4, 1024] {
        for count in [0, 1, 3, 4, 5, 2051] {
            let graph = SharedGraph::new(GraphId::new(44_100));
            let before = graph.read();
            let mut txn = graph.begin_write();
            let mut ctx = TxContext::write(
                before.clone(),
                &plan.impl_defined_caps,
                &EmptyProcedureRegistry,
                &mut txn,
                graph.index_providers(),
            );
            let mut table = input(count);
            let mut calls = 0;
            for op in &plan.pipeline {
                if let PipelineOp::Mutation(op) = op {
                    let mut batches = Vec::new();
                    table = PhysicalMutation::new(
                        op,
                        &plan.expr_ids,
                        &plan.subqueries,
                        BatchPolicy::new(size, usize::MAX).unwrap(),
                    )
                    .execute_observing(table, &mut ctx, |rows, _| batches.push(rows))
                    .unwrap();
                    assert_eq!(batches.iter().sum::<usize>(), count);
                    assert_eq!(batches.len(), count.div_ceil(size));
                    assert!(batches.iter().all(|&rows| rows <= size));
                    calls += 1;
                }
            }
            assert_eq!(calls, 3);
            assert_eq!(table.row_count(), count);
            assert_eq!(ctx.snapshot().node_count(), count * 2);
            assert_eq!(ctx.snapshot().edge_count(), count);
            assert_eq!(graph.read().node_count(), 0, "no batch publishes");
            drop(ctx);
            txn.commit().unwrap();
            assert_eq!(graph.read().meta.generation, before.meta.generation + 1);
        }
    }
}

#[test]
fn cancellation_after_first_batch_discards_output_and_owner_rolls_back() {
    let plan = planned("INSERT (:A {k: 1})");
    let op = plan
        .pipeline
        .iter()
        .find_map(|op| match op {
            PipelineOp::Mutation(op) => Some(op),
            _ => None,
        })
        .unwrap();
    let graph = SharedGraph::new(GraphId::new(44_101));
    let cancellation = CancellationToken::new();
    let mut txn = graph.begin_write();
    let mut ctx = TxContext::write(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        &mut txn,
        graph.index_providers(),
    )
    .with_resource_limits(Some(&cancellation), None, None, None);
    let mut batches = 0;
    let error = PhysicalMutation::new(
        op,
        &plan.expr_ids,
        &plan.subqueries,
        BatchPolicy::new(4, usize::MAX).unwrap(),
    )
    .execute_observing(input(9), &mut ctx, |rows, ctx| {
        batches += 1;
        assert_eq!(rows, 4);
        assert_eq!(ctx.snapshot().node_count(), 4);
        cancellation.cancel();
    })
    .unwrap_err();
    assert!(matches!(error, ExecutorError::Cancelled { .. }));
    assert_eq!(batches, 1);
    drop(ctx);
    txn.rollback();
    assert_eq!(graph.read().node_count(), 0);
    // A new writer can acquire the reservation immediately after cleanup.
    Session::new(&graph)
        .execute_source("INSERT (:AfterCancel)", &EmptyProcedureRegistry)
        .unwrap();
}

#[test]
fn unit_and_empty_tables_have_distinct_mutation_cardinality() {
    for (source, expected) in [("INSERT (:A)", 1), ("FILTER false INSERT (:A)", 0)] {
        let graph = SharedGraph::new(GraphId::new(44_102));
        Session::new(&graph)
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap();
        assert_eq!(graph.read().node_count(), expected);
    }
}

#[test]
fn read_only_effect_gate_rejects_nested_call_even_with_empty_input() {
    let registry = crate::BuiltinProcedureRegistry::new();
    let ast = crate::parse("CALL selene.create_text_index('A', 'text')").unwrap();
    let analyzed = crate::analyze(ast, &registry, None).unwrap();
    let nested = crate::plan(&analyzed, &registry).unwrap();
    let mut plan = planned("RETURN 1");
    plan.pipeline = vec![PipelineOp::CorrelatedChain(Box::new(nested))];
    assert!(crate::plan::classify_plan(&plan).rejects_in_read_only());
    let graph = SharedGraph::new(GraphId::new(44_105));
    let ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &registry,
        graph.index_providers(),
    );
    let error = crate::runtime::plan_runner::execute_plan_read_only_with_seed(
        &plan,
        Some(BindingTable::new(
            crate::BindingTableSchema {
                columns: Vec::new(),
            },
            Vec::new(),
        )),
        &ctx,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ExecutorError::InvalidTransactionState { .. }
    ));
    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn delete_collects_incident_edges_across_all_input_batches() {
    let graph = SharedGraph::new(GraphId::new(44_103));
    Session::new(&graph)
        .execute_source(
            "INSERT (a:A)-[:E]->(:B), (a)-[:E]->(:B)",
            &EmptyProcedureRegistry,
        )
        .unwrap();
    let plan = planned("MATCH (a:A)-[e:E]->(b:B) DELETE a, e");
    let mut txn = graph.begin_write();
    let mut ctx = TxContext::write(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        &mut txn,
        graph.index_providers(),
    );
    let prefix = super::query::try_execute_prefix(&plan, &ctx)
        .unwrap()
        .unwrap();
    assert_eq!(prefix.table.row_count(), 2);
    let PipelineOp::Mutation(op) = &plan.pipeline[prefix.suffix_from] else {
        panic!("mutation")
    };
    PhysicalMutation::new(
        op,
        &plan.expr_ids,
        &plan.subqueries,
        BatchPolicy::new(1, usize::MAX).unwrap(),
    )
    .execute(prefix.table, &mut ctx)
    .unwrap();
    assert_eq!(ctx.snapshot().node_count(), 2);
    assert_eq!(ctx.snapshot().edge_count(), 0);
    drop(ctx);
    txn.commit().unwrap();
}

#[test]
fn limit_after_mutation_does_not_stop_staging_and_close_releases_writer() {
    let graph = SharedGraph::new(GraphId::new(44_104));
    let mut session = Session::new(&graph);
    session
        .execute_source(
            "INSERT (:Seed), (:Seed), (:Seed), (:Seed), (:Seed)",
            &EmptyProcedureRegistry,
        )
        .unwrap();
    session
        .execute_source("START TRANSACTION", &EmptyProcedureRegistry)
        .unwrap();
    let mut plan = planned("MATCH (s:Seed) INSERT (n:A) RETURN n");
    // Exercise the physical barrier directly; mutation RETURN does not admit
    // a trailing LIMIT in the selected grammar.
    plan.pipeline.push(PipelineOp::Limit {
        offset: crate::LimitAmount::Literal(0),
        count: crate::LimitAmount::Literal(1),
    });
    let mut ctx = TxContext::write(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        session.active_txn.as_mut().unwrap(),
        graph.index_providers(),
    );
    assert_eq!(
        crate::runtime::execute_plan(&plan, &mut ctx)
            .unwrap()
            .row_count(),
        1
    );
    drop(ctx);
    assert_eq!(session.active_txn.as_ref().unwrap().read().node_count(), 10);
    session
        .execute_source("SESSION CLOSE", &EmptyProcedureRegistry)
        .unwrap();
    assert!(session.active_txn.is_none());
    assert_eq!(graph.read().node_count(), 5);
    Session::new(&graph)
        .execute_source("INSERT (:AfterClose)", &EmptyProcedureRegistry)
        .unwrap();
}
