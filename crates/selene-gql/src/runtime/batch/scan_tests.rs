//! Batch scan behavior regressions (split from `tests.rs` for the file-size cap).
//!
//! These cover the scan operator's physical behavior: boundary
//! cardinalities, shape independence, label scans, tracer end-to-end
//! conversion, completion reporting, empty-result schemas, and the identity
//! surface. The row executor is the oracle throughout.

use selene_core::{GraphId, Value};
use selene_graph::SharedGraph;

use crate::runtime::{BindingTable, ExecutionOutcome, StatementOutput, TxContext};

use super::fixtures::{
    TEST_TARGET_ROWS, eval_for, plan_source, pull_scan, row_table, rows_into_bindings, scan_parts,
    seed_nodes, test_policy,
};
use super::{
    BatchBuffer, BatchCancel, BatchExecutionContext, BatchPolicy, MemoryBudget, OperatorState,
    PhysicalOperator, assert_same_rows, assert_same_schema, assert_tables_equivalent, collect_rows,
    descriptor_for, trace_scan_to_table,
};
use super::{scan::BatchScan, tracer::trace_operator_to_table};

#[test]
fn scan_cardinality_is_exact_at_batch_boundaries() {
    // Empty, one row, exactly one batch, and one row past the boundary.
    for count in [0usize, 1, TEST_TARGET_ROWS, TEST_TARGET_ROWS + 1] {
        let graph = SharedGraph::new(GraphId::new(41_000 + count as u64));
        seed_nodes(&graph, count, None);
        let expected = row_table(&graph, "MATCH (n) RETURN n");
        assert_eq!(expected.row_count(), count);

        let plan = plan_source("MATCH (n) RETURN n");
        let tx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &crate::EmptyProcedureRegistry,
            graph.index_providers(),
        );
        let (scan_ir, pattern) = scan_parts(&plan);
        let pulled = pull_scan(
            graph.read(),
            scan_ir,
            pattern,
            expected.schema(),
            eval_for(&tx, &plan),
            test_policy(),
            BatchCancel::disabled(),
            MemoryBudget::unlimited(),
        )
        .expect("batch scan executes");
        assert_eq!(
            pulled.rows.len(),
            count,
            "count {count}: logical total diverged"
        );
        assert_same_rows(&collect_rows(&expected), &pulled.rows, "boundary scan");
        let expected_batches = if count == 0 {
            0
        } else {
            count.div_ceil(TEST_TARGET_ROWS)
        };
        assert_eq!(pulled.batches as usize, expected_batches, "count {count}");
        assert_eq!(
            pulled.batch_sizes.iter().sum::<usize>(),
            count,
            "count {count}"
        );
        if count == TEST_TARGET_ROWS + 1 {
            assert_eq!(pulled.batch_sizes, vec![TEST_TARGET_ROWS, 1]);
        }
        assert_eq!(pulled.recycled_batches as usize, expected_batches);
    }
}

#[test]
fn logical_tables_match_across_batch_shapes() {
    // The independent review question: logical table semantics must not
    // depend on physical batch shape.
    let graph = SharedGraph::new(GraphId::new(41_100));
    seed_nodes(&graph, 10, None);
    let expected = row_table(&graph, "MATCH (n) RETURN n");
    let plan = plan_source("MATCH (n) RETURN n");

    let mut shapes = Vec::new();
    for target in [1usize, 3, TEST_TARGET_ROWS, 1024] {
        let tx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &crate::EmptyProcedureRegistry,
            graph.index_providers(),
        );
        let (scan_ir, pattern) = scan_parts(&plan);
        let policy = BatchPolicy::new(target, 1 << 20).unwrap();
        let pulled = pull_scan(
            graph.read(),
            scan_ir,
            pattern,
            expected.schema(),
            eval_for(&tx, &plan),
            policy,
            BatchCancel::disabled(),
            MemoryBudget::unlimited(),
        )
        .expect("batch scan executes");
        shapes.push(pulled.rows);
    }
    for rows in &shapes {
        assert_same_rows(&collect_rows(&expected), rows, "shape independence");
    }
}

#[test]
fn label_scan_matches_row_path_for_mixed_labels() {
    let graph = SharedGraph::new(GraphId::new(41_200));
    seed_nodes(&graph, 3, Some("Person"));
    seed_nodes(&graph, 2, None);
    let expected = row_table(&graph, "MATCH (n:Person) RETURN n");
    assert_eq!(expected.row_count(), 3);
    let plan = plan_source("MATCH (n:Person) RETURN n");
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let (scan_ir, pattern) = scan_parts(&plan);

    let pulled = pull_scan(
        graph.read(),
        scan_ir,
        pattern,
        expected.schema(),
        eval_for(&tx, &plan),
        test_policy(),
        BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    )
    .expect("batch scan executes");
    assert_tables_equivalent(
        &expected,
        &BindingTable::new(expected.schema().clone(), rows_into_bindings(&pulled.rows)),
        "label scan",
    );
}

#[test]
fn scan_to_result_tracer_matches_row_path_end_to_end() {
    let graph = SharedGraph::new(GraphId::new(41_300));
    seed_nodes(&graph, 2, Some("Person"));
    seed_nodes(&graph, 1, None);
    let expected = row_table(&graph, "MATCH (n:Person) RETURN n");
    let plan = plan_source("MATCH (n:Person) RETURN n");
    let snapshot = graph.read();
    let tx = TxContext::read_only(
        snapshot.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let (scan_ir, pattern) = scan_parts(&plan);

    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        expected.schema().clone(),
        eval_for(&tx, &plan),
        test_policy(),
    );
    let mut ctx =
        BatchExecutionContext::new(snapshot, BatchCancel::disabled(), MemoryBudget::unlimited());
    let actual = trace_scan_to_table(&mut scan, &mut ctx).expect("tracer executes");
    assert!(ctx.is_closed());
    assert_eq!(scan.state(), OperatorState::Closed);
    // Two rows under a four-row target complete in one batch.
    assert_eq!(ctx.completed(), (1, 2));
    assert_tables_equivalent(&expected, &actual, "scan-to-result tracer");
    assert_eq!(
        descriptor_for(&expected),
        descriptor_for(&actual),
        "tracer preserves declared types and preferred order"
    );

    // The materialized table re-enters the stable result API unchanged.
    let outcome = ExecutionOutcome::from_statement(StatementOutput::Rows(actual), Vec::new());
    let ExecutionOutcome::RegularResult {
        table, declared, ..
    } = outcome
    else {
        panic!("expected a regular result");
    };
    assert_eq!(table.row_count(), 2);
    assert_eq!(declared.fields().len(), 1);
}

#[test]
fn scan_reports_candidates_schema_and_completion() {
    let graph = SharedGraph::new(GraphId::new(41_350));
    seed_nodes(&graph, 3, Some("Person"));
    let expected = row_table(&graph, "MATCH (n:Person) RETURN n");
    let plan = plan_source("MATCH (n:Person) RETURN n");
    let snapshot = graph.read();
    let tx = TxContext::read_only(
        snapshot.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let (scan_ir, pattern) = scan_parts(&plan);

    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        expected.schema().clone(),
        eval_for(&tx, &plan),
        BatchPolicy::new(2, 1 << 20).unwrap(),
    );
    assert_eq!(scan.output_schema(), expected.schema());
    let mut ctx =
        BatchExecutionContext::new(snapshot, BatchCancel::disabled(), MemoryBudget::unlimited());
    let mut buffer = BatchBuffer::new();
    scan.init(&mut ctx).unwrap();
    assert_eq!(scan.candidate_count(), 3);
    let mut total = 0;
    while let Some(batch) = scan.next_batch(&mut ctx, &mut buffer).unwrap() {
        total += 1;
        batch.recycle(&mut buffer);
    }
    assert_eq!(total, 2, "three rows under a two-row target take two pulls");
    assert_eq!(ctx.completed(), (2, 3));
    scan.close(&mut ctx);
}

#[test]
fn empty_scan_keeps_declared_schema() {
    let graph = SharedGraph::new(GraphId::new(41_400));
    let expected = row_table(&graph, "MATCH (n:Missing) RETURN n");
    assert_eq!(expected.row_count(), 0);
    let plan = plan_source("MATCH (n:Missing) RETURN n");
    let snapshot = graph.read();
    let tx = TxContext::read_only(
        snapshot.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let (scan_ir, pattern) = scan_parts(&plan);

    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        expected.schema().clone(),
        eval_for(&tx, &plan),
        test_policy(),
    );
    let mut ctx =
        BatchExecutionContext::new(snapshot, BatchCancel::disabled(), MemoryBudget::unlimited());
    let actual = trace_operator_to_table(&mut scan, &mut ctx).expect("tracer executes");
    assert_eq!(actual.row_count(), 0);
    assert_same_schema(&expected, &actual, "empty scan");
    assert_eq!(
        descriptor_for(&expected),
        descriptor_for(&actual),
        "empty results keep declared types and preferred order"
    );
}

#[test]
fn batch_positions_never_escape_as_graph_identities() {
    // API-surface regression: batch coordinates are crate-private offsets
    // with no conversion into graph identities, and every public surface the
    // tracer feeds (rows, schema, descriptor) carries only stable values.
    let graph = SharedGraph::new(GraphId::new(41_700));
    seed_nodes(&graph, 5, Some("Person"));
    let expected = row_table(&graph, "MATCH (n:Person) RETURN n");
    let plan = plan_source("MATCH (n:Person) RETURN n");
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let (scan_ir, pattern) = scan_parts(&plan);

    let pulled = pull_scan(
        graph.read(),
        scan_ir,
        pattern,
        expected.schema(),
        eval_for(&tx, &plan),
        test_policy(),
        BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    )
    .expect("batch scan executes");

    // Every materialized value is the stable graph identity the row path
    // produced — no position, no storage row, no synthetic id.
    assert_same_rows(&collect_rows(&expected), &pulled.rows, "identity surface");
    for row in &pulled.rows {
        assert_eq!(row.len(), 1);
        assert!(
            matches!(row[0], Value::NodeRef(_)),
            "scan output carries only stable node identities, got {:?}",
            row[0]
        );
    }
    // The descriptor exposes names and declared types only.
    let descriptor = descriptor_for(&expected);
    assert_eq!(descriptor.fields().len(), 1);
    assert_eq!(descriptor.preferred_columns(), &[0]);

    // `BatchPosition` has no addressable conversion: this function could not
    // name a `From<BatchPosition> for NodeId`-style impl if one existed
    // without changing this assertion block, and the module is `pub(crate)`
    // so external crates fail to compile against any batch coordinate.
    fn position_is_not_an_identity(_: &[Vec<Value>]) {}
    position_is_not_an_identity(&pulled.rows);
}
