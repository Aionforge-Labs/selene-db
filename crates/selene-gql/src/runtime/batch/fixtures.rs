//! Shared fixtures for batch-substrate acceptance tests.
//!
//! Split from `tests.rs` to keep both files under the repository file-size
//! cap. Everything here serves the differential strategy: seed a real graph,
//! run the row executor as the oracle, and pull the batch scan manually with
//! full physical-shape and budget telemetry.

use std::sync::Arc;

use selene_core::{LabelSet, PropertyMap, Value, db_string};
use selene_graph::{SeleneGraph, SharedGraph};

use crate::{
    EmptyProcedureRegistry, analyze, parse, plan,
    plan::BindingTableSchema,
    runtime::{BindingTable, ExecutorError, TxContext},
};

use super::{
    BatchBuffer, BatchCancel, BatchExecutionContext, BatchNodeScan, BatchPolicy, BatchScanSpec,
    MemoryBudget, OperatorState, PhysicalOperator,
};

/// Row target for boundary-cardinality tests.
pub(super) const TEST_TARGET_ROWS: usize = 4;

/// Boundary-cardinality policy: tiny batches so every test crosses pull
/// boundaries deterministically.
pub(super) fn test_policy() -> BatchPolicy {
    BatchPolicy::new(TEST_TARGET_ROWS, 1 << 20).unwrap()
}

/// Insert `count` nodes, all carrying `label` when supplied.
pub(super) fn seed_nodes(graph: &SharedGraph, count: usize, label: Option<&str>) {
    let mut txn = graph.begin_write();
    let mut mutator = txn.mutator();
    for _ in 0..count {
        let labels = match label {
            Some(name) => LabelSet::single(db_string(name).unwrap()),
            None => LabelSet::new(),
        };
        mutator
            .create_node(labels, PropertyMap::default())
            .expect("fixture node inserts");
    }
    txn.commit().expect("fixture commits");
}

/// Run the existing row executor for `source`: the differential oracle.
pub(super) fn row_table(graph: &SharedGraph, source: &str) -> BindingTable {
    let planned = plan_source(source);
    execute_row_plan(graph, &planned)
}

/// Plan `source` once so probes can time row execution separately from
/// parse/analyze/plan.
pub(super) fn plan_source(source: &str) -> crate::ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

/// Execute an already-planned query through the row executor.
pub(super) fn execute_row_plan(
    graph: &SharedGraph,
    planned: &crate::ExecutionPlan,
) -> BindingTable {
    let mut ctx = TxContext::read_only(
        graph.read(),
        &planned.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );
    super::super::plan_runner::execute_plan(planned, &mut ctx).expect("row path executes")
}

/// Manually pulled scan output: materialized rows plus the physical shape
/// and budget counters that produced them.
pub(super) struct PulledScan {
    /// Materialized logical rows in pull order.
    pub(super) rows: Vec<Vec<Value>>,
    /// Logical rows per produced batch.
    pub(super) batch_sizes: Vec<usize>,
    /// Total batches produced.
    pub(super) batches: u64,
    /// Successful budget reservations observed.
    pub(super) reserve_events: u64,
    /// High-water reserved estimated bytes.
    pub(super) peak_bytes: usize,
    /// Batches recycled through the buffer.
    pub(super) recycled_batches: u64,
    /// Retained idle buffer bytes after the run.
    pub(super) retained_bytes: usize,
}

/// Pull a batch scan manually, reporting rows, per-batch sizes, and budget
/// counters. The context is closed before returning.
pub(super) fn pull_scan(
    snapshot: Arc<SeleneGraph>,
    label: Option<selene_core::DbString>,
    schema: &BindingTableSchema,
    policy: BatchPolicy,
    cancel: BatchCancel<'_>,
    budget: MemoryBudget,
) -> Result<PulledScan, ExecutorError> {
    let mut scan = BatchNodeScan::new(BatchScanSpec::new(label, schema.clone())?, policy);
    assert_eq!(scan.state(), OperatorState::Created);
    let mut ctx = BatchExecutionContext::new(snapshot, cancel, budget);
    scan.init(&mut ctx)?;
    assert_eq!(scan.state(), OperatorState::Open);
    let mut buffer = BatchBuffer::new();
    let mut rows = Vec::new();
    let mut batch_sizes = Vec::new();
    while let Some(batch) = scan.next_batch(&mut ctx, &mut buffer)? {
        batch_sizes.push(batch.logical_rows());
        rows.extend(batch.logical_rows_vec());
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    assert_eq!(scan.state(), OperatorState::Exhausted);
    let pulled = PulledScan {
        rows,
        batch_sizes,
        batches: scan.batches_produced(),
        reserve_events: ctx.budget().reserve_events(),
        peak_bytes: ctx.budget().peak_bytes(),
        recycled_batches: buffer.recycled_batches(),
        retained_bytes: buffer.retained_capacity_bytes(),
    };
    scan.close(&mut ctx);
    assert_eq!(scan.state(), OperatorState::Closed);
    assert!(ctx.is_closed());
    Ok(pulled)
}

/// Convert owned rows into executor row storage.
pub(super) fn rows_into_bindings(rows: &[Vec<Value>]) -> Vec<crate::runtime::Binding> {
    rows.iter()
        .map(|row| crate::runtime::Binding::new(row.clone()))
        .collect()
}

/// Assert null bitmaps, selection bounds, and logical counts agree.
pub(super) fn assert_all_aligned(batch: &super::BindingBatch) {
    for index in 0..batch.width() {
        let column = batch.column(index).unwrap();
        assert_eq!(column.values().len(), column.nulls().len());
        for (value, null) in column.values().iter().zip(column.nulls()) {
            assert_eq!(*null, *value == Value::Null, "bitmap drifted from values");
        }
    }
    if let Some(selection) = batch.selection() {
        for position in selection {
            assert!(
                position.index() < batch.physical_len(),
                "selection escaped physical storage"
            );
        }
        assert_eq!(selection.len(), batch.logical_rows());
    }
}
