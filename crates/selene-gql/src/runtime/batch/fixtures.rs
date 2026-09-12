//! Shared fixtures for batch-substrate acceptance tests.
//!
//! Split from `tests.rs` to keep both files under the repository file-size
//! cap. Everything here serves the differential strategy: seed a real graph,
//! run the row executor as the oracle, and pull batch operators manually with
//! full physical-shape and budget telemetry.
//!
//! The row oracle ([`row_table`]/[`execute_row_only`]) replicates the plan
//! runner's row logic directly instead of calling it, because the plan runner
//! is the batch production seam since F04-PR02: routing the oracle through it
//! would compare the batch path against itself. [`production_table`] goes
//! through the plan runner on purpose, proving what live queries execute.

use std::sync::Arc;

use selene_core::{GraphId, LabelSet, PropertyMap, Value, db_string};
use selene_graph::{SeleneGraph, SharedGraph, TypedIndexKind};

use crate::{
    Aggregate, AggregateArg, EmptyProcedureRegistry, ExecutionPlan, NullsPolicy, OrderDirection,
    OrderKey, ProjectExpr, SourceSpan, ValueExpr, analyze, parse, plan,
    plan::{BindingTableColumn, BindingTableSchema},
    runtime::{BindingTable, EvalCtx, ExecutorError, TxContext},
};

use super::scan::BatchScan;
use super::{
    BatchBuffer, BatchCancel, BatchExecutionContext, BatchPolicy, MemoryBudget, OperatorState,
    PhysicalOperator,
};

/// Row target for boundary-cardinality tests.
pub(super) const TEST_TARGET_ROWS: usize = 4;

/// Kernel-only execution context over a detached snapshot.
///
/// Hand-built rows never touch graph data; the retained handle satisfies
/// the pin requirement. Pair with a bounded [`MemoryBudget`] for
/// resource-failure tests or `unlimited` for success paths.
pub(super) fn kernel_ctx(budget: MemoryBudget) -> BatchExecutionContext<'static> {
    BatchExecutionContext::new(
        Arc::new(SeleneGraph::new(GraphId::new(43_001))),
        BatchCancel::disabled(),
        budget,
    )
}

/// One named dynamic column for hand-built kernel tables.
pub(super) fn kernel_column(name: &str) -> BindingTableColumn {
    BindingTableColumn {
        name: Some(db_string(name).unwrap()),
        hidden: None,
        ty: crate::AnalyzedType::Dynamic,
    }
}

/// Two-column `(k, v)` schema for hand-built join kernel tables.
pub(super) fn pair_schema() -> BindingTableSchema {
    BindingTableSchema {
        columns: vec![kernel_column("k"), kernel_column("v")],
    }
}

/// One hand-built `(key, value)` kernel row.
pub(super) fn pair(key: Value, value: Value) -> crate::runtime::Binding {
    crate::runtime::Binding::new([key, value])
}

/// Single-column schema over `k` for hand-built sort keys.
pub(super) fn single_schema() -> BindingTableSchema {
    BindingTableSchema {
        columns: vec![kernel_column("k")],
    }
}

/// One single-column kernel row.
pub(super) fn cell(value: Value) -> crate::runtime::Binding {
    crate::runtime::Binding::new([value])
}

/// Assert a kernel table carries exactly `expected` rows in order.
///
/// Shared by the grouping/sorting/dedup kernel tests so each file keeps
/// one comparison shape; `what` names the calling context.
pub(super) fn assert_kernel_rows(table: &BindingTable, expected: &[Vec<Value>], what: &str) {
    let actual = table
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "{what}: rows diverged (actual {actual:?})"
    );
}

/// Variable reference to a kernel-schema column.
pub(super) fn kernel_var(name: &str) -> ValueExpr {
    ValueExpr::Variable {
        name: db_string(name).unwrap(),
        span: SourceSpan::default(),
    }
}

/// Grouping key over one kernel-schema column.
pub(super) fn kernel_key(name: &str, id: u32) -> ProjectExpr {
    ProjectExpr {
        expr: kernel_var(name),
        expr_id: crate::analyze::ExprId::new(id),
        ty: crate::analyze::AnalyzedType::Dynamic,
        declared_type: None,
        alias: None,
        binding_refs: Vec::new(),
        span: SourceSpan::default(),
    }
}

/// Aggregate descriptor over one kernel-schema column (or none for `*`).
pub(super) fn kernel_agg(
    function: &str,
    arg: Option<&str>,
    star: bool,
    distinct: bool,
    id: u32,
) -> Aggregate {
    Aggregate {
        aggregate_id: crate::analyze::ExprId::new(id),
        output_name: db_string(&format!("{function}_{id}")).unwrap(),
        function: db_string(function).unwrap(),
        args: arg
            .map(|name| AggregateArg {
                expr: kernel_var(name),
                expr_id: crate::analyze::ExprId::new(id + 100),
                ty: crate::analyze::AnalyzedType::Dynamic,
            })
            .into_iter()
            .collect(),
        star,
        distinct,
        ty: crate::analyze::AnalyzedType::Dynamic,
        span: SourceSpan::default(),
    }
}

/// Sort key over one kernel-schema column.
pub(super) fn kernel_order_key(
    name: &str,
    id: u32,
    direction: OrderDirection,
    nulls: Option<NullsPolicy>,
) -> OrderKey {
    OrderKey {
        expr: kernel_var(name),
        expr_id: crate::analyze::ExprId::new(id),
        ty: crate::analyze::AnalyzedType::Dynamic,
        direction,
        nulls,
        binding_refs: Vec::new(),
        access: None,
        span: SourceSpan::default(),
    }
}

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

fn props<const N: usize>(pairs: [(selene_core::DbString, Value); N]) -> PropertyMap {
    PropertyMap::from_pairs(pairs).expect("test properties fit caps")
}

/// People plus robots with integer ages and names, plus a KNOWS chain.
pub(super) fn person_graph() -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(42_001));
    let person = db_string("Person").unwrap();
    let robot = db_string("Robot").unwrap();
    let knows = db_string("KNOWS").unwrap();
    let age = db_string("age").unwrap();
    let name = db_string("name").unwrap();
    let people = [
        ("Alice", 21),
        ("Bob", 22),
        ("Cara", 23),
        ("Dan", 24),
        ("Erin", 25),
        ("Fay", 26),
        ("Gus", 27),
        ("Hal", 28),
    ];
    let mut ids = Vec::new();
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        for (who, years) in people {
            ids.push(
                mutator
                    .create_node(
                        LabelSet::single(person.clone()),
                        props([
                            (name.clone(), Value::String(db_string(who).unwrap())),
                            (age.clone(), Value::Int(years)),
                        ]),
                    )
                    .expect("person inserts"),
            );
        }
        for bot in ["R2", "R3"] {
            mutator
                .create_node(
                    LabelSet::single(robot.clone()),
                    props([(name.clone(), Value::String(db_string(bot).unwrap()))]),
                )
                .expect("robot inserts");
        }
        mutator
            .create_edge(knows.clone(), ids[0], ids[1], PropertyMap::default())
            .expect("edge inserts");
        mutator
            .create_edge(knows, ids[1], ids[2], PropertyMap::default())
            .expect("edge inserts");
        txn.commit().expect("fixture commits");
    }
    graph
}

/// People with null, missing, and wrong-type ages for three-valued-logic agreement.
pub(super) fn oddball_graph() -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(42_002));
    let person = db_string("Person").unwrap();
    let age = db_string("age").unwrap();
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(person.clone()),
                props([(age.clone(), Value::Int(40))]),
            )
            .expect("int age inserts");
        mutator
            .create_node(LabelSet::single(person.clone()), PropertyMap::default())
            .expect("missing age inserts");
        mutator
            .create_node(
                LabelSet::single(person.clone()),
                props([(age.clone(), Value::String(db_string("old").unwrap()))]),
            )
            .expect("string age inserts");
        txn.commit().expect("fixture commits");
    }
    graph
}

/// One hub with many spokes (plus a loop and a parallel duplicate) for
/// multiplicity and limit-after-expansion agreement.
pub(super) fn hub_graph() -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(42_003));
    let hub = db_string("Hub").unwrap();
    let spoke = db_string("Spoke").unwrap();
    let knows = db_string("KNOWS").unwrap();
    let score = db_string("score").unwrap();
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let hub = mutator
            .create_node(LabelSet::single(hub), PropertyMap::default())
            .expect("hub inserts");
        let mut spokes = Vec::new();
        for _ in 0..12 {
            spokes.push(
                mutator
                    .create_node(LabelSet::single(spoke.clone()), PropertyMap::default())
                    .expect("spoke inserts"),
            );
        }
        for (index, spoke) in spokes.iter().enumerate() {
            mutator
                .create_edge(
                    knows.clone(),
                    hub,
                    *spoke,
                    props([(score.clone(), Value::Int(index as i64))]),
                )
                .expect("spoke edge inserts");
        }
        // Parallel duplicate and a directed loop: multiplicity must survive.
        mutator
            .create_edge(
                knows.clone(),
                hub,
                spokes[0],
                props([(score.clone(), Value::Int(100))]),
            )
            .expect("parallel edge inserts");
        mutator
            .create_edge(knows, hub, hub, props([(score.clone(), Value::Int(101))]))
            .expect("loop edge inserts");
        txn.commit().expect("fixture commits");
    }
    graph
        .create_edge_property_index(
            db_string("KNOWS").unwrap(),
            db_string("score").unwrap(),
            TypedIndexKind::I64,
        )
        .expect("edge index builds");
    graph
}

/// People with a real `Person.age` index for indexed-access agreement.
pub(super) fn indexed_person_graph() -> SharedGraph {
    let graph = person_graph();
    graph
        .create_property_index(
            db_string("Person").unwrap(),
            db_string("age").unwrap(),
            TypedIndexKind::I64,
        )
        .expect("age index builds");
    graph
}

/// Run the row executor for `source`: the differential oracle.
///
/// This never touches the batch driver (see [`execute_row_only`]), so
/// batch-vs-row comparisons stay honest after the production wiring.
pub(super) fn row_table(graph: &SharedGraph, source: &str) -> BindingTable {
    let planned = plan_source(source);
    let mut ctx = TxContext::read_only(
        graph.read(),
        &planned.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    )
    .with_plan_metadata(&planned.expr_ids, &planned.subqueries);
    execute_row_only(&planned, &mut ctx).expect("row path executes")
}

/// Run `source` through the production plan runner (batch-routed).
///
/// Comparing this against [`row_table`] proves what live queries execute.
pub(super) fn production_table(graph: &SharedGraph, source: &str) -> BindingTable {
    let planned = plan_source(source);
    let mut ctx = TxContext::read_only(
        graph.read(),
        &planned.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    )
    .with_plan_metadata(&planned.expr_ids, &planned.subqueries);
    super::super::plan_runner::execute_plan(&planned, &mut ctx).expect("production path executes")
}

/// Plan `source` once so probes can time row execution separately from
/// parse/analyze/plan.
pub(super) fn plan_source(source: &str) -> crate::ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

/// Plan `source` and optimize it against the graph's live indexes.
///
/// The [`LiveIndexCatalog`](crate::LiveIndexCatalog) pins the same snapshot
/// the test executes against, so optimizer index selection agrees with the
/// executed snapshot by construction. In-crate tests use this instead of the
/// integration-test `MockIndexCatalog`, which cannot cross the unit-test
/// crate boundary (the `cfg(test)` build is a distinct crate instance from
/// the dependency build).
pub(super) fn optimized_plan(source: &str, graph: &SharedGraph) -> crate::ExecutionPlan {
    let planned = plan_source(source);
    let catalog = crate::LiveIndexCatalog::new(graph.read());
    let ctx = crate::OptimizeContext::default().with_index_catalog(&catalog);
    crate::optimize(planned, &ctx)
}

/// Execute the batch prefix of an already-planned query with `policy`.
///
/// Returns `None` when the driver declines the shape (the test then asserts
/// the decline was expected). Fully-batch plans yield `suffix_from ==`
/// pipeline length; anything earlier means a row suffix resumes.
pub(super) fn batch_prefix_with_policy(
    graph: &SharedGraph,
    planned: &ExecutionPlan,
    policy: BatchPolicy,
) -> Result<Option<super::query::PrefixOutcome>, ExecutorError> {
    let ctx = TxContext::read_only(
        graph.read(),
        &planned.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );
    super::query::execute_with_test_policy(planned, &ctx, policy)
}

/// Execute an already-planned query through the row executor only.
///
/// This replicates the plan runner's row logic without its batch seam: the
/// pattern runs with no pushed-down row limit (final results are identical;
/// only early termination differs) and the pipeline runs through the row
/// dispatcher. Any divergence from production routing is a batch bug.
pub(super) fn execute_row_only(
    planned: &ExecutionPlan,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let table = match planned.pattern_plan.as_ref() {
        Some(pattern_plan) => {
            let eval_ctx = EvalCtx {
                tx: ctx,
                expr_ids: &planned.expr_ids,
                subqueries: &planned.subqueries,
            };
            let schema = super::super::pattern::schema_for_pattern(pattern_plan);
            super::super::pattern::execute_pattern_with_seed_schema_and_limit(
                pattern_plan,
                None,
                schema,
                &eval_ctx,
                None,
            )?
        }
        None => super::super::plan_runner::seed_table(),
    };
    super::super::pipeline::execute_pipeline_with_plan(
        planned.pipeline.as_slice(),
        table,
        ctx,
        &planned.expr_ids,
        &planned.subqueries,
    )
}

/// Manually pulled scan output: materialized rows plus the physical shape
/// that produced them.
pub(super) struct PulledScan {
    /// Materialized logical rows in pull order.
    pub(super) rows: Vec<Vec<Value>>,
    /// Logical rows per produced batch.
    pub(super) batch_sizes: Vec<usize>,
    /// Total batches produced.
    pub(super) batches: u64,
    /// Batches recycled through the buffer.
    pub(super) recycled_batches: u64,
}

/// Pull one batch scan manually, reporting rows, per-batch sizes, and budget
/// counters. The context is closed before returning.
#[allow(clippy::too_many_arguments)]
pub(super) fn pull_scan(
    snapshot: Arc<SeleneGraph>,
    scan: &crate::NodeOrEdgeScan,
    pattern: &crate::PatternPlan,
    schema: &BindingTableSchema,
    eval: EvalCtx<'_, '_, '_, '_>,
    policy: BatchPolicy,
    cancel: BatchCancel<'_>,
    budget: MemoryBudget,
) -> Result<PulledScan, ExecutorError> {
    let mut scan = BatchScan::new(scan, pattern, schema.clone(), eval, policy);
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
        recycled_batches: buffer.recycled_batches(),
    };
    scan.close(&mut ctx);
    assert_eq!(scan.state(), OperatorState::Closed);
    assert!(ctx.is_closed());
    Ok(pulled)
}

/// Borrow the single scan and its pattern from a single-scan test plan.
pub(super) fn scan_parts(plan: &ExecutionPlan) -> (&crate::NodeOrEdgeScan, &crate::PatternPlan) {
    let pattern = plan.pattern_plan.as_ref().expect("test plan has a pattern");
    let crate::JoinTree::Scan(scan) = &pattern.join_tree else {
        panic!("test plan is a single scan");
    };
    (scan, pattern)
}

/// Build an evaluation context borrowing a read-only transaction context.
///
/// Lifetimes stay decoupled (context borrow vs plan borrow) so tests can
/// drop the context while reusing the plan: only the operator holding the
/// context borrow keeps it alive.
pub(super) fn eval_for<'a, 't, 'p>(
    tx: &'a TxContext<'t, 't>,
    plan: &'p ExecutionPlan,
) -> EvalCtx<'a, 't, 't, 'p> {
    EvalCtx {
        tx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    }
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
