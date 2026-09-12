//! Batch substrate acceptance regressions (F04-PR01, carried forward).
//!
//! Substrate-contract tests: policy construction, the zero-column unit
//! table, null/selection alignment through filtering and buffer reuse,
//! declared types and preferred order, and cancellation/error release
//! without partial output. Scan-behavior tests live in [`super::scan_tests`];
//! F04-PR02 acceptance differentials (indexed access, filters, paging,
//! expansion, mixed edges, staleness) live in [`super::differentials`].
//! Every differential compares against the row executor on the same pinned
//! snapshot; the row path is the oracle and the batch path must match it.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use selene_core::{CancellationToken, GraphId, NodeScanBudget, Value, db_string};
use selene_graph::SharedGraph;

use crate::{
    AnalyzedType,
    plan::{BindingTableColumn, BindingTableSchema},
    runtime::{Binding, BindingTable, ExecutorError, TxContext},
};

use super::binding_batch::BatchColumn;
use super::fixtures::{
    assert_all_aligned, eval_for, plan_source, row_table, scan_parts, seed_nodes, test_policy,
};
use super::scan::BatchScan;
use super::{
    BatchBuffer, BatchCancel, BatchError, BatchExecutionContext, BatchPolicy, BatchPolicyError,
    BindingBatch, MemoryBudget, MemoryBudgetError, OperatorState, PhysicalOperator, descriptor_for,
    trace_scan_to_table,
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
    let plan = plan_source("MATCH (n) RETURN n");
    let schema = row_table(&graph, "MATCH (n) RETURN n").schema().clone();
    let before = graph.read().node_count();
    let (scan_ir, pattern) = scan_parts(&plan);

    // Pre-cancelled token: the tracer fails before producing anything.
    let token = CancellationToken::new();
    token.cancel();
    // `SharedGraph` retains its own snapshot handle(s), so release is
    // observed relatively against this baseline, which already includes the
    // evaluation context's clone: only the execution context's move must
    // come and go while the retained handles stay put.
    let snapshot = graph.read();
    let tx = TxContext::read_only(
        snapshot.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let probe = Arc::clone(&snapshot);
    let baseline = Arc::strong_count(&snapshot);
    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        schema.clone(),
        eval_for(&tx, &plan),
        test_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        snapshot,
        BatchCancel::new(Some(&token), None, None),
        MemoryBudget::unlimited(),
    );
    assert_eq!(
        Arc::strong_count(&probe),
        baseline,
        "moving the snapshot into the execution context clones nothing"
    );
    assert!(
        std::ptr::eq(ctx.snapshot().expect("context holds the snapshot"), &*probe),
        "the context pins this exact snapshot allocation"
    );
    let err = trace_scan_to_table(&mut scan, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::Cancelled { .. }));
    assert_eq!(err.gqlstatus().as_str(), "5GQL2");
    assert!(ctx.is_closed(), "tracer closes the context on error");
    assert_eq!(scan.state(), OperatorState::Closed);
    drop(scan);
    drop(tx);
    drop(ctx);
    assert_eq!(
        Arc::strong_count(&probe),
        baseline - 2,
        "error paths release the execution move and the evaluation clone"
    );
    assert_eq!(graph.read().node_count(), before, "no partial mutation");

    // Mid-stream cancellation: one batch succeeds, the next pull fails, and
    // close still releases everything.
    let live = CancellationToken::new();
    let snapshot = graph.read();
    let tx = TxContext::read_only(
        snapshot.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let probe = Arc::clone(&snapshot);
    let baseline = Arc::strong_count(&snapshot);
    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        schema.clone(),
        eval_for(&tx, &plan),
        BatchPolicy::new(3, 1 << 20).unwrap(),
    );
    let mut ctx = BatchExecutionContext::new(
        snapshot,
        BatchCancel::new(Some(&live), None, None),
        MemoryBudget::unlimited(),
    );
    assert_eq!(Arc::strong_count(&probe), baseline);
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
    drop(scan);
    drop(tx);
    drop(ctx);
    drop(buffer);
    assert_eq!(
        Arc::strong_count(&probe),
        baseline - 2,
        "closing releases the execution move and the evaluation clone"
    );
    assert_eq!(graph.read().node_count(), before, "no partial mutation");
}

#[test]
fn scan_budget_timeout_and_memory_cap_surface_without_writes() {
    let graph = SharedGraph::new(GraphId::new(41_600));
    seed_nodes(&graph, 8, None);
    let plan = plan_source("MATCH (n) RETURN n");
    let schema = row_table(&graph, "MATCH (n) RETURN n").schema().clone();
    let before = graph.read().node_count();
    let (scan_ir, pattern) = scan_parts(&plan);

    // Deterministic node-scan budget trips during init accounting.
    let budget = NodeScanBudget::new(3);
    let snapshot = graph.read();
    let tx = TxContext::read_only(
        snapshot.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        schema.clone(),
        eval_for(&tx, &plan),
        test_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        snapshot,
        BatchCancel::new(None, None, Some(&budget)),
        MemoryBudget::unlimited(),
    );
    let err = trace_scan_to_table(&mut scan, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::ProgramLimitExceeded { .. }));
    assert!(ctx.is_closed());
    drop(scan);
    drop(tx);

    // Elapsed deadline surfaces as a timeout, not a cancellation.
    let past = Instant::now() - Duration::from_secs(1);
    let snapshot = graph.read();
    let tx = TxContext::read_only(
        snapshot.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        schema.clone(),
        eval_for(&tx, &plan),
        test_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        snapshot,
        BatchCancel::new(None, Some(past), None),
        MemoryBudget::unlimited(),
    );
    let err = trace_scan_to_table(&mut scan, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::Timeout { .. }));
    assert_eq!(err.gqlstatus().as_str(), "5GQL3");
    assert!(ctx.is_closed());
    drop(scan);
    drop(tx);

    // A zero-room memory budget aborts before materializing rows.
    assert!(matches!(
        MemoryBudget::new(1).reserve(2),
        Err(MemoryBudgetError::Exceeded { .. })
    ));
    let snapshot = graph.read();
    let tx = TxContext::read_only(
        snapshot.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        schema,
        eval_for(&tx, &plan),
        test_policy(),
    );
    let mut ctx =
        BatchExecutionContext::new(snapshot, BatchCancel::disabled(), MemoryBudget::new(1));
    let err = trace_scan_to_table(&mut scan, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::ProgramLimitExceeded { .. }));
    assert!(ctx.is_closed());
    drop(scan);
    drop(tx);
    assert_eq!(graph.read().node_count(), before, "no partial mutation");
}
