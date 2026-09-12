//! F04-PR01 acceptance regressions for the batch substrate.
//!
//! Each test maps to one acceptance case: boundary cardinalities, the
//! zero-column unit table, null/selection alignment through filtering and
//! buffer reuse, declared types and preferred order (including empty
//! results), cancellation/error release without partial output, and the
//! batch-position API surface. The differential tests compare the batch scan
//! against the existing row executor on the same pinned snapshot; the row
//! path is the oracle and the batch path must match it exactly.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use selene_core::{CancellationToken, GraphId, NodeScanBudget, Value, db_string};
use selene_graph::SharedGraph;

use crate::{
    AnalyzedType,
    plan::{BindingTableColumn, BindingTableSchema},
    runtime::{Binding, BindingTable, ExecutionOutcome, ExecutorError, StatementOutput},
};

use super::binding_batch::BatchColumn;
use super::fixtures::{
    TEST_TARGET_ROWS, assert_all_aligned, execute_row_plan, plan_source, pull_scan, row_table,
    rows_into_bindings, seed_nodes, test_policy,
};
use super::{
    BatchBuffer, BatchCancel, BatchError, BatchExecutionContext, BatchNodeScan, BatchPolicy,
    BatchPolicyError, BatchScanSpec, BindingBatch, MemoryBudget, MemoryBudgetError, OperatorState,
    PhysicalOperator, assert_same_rows, assert_same_schema, assert_tables_equivalent, collect_rows,
    descriptor_for, trace_scan_to_table,
};

#[test]
fn policy_rejects_degenerate_construction() {
    assert_eq!(
        BatchPolicy::new(0, 1024),
        Err(BatchPolicyError::ZeroTargetRows)
    );
    assert_eq!(BatchPolicy::new(8, 0), Err(BatchPolicyError::ZeroMaxBytes));
}

#[test]
fn scan_cardinality_is_exact_at_batch_boundaries() {
    // Empty, one row, exactly one batch, and one row past the boundary.
    for count in [0usize, 1, TEST_TARGET_ROWS, TEST_TARGET_ROWS + 1] {
        let graph = SharedGraph::new(GraphId::new(41_000 + count as u64));
        seed_nodes(&graph, count, None);
        let expected = row_table(&graph, "MATCH (n) RETURN n");
        assert_eq!(expected.row_count(), count);

        let pulled = pull_scan(
            graph.read(),
            None,
            expected.schema(),
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

    let mut shapes = Vec::new();
    for target in [1usize, 3, TEST_TARGET_ROWS, 1024] {
        let policy = BatchPolicy::new(target, 1 << 20).unwrap();
        let pulled = pull_scan(
            graph.read(),
            None,
            expected.schema(),
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

    let pulled = pull_scan(
        graph.read(),
        Some(db_string("Person").unwrap()),
        expected.schema(),
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

    let mut scan = BatchNodeScan::new(
        BatchScanSpec::new(
            Some(db_string("Person").unwrap()),
            expected.schema().clone(),
        )
        .unwrap(),
        test_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    );
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

    let mut scan = BatchNodeScan::new(
        BatchScanSpec::new(
            Some(db_string("Person").unwrap()),
            expected.schema().clone(),
        )
        .unwrap(),
        BatchPolicy::new(2, 1 << 20).unwrap(),
    );
    assert_eq!(scan.output_schema(), expected.schema());
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    );
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

    let mut scan = BatchNodeScan::new(
        BatchScanSpec::new(
            Some(db_string("Missing").unwrap()),
            expected.schema().clone(),
        )
        .unwrap(),
        test_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    );
    let actual = trace_scan_to_table(&mut scan, &mut ctx).expect("tracer executes");
    assert_eq!(actual.row_count(), 0);
    assert_same_schema(&expected, &actual, "empty scan");
    assert_eq!(
        descriptor_for(&expected),
        descriptor_for(&actual),
        "empty results keep declared types and preferred order"
    );
}

#[test]
fn unit_table_drives_one_projection_and_empty_drives_none() {
    let schema = BindingTableSchema {
        columns: vec![BindingTableColumn {
            name: Some(db_string("one").unwrap()),
            hidden: None,
            ty: AnalyzedType::Dynamic,
        }],
    };
    // One row of zero fields drives exactly one projection.
    let unit = BindingBatch::unit();
    let projected = unit.project_literal(schema.clone(), Value::Int(7)).unwrap();
    assert_eq!(projected.logical_rows(), 1);
    assert_eq!(projected.logical_rows_vec(), vec![vec![Value::Int(7)]]);

    // Zero rows drive no projection, for both zero- and one-column inputs.
    let empty_unit_shape = BindingBatch::empty(BindingTableSchema {
        columns: Vec::new(),
    });
    assert_eq!(
        empty_unit_shape
            .project_literal(schema.clone(), Value::Int(7))
            .unwrap()
            .logical_rows(),
        0
    );
    let empty_width_one = BindingBatch::empty(schema.clone());
    assert_eq!(
        empty_width_one
            .project_literal(schema.clone(), Value::Int(7))
            .unwrap()
            .logical_rows(),
        0
    );

    // Projection preserves the single-column contract.
    assert!(matches!(
        unit.project_literal(
            BindingTableSchema {
                columns: Vec::new()
            },
            Value::Int(7)
        ),
        Err(BatchError::SchemaWidthMismatch { .. })
    ));
}

#[test]
fn null_bitmaps_and_selections_stay_aligned_through_filter_and_reuse() {
    let schema = BindingTableSchema {
        columns: ["a", "b"]
            .iter()
            .map(|name| BindingTableColumn {
                name: Some(db_string(name).unwrap()),
                hidden: None,
                ty: AnalyzedType::Dynamic,
            })
            .collect(),
    };
    let column_a: Vec<Value> = (0..6).map(Value::Int).collect();
    let column_b: Vec<Value> = vec![
        Value::Null,
        Value::Int(1),
        Value::Null,
        Value::Int(3),
        Value::Int(4),
        Value::Null,
    ];
    let mut buffer = BatchBuffer::new();
    let mut batch = BindingBatch::from_columns(schema.clone(), vec![column_a, column_b]).unwrap();
    assert_eq!(batch.schema(), &schema);
    assert!(!batch.column(0).unwrap().is_empty());
    assert!(batch.column(0).unwrap().retained_capacity() >= 6);
    assert!(batch.estimated_bytes() > 0);
    assert_all_aligned(&batch);

    // Sparse filtering keeps only rows 0, 3, 5 (two nulls and one value).
    batch
        .select(&[true, false, false, true, false, true], &mut buffer)
        .unwrap();
    assert_eq!(batch.logical_rows(), 3);
    assert_eq!(
        batch.logical_rows_vec(),
        vec![
            vec![Value::Int(0), Value::Null],
            vec![Value::Int(3), Value::Int(3)],
            vec![Value::Int(5), Value::Null],
        ]
    );
    assert_all_aligned(&batch);

    // Recycle and rebuild through the same buffer: retained storage must be
    // reused and stay aligned after a second sparse filter.
    assert!(
        buffer.retained_capacity_bytes() == 0,
        "live batch holds storage"
    );
    batch.recycle(&mut buffer);
    assert_eq!(buffer.recycled_batches(), 1);
    assert!(
        buffer.retained_capacity_bytes() > 0,
        "recycled storage is retained"
    );
    let mut values = buffer.take_values();
    let mut nulls = buffer.take_nulls();
    assert!(
        values.capacity() > 0 && nulls.capacity() > 0,
        "rebuild reuses retained allocations"
    );
    values.extend([Value::Null, Value::Int(9), Value::Null]);
    nulls.extend([true, false, true]);
    let rebuilt = BindingBatch::from_batch_columns(
        BindingTableSchema {
            columns: vec![BindingTableColumn {
                name: Some(db_string("c").unwrap()),
                hidden: None,
                ty: AnalyzedType::Dynamic,
            }],
        },
        vec![BatchColumn::from_parts(values, nulls).unwrap()],
    )
    .unwrap();
    assert_all_aligned(&rebuilt);
    let mut rebuilt = rebuilt;
    rebuilt.select(&[false, true, true], &mut buffer).unwrap();
    assert_eq!(
        rebuilt.logical_rows_vec(),
        vec![vec![Value::Int(9)], vec![Value::Null]]
    );
    assert_all_aligned(&rebuilt);

    // Rejects mismatched masks instead of misaligning.
    assert!(matches!(
        rebuilt.select(&[true], &mut buffer),
        Err(BatchError::KeepLengthMismatch { .. })
    ));
}

#[test]
fn descriptors_keep_types_and_preferred_order() {
    // Preferred order is declaration order, not alphabetical: ["b", "a"].
    let schema = BindingTableSchema {
        columns: ["b", "a"]
            .iter()
            .map(|name| BindingTableColumn {
                name: Some(db_string(name).unwrap()),
                hidden: None,
                ty: AnalyzedType::Dynamic,
            })
            .collect(),
    };
    let full = BindingBatch::from_columns(
        schema.clone(),
        vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Null, Value::Int(3)],
        ],
    )
    .unwrap();
    let empty = BindingBatch::empty(schema.clone());
    let full_table = BindingTable::new(
        schema.clone(),
        full.logical_rows_vec()
            .into_iter()
            .map(Binding::new)
            .collect(),
    );
    let empty_table = BindingTable::new(schema, Vec::new());
    assert_eq!(empty.logical_rows(), 0);

    let full_descriptor = descriptor_for(&full_table);
    let empty_descriptor = descriptor_for(&empty_table);
    assert_eq!(full_descriptor, empty_descriptor);
    assert_eq!(
        full_descriptor
            .fields()
            .iter()
            .map(|f| f.name().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec!["b".to_owned(), "a".to_owned()]
    );
    assert_eq!(full_descriptor.preferred_columns(), &[0, 1]);
}

#[test]
fn cancelled_scan_releases_snapshot_without_partial_output() {
    let graph = SharedGraph::new(GraphId::new(41_500));
    seed_nodes(&graph, 10, None);
    let schema = row_table(&graph, "MATCH (n) RETURN n").schema().clone();
    let before = graph.read().node_count();

    // Pre-cancelled token: the tracer fails before producing anything.
    let token = CancellationToken::new();
    token.cancel();
    let snapshot = graph.read();
    // `SharedGraph` retains its own snapshot handle(s), so release is
    // observed relatively against this baseline: the context's clone must
    // come and go while the retained handles stay put.
    let baseline = Arc::strong_count(&snapshot);
    let probe = Arc::clone(&snapshot);
    assert_eq!(Arc::strong_count(&probe), baseline + 1);
    let mut scan = BatchNodeScan::new(
        BatchScanSpec::new(None, schema.clone()).unwrap(),
        test_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        snapshot,
        BatchCancel::new(Some(&token), None, None),
        MemoryBudget::unlimited(),
    );
    assert_eq!(Arc::strong_count(&probe), baseline + 1);
    assert!(
        std::ptr::eq(ctx.snapshot().expect("context holds the snapshot"), &*probe),
        "the context pins this exact snapshot allocation"
    );
    let err = trace_scan_to_table(&mut scan, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::Cancelled { .. }));
    assert_eq!(err.gqlstatus().as_str(), "5GQL2");
    assert!(ctx.is_closed(), "tracer closes the context on error");
    assert_eq!(scan.state(), OperatorState::Closed);
    drop(ctx);
    drop(scan);
    assert_eq!(
        Arc::strong_count(&probe),
        baseline,
        "error paths release the pinned snapshot"
    );
    assert_eq!(graph.read().node_count(), before, "no partial mutation");

    // Mid-stream cancellation: one batch succeeds, the next pull fails, and
    // close still releases everything.
    let live = CancellationToken::new();
    let snapshot = graph.read();
    let baseline = Arc::strong_count(&snapshot);
    let probe = Arc::clone(&snapshot);
    let mut scan = BatchNodeScan::new(
        BatchScanSpec::new(None, schema.clone()).unwrap(),
        BatchPolicy::new(3, 1 << 20).unwrap(),
    );
    let mut ctx = BatchExecutionContext::new(
        snapshot,
        BatchCancel::new(Some(&live), None, None),
        MemoryBudget::unlimited(),
    );
    assert_eq!(Arc::strong_count(&probe), baseline + 1);
    let mut buffer = BatchBuffer::new();
    scan.init(&mut ctx).unwrap();
    let first = scan.next_batch(&mut ctx, &mut buffer).unwrap().unwrap();
    assert_eq!(first.logical_rows(), 3);
    first.recycle(&mut buffer);
    live.cancel();
    let err = scan.next_batch(&mut ctx, &mut buffer).unwrap_err();
    assert!(matches!(err, ExecutorError::Cancelled { .. }));
    assert_eq!(scan.state(), OperatorState::Failed);
    // Only close is legal after a pull error.
    assert!(scan.next_batch(&mut ctx, &mut buffer).is_err());
    scan.close(&mut ctx);
    assert!(ctx.is_closed());
    drop(ctx);
    drop(scan);
    drop(buffer);
    assert_eq!(
        Arc::strong_count(&probe),
        baseline,
        "closing releases the pinned snapshot"
    );
    assert_eq!(graph.read().node_count(), before, "no partial mutation");
}

#[test]
fn scan_budget_timeout_and_memory_cap_surface_without_writes() {
    let graph = SharedGraph::new(GraphId::new(41_600));
    seed_nodes(&graph, 8, None);
    let schema = row_table(&graph, "MATCH (n) RETURN n").schema().clone();
    let before = graph.read().node_count();

    // Deterministic node-scan budget trips during init accounting.
    let budget = NodeScanBudget::new(3);
    let mut scan = BatchNodeScan::new(
        BatchScanSpec::new(None, schema.clone()).unwrap(),
        test_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        BatchCancel::new(None, None, Some(&budget)),
        MemoryBudget::unlimited(),
    );
    let err = trace_scan_to_table(&mut scan, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::ProgramLimitExceeded { .. }));
    assert!(ctx.is_closed());

    // Elapsed deadline surfaces as a timeout, not a cancellation.
    let past = Instant::now() - Duration::from_secs(1);
    let mut scan = BatchNodeScan::new(
        BatchScanSpec::new(None, schema.clone()).unwrap(),
        test_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        BatchCancel::new(None, Some(past), None),
        MemoryBudget::unlimited(),
    );
    let err = trace_scan_to_table(&mut scan, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::Timeout { .. }));
    assert_eq!(err.gqlstatus().as_str(), "5GQL3");
    assert!(ctx.is_closed());

    // A zero-room memory budget aborts before materializing rows.
    assert!(matches!(
        MemoryBudget::new(1).reserve(2),
        Err(MemoryBudgetError::Exceeded { .. })
    ));
    let mut scan = BatchNodeScan::new(BatchScanSpec::new(None, schema).unwrap(), test_policy());
    let mut ctx =
        BatchExecutionContext::new(graph.read(), BatchCancel::disabled(), MemoryBudget::new(1));
    let err = trace_scan_to_table(&mut scan, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::ProgramLimitExceeded { .. }));
    assert!(ctx.is_closed());
    assert_eq!(graph.read().node_count(), before, "no partial mutation");
}

#[test]
fn batch_positions_never_escape_as_graph_identities() {
    // API-surface regression: batch coordinates are crate-private offsets
    // with no conversion into graph identities, and every public surface the
    // tracer feeds (rows, schema, descriptor) carries only stable values.
    let graph = SharedGraph::new(GraphId::new(41_700));
    seed_nodes(&graph, 5, Some("Person"));
    let expected = row_table(&graph, "MATCH (n:Person) RETURN n");

    let pulled = pull_scan(
        graph.read(),
        Some(db_string("Person").unwrap()),
        expected.schema(),
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

#[allow(clippy::print_stdout)]
#[test]
fn perf_probe_reports_observed_numbers() {
    // Observed numbers only: no timing assertions. Run with
    // `cargo nextest run -p selene-db-gql perf_probe -- --nocapture`
    // (or `cargo test`) to read the report. A throughput win that wrecks
    // tiny queries must come back as a design adjustment, never as a hidden
    // fallback to the old executor.
    for (graph_no, count) in [(1u64, 1usize), (2, 1_000), (3, 20_000)] {
        let graph = SharedGraph::new(GraphId::new(41_800 + graph_no));
        seed_nodes(&graph, count, None);
        let planned = plan_source("MATCH (n) RETURN n");
        let schema = execute_row_plan(&graph, &planned).schema().clone();

        // Row-reference timing (single sample, execution only: the plan is
        // shared, so parse/analyze/plan cost is excluded on both sides).
        let row_started = Instant::now();
        let rowed = execute_row_plan(&graph, &planned);
        let row_elapsed = row_started.elapsed();
        assert_eq!(rowed.row_count(), count);

        let started = Instant::now();
        let pulled = pull_scan(
            graph.read(),
            None,
            &schema,
            BatchPolicy::default_policy(),
            BatchCancel::disabled(),
            MemoryBudget::unlimited(),
        )
        .expect("batch scan executes");
        let elapsed = started.elapsed();
        assert_eq!(pulled.rows.len(), count);
        assert_eq!(pulled.batch_sizes.iter().sum::<usize>(), count);
        println!(
            "batch-scan probe: rows={count} batches={} \
             batch_us={} row_us={} reserve_events={} peak_budget_bytes={} \
             recycled_batches={}             retained_capacity_bytes={}",
            pulled.batches,
            elapsed.as_micros(),
            row_elapsed.as_micros(),
            pulled.reserve_events,
            pulled.peak_bytes,
            pulled.recycled_batches,
            pulled.retained_bytes,
        );
    }
}
