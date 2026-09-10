//! GQL execution runtime.
//!
//! The runtime consumes an optimized `ExecutionPlan` and a transaction context,
//! walks pattern join trees into binding tables, applies pipeline operators,
//! dispatches procedure calls through tier-checked contexts, and coordinates
//! statement-level transaction control. It relies on parser, analyzer, and
//! planner invariants for binding and type structure; this layer owns runtime
//! three-valued logic, mutation routing through `Mutator`, provider error
//! propagation, and statement output shaping. See Spec 08 §5-§8 and Spec 14
//! §3-§8.

mod binding_table;
mod binding_table_registry;
mod builtin_registry;
mod builtins;
mod call_plan_cache;
mod comparison_domain;
mod context;
mod context_tiers;
mod edge_access;
mod error;
pub(crate) mod evaluator;
mod execution_context;
mod expand;
mod hash_join;
mod join_domain;
mod match_mode;
mod native_algorithms;
mod outcome;
mod outer;
mod parameter_type;
mod path_mode;
mod path_search;
mod pattern;
mod pipeline;
mod plan_cache;
mod plan_runner;
mod prepared_catalog;
mod property_filter_rows;
mod questioned;
mod reference_access;
mod repeat;
mod request;
mod request_runtime;
mod result_order;
mod scan;
mod scan_bind;
mod scan_duration;
mod scan_resolve;
mod scan_seed;
mod session;
#[cfg(any(test, feature = "test-harness"))]
mod snapshot_summary;
mod statement;
mod statement_exec;
mod subplan;
mod value_compare;
mod value_key;
mod value_type_match;
mod visited_set;
mod wco;

pub use binding_table::{Binding, BindingTable};
pub use binding_table_registry::{
    BindingTableAllocationError, BindingTableLookupError, BindingTableRegistry,
};
pub use builtin_registry::BuiltinProcedureRegistry;
pub use call_plan_cache::{CallPlanCache, CallPlanCacheStats, CallPlanKey};
pub use context::{AdaptiveOptimizer, EvalCtx, TxContext};
pub use context_tiers::{GraphContext, MaintenanceContext, MutationContext, ProcedureContext};
pub use error::{DataExceptionSubclass, ExecutorError, ExecutorWarning, WarningSink};
pub use execution_context::{
    ExecutionContext, ExecutionContextError, ExecutionFrame, ExecutionStack, Record,
};
pub use outcome::{
    BindingTableDescriptor, BindingTableField, DiagnosticBundle, ExecutionOutcome, GqlStatusObject,
};
pub use parameter_type::validate_parameter_value;
pub use pattern::execute_pattern;
pub use pipeline::execute_pipeline;
pub use plan_cache::{PlanCache, PlanCacheStats, SharedPlanCache, SharedPlanCacheStats};
pub(crate) use plan_runner::execute_plan;
#[doc(hidden)]
pub use prepared_catalog::{
    PreparedCatalogMutationOutput, PreparedCatalogPlan, PreparedCatalogRequest,
    PreparedCatalogRequestKind, PreparedSessionControl, PreparedTransactionControl,
    parse_graph_independent_session_control, parse_session_close, parse_transaction_control,
};
pub use request::{RequestExecutionInput, RequestParameter};
#[doc(hidden)]
pub use request_runtime::RequestRuntimeHandle;
pub use session::{RollbackOutcome, Session, SessionParameterValue, TransactionOutcome};
#[cfg(any(test, feature = "test-harness"))]
pub use snapshot_summary::{
    ExecutorSnapshot, ExecutorSummaryInput, NetGraphDelta, RowOrderPolicy, SnapshotColumn,
    executor_summary,
};
pub use statement::{CatalogSessionOutput, StatementOutput, WriteOutcome, execute_statement};

pub use crate::plan::{BindingTableColumn, BindingTableSchema};

#[cfg(any(test, feature = "test-harness"))]
pub use evaluator::evaluate_for_test;
