//! ISO/IEC 39075:2024 GQL parser, AST, and Flagger for selene-db.
//!
//! See Spec 07 for the parser and Flagger design contract.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod analyze;
pub mod ast;
pub mod catalog_command;
pub mod diagnostic;
pub mod error;
mod flagger;
pub mod parser;
pub mod plan;
pub mod procedure_registry;
pub mod runtime;
mod temporal_parse;
mod type_adapter;

pub use type_adapter::{lower_value_type, normalize_value_type};

pub use crate::analyze::{
    AnalysisError, AnalyzedStatement, AnalyzedType, BindingDecl, BindingDeclKind, BindingId,
    BindingScope, BindingScopeTree, BindingUse, BindingUseKind, ConditionClause, ElementKind,
    ExpectedType, ExprId, ExprIdLookup, ExprTypeTable, InvalidLabelForm, MutationWriteSet,
    ParameterUse, ScopeId, ScopeKind, Side, StatementCategory, TypeMismatchContext, WriteKind,
    WriteSetEntry, analyze,
};
pub use crate::ast::{
    call::{InlineProcedureCall, ProcedureCall, YieldColumn, YieldItem},
    catalog_ref::{CatalogObjectReference, CatalogPathSegment, IdentifierForm},
    ddl::{
        CatalogGraphTypeDefinition, CatalogNodeTypeDefinition, DdlStatement, DropBehavior,
        EdgeEndpointSpec, KeyLabelSet, TypePropertyConstraint, TypePropertyDef, ValidationMode,
    },
    expr::{
        BinaryOp, DecimalLiteralKind, ExistsBody, FloatLiteralKind, IntegerLiteralKind,
        IsCheckKind, Literal, NormalForm, TemporalDurationQualifier, TrimSpec, TruthValue, UnaryOp,
        ValueExpr,
    },
    format::format_procedure_call,
    mutation::{
        DeleteMode, DeleteStatement, InsertStatement, MutationPipeline, MutationStatement,
        MutationTerminator, RemoveItem, SetItem,
    },
    pattern::{
        EdgeDirection, EdgePattern, GraphPattern, LabelExpr, MatchClause, MatchMode, NodePattern,
        PathMode, PathSelector, PatternElement, Quantifier,
    },
    span::SourceSpan,
    statement::{
        ForStatement, LetBinding, LimitValue, NullsPolicy, OrderDirection, OrderTerm,
        PipelineStatement, QueryPipeline, ReturnClause, ReturnItem, RowExpansionPosition,
        RowExpansionPositionKind, SessionResetTarget, SessionSetGraphTarget, SetOp, Statement,
        WithClause,
    },
    types::{BindingTableType, GqlType, RecordType},
    util::{EmptyVecError, NonEmpty, Vec2OrMore},
    working_scope::{GraphExpression, WorkingScopeClause},
};
pub use crate::catalog_command::DatabaseCatalogCommand;
pub use crate::diagnostic::DiagnosticReport;
pub use crate::error::{GqlStatus, ParserError};
pub use crate::flagger::{FeatureUse, feature_walk};
pub use crate::parser::{is_parameter_name, parse, parse_many, parse_with_source};
pub use crate::plan::{
    Aggregate, AggregateArg, AutomatonStats, BindingDef, BindingElement, BindingExposure,
    BindingTableColumn, BindingTableSchema, BuildSide, CatalogOp, CompositeIndexHandle,
    DeleteTargetPlan, EdgeMatch, EdgeQuantifierKind, EdgeTest, EffectSummary, EmptyIndexCatalog,
    ExecutionPlan, FilterPredicate, FilterPredicateKind, HiddenBindingId, ImplDefinedCaps,
    IndexCatalog, IndexHandle, IndexKey, IndexKind, IndexTarget, InsertEndpointRef, InsertSiteId,
    JoinTree, LimitAmount, LiveIndexCatalog, LogicalAggregate, LogicalCallDescriptor,
    LogicalCatalogKind, LogicalControlKind, LogicalEffect, LogicalMultiplicity,
    LogicalMutationDescriptor, LogicalOp, LogicalOrderKey, LogicalOrdering, LogicalPageAmount,
    LogicalPlan, LogicalScanDescriptor, LoweredPathSet, MutationOp, NodeIdOrdering, NodeOrEdgeScan,
    NodeTest, OptimizeContext, OrderAccess, OrderKey, OrientationAcceptance, OuterBindingRef,
    PATH_AUTOMATA_CONTRACT_VERSION, PathAutomaton, PathConditions, PathFeatureInventory,
    PathLoweringLimits, PathModeScope, PathProgram, PathSemanticElement, PathSemanticPattern,
    PathState, PathStateId, PathTransition, PathTransitionId, PatternPlan, PipelineOp,
    PipelineOpId, PlannedCall, PlannedSubquery, PlannedTableSubquery, PlannedTableSubqueryYield,
    PlannedTypePropertyConstraint, PlannedTypePropertyDef, PlannedYieldItem, PlannerError,
    ProjectExpr, PropertyInit, Rule, ScanAccess, ScanKind, SelectorScope, SessionOp, SubqueryBody,
    SubqueryKind, SubqueryRegistry, TemporaryBinding, Transformed, TransitionKind, TxOp,
    TypedIndexBounds, TypedIndexLookup, YieldKind, acceptance_for, check_gp18, classify_analyzed,
    classify_plan, explain_automaton, explain_logical, explain_set, is_selective_selector,
    lower_logical, lower_path_automata, lower_path_automata_with_defaults, measure_lowering_cost,
    measure_path_lowering, optimize, plan, supported_path_inventory, verify_plan_effects,
};
pub use crate::procedure_registry::{
    EmptyProcedureRegistry, ProcedureArity, ProcedureDefaultValue, ProcedureError, ProcedureHandle,
    ProcedureMetadata, ProcedureMutability, ProcedureOutputColumn, ProcedureOutputSchema,
    ProcedureParameter, ProcedureRegistry, ProcedureResult, ProcedureSignature, ProcedureTier,
    Value,
};
pub use crate::runtime::{
    AdaptiveOptimizer, Binding, BindingTable, BindingTableAllocationError, BindingTableDescriptor,
    BindingTableField, BindingTableLookupError, BindingTableRegistry, BuiltinProcedureRegistry,
    CallPlanCache, CallPlanCacheStats, CallPlanKey, CatalogSessionOutput, DataExceptionSubclass,
    DiagnosticBundle, ExecutionContext, ExecutionContextError, ExecutionFrame, ExecutionOutcome,
    ExecutionStack, ExecutorError, ExecutorWarning, GqlStatusObject, GraphContext,
    MaintenanceContext, MutationContext, PlanCache, PlanCacheStats, PreparedCatalogMutationOutput,
    PreparedCatalogPlan, PreparedCatalogRequest, PreparedCatalogRequestKind,
    PreparedSessionControl, PreparedTransactionControl, ProcedureContext, Record,
    RequestExecutionInput, RequestParameter, RequestRuntimeHandle, RollbackOutcome, Session,
    SessionParameterValue, SharedPlanCache, SharedPlanCacheStats, StatementOutput,
    TransactionOutcome, TxContext, WarningSink, WriteOutcome, execute_pattern, execute_pipeline,
    execute_statement, parse_graph_independent_session_control, parse_session_close,
    parse_transaction_control, validate_parameter_value,
};
pub use selene_core::{CancellationCause, CancellationChecker, CancellationToken, NodeScanBudget};

#[cfg(any(test, feature = "test-harness"))]
pub use crate::runtime::{
    ExecutorSnapshot, ExecutorSummaryInput, NetGraphDelta, RowOrderPolicy, SnapshotColumn,
    executor_summary,
};
