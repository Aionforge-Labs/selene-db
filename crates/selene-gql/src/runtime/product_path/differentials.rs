//! Batch/row shared-operator comparisons and product-source lifecycle tests.

use super::*;
use super::{
    oracle::{Fixture, canonical},
    tests::analyzed,
};
use crate::runtime::batch::{filter::BatchFilter, page::BatchPage, project::BatchProject};
use crate::runtime::{EvalCtx, execute_pattern, execute_pipeline};
use crate::{EmptyProcedureRegistry, ImplDefinedCaps, lower_path_automata_with_defaults, plan};

/// Actual statement lowering and production batch assembly, not the direct
/// native constructor. The independent models call this as a second consumer.
pub(super) fn statement_table(f: &Fixture, source: &str, size: usize) -> BindingTable {
    let a = analyzed(source);
    let plan = plan(&a, &EmptyProcedureRegistry).unwrap();
    let caps = ImplDefinedCaps::default();
    let tx = TxContext::read_only(
        f.graph.read(),
        &caps,
        &EmptyProcedureRegistry,
        f.graph.index_providers(),
    );
    crate::runtime::batch::query::execute_with_test_policy(
        &plan,
        &tx,
        BatchPolicy::new(size, 1 << 20).unwrap(),
    )
    .expect("complete physical path execution")
}

#[test]
fn physical_path_charges_recursive_input_storage_and_rejects_missing_binding_metadata() {
    use selene_core::Value;
    let f = Fixture::new(2, &[(0, 1, true, true)]);
    let a = analyzed("MATCH p = (a)-[r{0,1}]->(b) RETURN p");
    let plan = plan(&a, &EmptyProcedureRegistry).unwrap();
    let pattern = plan.pattern_plan.as_ref().unwrap();
    let crate::JoinTree::Paths(program) = &pattern.join_tree else {
        panic!("path program")
    };
    let mut malformed = program.as_ref().clone();
    malformed.bindings.clear();
    malformed.schema.columns.clear();
    assert!(BoundedPathProgram::from_plan(&malformed).is_err());
    let caps = ImplDefinedCaps::default();
    let tx = TxContext::read_only(
        f.graph.read(),
        &caps,
        &EmptyProcedureRegistry,
        f.graph.index_providers(),
    );
    let eval = EvalCtx {
        tx: &tx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let mut schema = crate::runtime::pattern::schema_for_pattern(pattern);
    schema.columns.push(crate::BindingTableColumn {
        name: Some(selene_core::db_string("payload").unwrap()),
        hidden: None,
        ty: crate::AnalyzedType::Resolved(crate::GqlType::List(Box::new(crate::GqlType::Integer))),
    });
    let mut values = vec![Value::Null; schema.columns.len() - 1];
    values.push(Value::List(vec![Value::Int(0); 256]));
    let mut operator = BatchPath::new(
        program,
        eval,
        schema,
        Some(crate::Binding::new(values)),
        BatchPolicy::default_policy(),
    );
    let mut ctx = BatchExecutionContext::borrowed(
        tx.snapshot(),
        tx.batch_cancel(),
        MemoryBudget::new(32 * 1024),
    );
    assert!(matches!(
        trace_operator_to_table(&mut operator, &mut ctx),
        Err(ExecutorError::ProgramLimitExceeded {
            detail: "batch memory budget exceeded",
            ..
        })
    ));
    assert_eq!(ctx.budget_used(), 0);
    assert!(ctx.is_closed());
}

#[test]
fn statement_batches_keep_correlation_multiplicity_and_bound_null_across_pulls() {
    let f = Fixture::new(3, &vec![(0, 1, true, true); 30]);
    let source = "MATCH (input), (replica) FILTER input IS NOT NULL MATCH ALL SHORTEST p = (input)-[r{0,1}]->(b) RETURN input, r, p";
    for size in [1, 2, 7, 1024] {
        let table = statement_table(&f, source, size);
        assert_eq!(table.row_count(), 99);
        let mut counts = [0; 3];
        for row in table.rows() {
            let [
                selene_core::Value::NodeRef(input),
                selene_core::Value::List(group),
                selene_core::Value::Path(path),
            ] = row.values()
            else {
                panic!("typed path batch")
            };
            assert_eq!(*input, path.start);
            assert_eq!(group.len(), path.segments.len());
            counts[f.nodes.iter().position(|n| n == input).unwrap()] += 1;
        }
        assert_eq!(counts, [93, 3, 3]);
        let nulls = statement_table(
            &f,
            "MATCH (a:Root)-[r?]->(b) FILTER r IS NULL MATCH (c)-[r?]->(d) RETURN r, c, d",
            size,
        );
        assert_eq!(
            nulls.row_count(),
            3,
            "a bound questioned NULL must not become unbound"
        );
        for row in nulls.rows() {
            assert_eq!(row.values()[0], selene_core::Value::Null);
        }
    }
}

#[test]
fn product_source_and_row_patterns_agree_through_shared_batch_operators() {
    let f = Fixture::new(
        3,
        &[
            (0, 1, true, true),
            (0, 1, true, true),
            (1, 2, true, false),
            (1, 1, true, true),
        ],
    );
    for source in [
        "MATCH (a)-[r]->(b) RETURN a, r, b",
        "MATCH (a)-[r{0,2}]->(b) WHERE a <> b RETURN a, r, b",
        "MATCH (a)-[r{1,2}]->(m)-[s{0,1}]->(b) RETURN a, r, m, s, b",
        "MATCH (a)-[r?]->(b) RETURN a, r, b",
        "MATCH (a)-[r{0,2}]->(b) RETURN count(*) AS n",
    ] {
        let a = analyzed(source);
        let set = lower_path_automata_with_defaults(&a).unwrap();
        let program = BoundedPathProgram::compile(&set.automata, &a).unwrap();
        let plan = plan(&a, &EmptyProcedureRegistry).unwrap();
        let caps = ImplDefinedCaps::default();
        let mut tx = TxContext::read_only(
            f.graph.read(),
            &caps,
            &EmptyProcedureRegistry,
            f.graph.index_providers(),
        )
        .with_plan_metadata(&plan.expr_ids, &plan.subqueries);
        let input = execute_pattern(plan.pattern_plan.as_ref().unwrap(), &tx).unwrap();
        let expected = execute_pipeline(&plan.pipeline, input, &mut tx).unwrap();
        for size in [1, 2, 7, 1024] {
            let eval = EvalCtx {
                tx: &tx,
                expr_ids: &plan.expr_ids,
                subqueries: &plan.subqueries,
            };
            let mut root: Box<dyn PhysicalOperator + '_> = Box::new(ProductPathOperator::new(
                &program,
                PathExecutionLimits::default(),
                BatchPolicy::new(size, 1 << 20).unwrap(),
            ));
            for predicate in &plan.pattern_plan.as_ref().unwrap().filters {
                root = Box::new(BatchFilter::new(root, predicate, eval));
            }
            for op in &plan.pipeline {
                match op {
                    crate::PipelineOp::GroupBy { keys, aggregates } => {
                        root = Box::new(crate::runtime::batch::aggregate::BatchGroupBy::new(
                            root,
                            keys,
                            aggregates,
                            eval,
                            BatchPolicy::new(size, 1 << 20).unwrap(),
                        ));
                    }
                    crate::PipelineOp::Filter(predicate) => {
                        root = Box::new(BatchFilter::new(root, predicate, eval))
                    }
                    crate::PipelineOp::Project(items) => {
                        root = Box::new(BatchProject::new(
                            root,
                            items,
                            crate::runtime::pipeline::schema_for_items(items),
                            eval,
                        ))
                    }
                    _ => panic!("unexpected pipeline shape"),
                }
            }
            let mut ctx = BatchExecutionContext::borrowed(
                tx.snapshot(),
                tx.batch_cancel(),
                MemoryBudget::unlimited(),
            );
            let actual = trace_operator_to_table(root.as_mut(), &mut ctx).unwrap();
            assert_eq!(
                format!("{:?}", actual.schema()),
                format!("{:?}", expected.schema())
            );
            assert_eq!(
                canonical(actual.rows().iter().map(|r| r.values().to_vec()).collect()),
                canonical(
                    expected
                        .rows()
                        .iter()
                        .map(|r| r.values().to_vec())
                        .collect()
                ),
                "{source}"
            );
        }
    }
}

#[test]
fn source_lifecycle_budget_release_and_late_cancel_never_return_partial_table() {
    let f = Fixture::new(2, &[(0, 1, true, true)]);
    let a = analyzed("MATCH (a)-[r{0,2}]->(b) RETURN a");
    let set = lower_path_automata_with_defaults(&a).unwrap();
    let program = BoundedPathProgram::compile(&set.automata, &a).unwrap();
    let token = crate::CancellationToken::new();
    let snapshot = f.graph.read();
    let mut ctx = BatchExecutionContext::borrowed(
        &snapshot,
        crate::runtime::batch::budget::BatchCancel::new(Some(&token), None, None),
        MemoryBudget::new(1),
    );
    let mut operator = ProductPathOperator::new(
        &program,
        PathExecutionLimits::default(),
        BatchPolicy::default_policy(),
    );
    assert!(matches!(
        operator.init(&mut ctx),
        Err(ExecutorError::ProgramLimitExceeded {
            detail: "batch memory budget exceeded",
            ..
        })
    ));
    operator.close(&mut ctx);
    assert_eq!(ctx.budget_used(), 0);
    assert!(ctx.is_closed());
    let snapshot = f.graph.read();
    let mut ctx = BatchExecutionContext::borrowed(
        &snapshot,
        crate::runtime::batch::budget::BatchCancel::new(Some(&token), None, None),
        MemoryBudget::unlimited(),
    );
    let mut operator = ProductPathOperator::new(
        &program,
        PathExecutionLimits::default(),
        BatchPolicy::new(1, 1 << 20).unwrap(),
    );
    operator.init(&mut ctx).unwrap();
    assert!(operator.init(&mut ctx).is_err());
    let mut buffer = BatchBuffer::new();
    let batch = operator.next_batch(&mut ctx, &mut buffer).unwrap().unwrap();
    assert_eq!(batch.logical_rows(), 1);
    ctx.budget_mut().release(batch.estimated_bytes());
    batch.recycle(&mut buffer);
    token.cancel();
    assert!(matches!(
        operator.next_batch(&mut ctx, &mut buffer),
        Err(ExecutorError::Cancelled { .. })
    ));
    assert!(operator.next_batch(&mut ctx, &mut buffer).is_err());
    operator.close(&mut ctx);
    operator.close(&mut ctx);
    assert_eq!(ctx.budget_used(), 0);
    assert!(ctx.is_closed());
}

#[test]
fn limit_above_product_source_does_not_hide_search_failure() {
    let f = Fixture::new(1, &[(0, 0, true, true)]);
    let a = analyzed("MATCH (a)-[r{0,2}]->(b) RETURN a");
    let set = lower_path_automata_with_defaults(&a).unwrap();
    let program = BoundedPathProgram::compile(&set.automata, &a).unwrap();
    let root = ProductPathOperator::new(
        &program,
        PathExecutionLimits {
            max_rows: 1,
            ..Default::default()
        },
        BatchPolicy::default_policy(),
    );
    let mut page = BatchPage::new(Box::new(root), 0, 1);
    let snapshot = f.graph.read();
    let mut ctx = BatchExecutionContext::borrowed(
        &snapshot,
        crate::runtime::batch::budget::BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    );
    assert!(matches!(
        trace_operator_to_table(&mut page, &mut ctx),
        Err(ExecutorError::ProgramLimitExceeded {
            detail: "max_path_rows",
            ..
        })
    ));
    assert_eq!(ctx.budget_used(), 0);
    assert!(ctx.is_closed());
}
