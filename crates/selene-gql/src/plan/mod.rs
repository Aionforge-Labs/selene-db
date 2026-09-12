//! Query planner entry points and execution-plan IR.
//!
//! The planner lowers a [`crate::AnalyzedStatement`] into a literal
//! [`ExecutionPlan`] covering reads, mutations, DDL, CALL, and transaction
//! control, with binding-table schemas attached wherever row shape changes.
//! Optimizer rewrites are explicit: callers that want canonicalization or
//! access-path selection call [`optimize()`] after [`plan()`]. This layer relies
//! on analyzer binding/type/write-set invariants and defers provider fanout,
//! three-valued logic evaluation, and transaction effects to the runtime. See
//! Spec 08 §6-§8 and Spec 13.

mod error;
mod ir;
pub mod logical;
mod lowering;
pub mod optimize;

pub use error::PlannerError;
pub use ir::{
    Aggregate, AggregateArg, BindingDef, BindingElement, BindingTableColumn, BindingTableSchema,
    BuildSide, CatalogOp, DeleteTargetPlan, EdgeMatch, ExecutionPlan, FilterPredicate,
    FilterPredicateKind, HiddenBindingId, HopContributor, ImplDefinedCaps, IndexKey,
    InsertEndpointRef, InsertSiteId, JoinTree, LimitAmount, MutationOp, NodeIdOrdering,
    NodeOrEdgeScan, OrderAccess, OrderKey, OuterBindingRef, PathContributor, PathPlan, PatternPlan,
    PipelineOp, PipelineOpId, PlannedCall, PlannedSubquery, PlannedTableSubquery,
    PlannedTableSubqueryYield, PlannedTypePropertyConstraint, PlannedTypePropertyDef,
    PlannedYieldItem, ProjectExpr, PropertyInit, RepeatEdgeMatch, ScanAccess, ScanKind, SessionOp,
    SubqueryBody, SubqueryKind, SubqueryRegistry, TailBinding, TxOp, TypedIndexBounds, YieldKind,
};
pub use logical::{
    EffectSummary, LogicalCallDescriptor, LogicalEffect, LogicalMultiplicity,
    LogicalMutationDescriptor, LogicalOp, LogicalOrdering, LogicalPageAmount, LogicalPlan,
    LogicalScanDescriptor, check_gp18, classify_analyzed, classify_plan,
    explain as explain_logical, lower_logical, measure_lowering_cost, verify_plan_effects,
};
pub use lowering::{plan, plan_with_caps};
pub use optimize::{
    CompositeIndexHandle, EmptyIndexCatalog, IndexCatalog, IndexHandle, IndexKind, IndexTarget,
    LiveIndexCatalog, OptimizeContext, Rule, Transformed, TypedIndexLookup, optimize,
};
