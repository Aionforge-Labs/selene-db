//! Logical binding-table planning with explicit effects.
//!
//! This layer lowers the frozen semantic tree into binding-table operators
//! built from semantic descriptors — binding, scope, and expression
//! identities, write-set entries, and procedure registration metadata — with
//! explicit schemas, dependencies, and effects. It carries no physical
//! execution policy: no access paths, join order, batch sizes, or parallelism.
//!
//! The slice covers scan, filter, project, page, one ordinary mutation path,
//! and named-procedure calls. Full family coverage and removal of the old
//! mixed plan belong to F03-PR04; the contracts here are sufficient for
//! physical batches, path semantic nodes, and native adapters. Execution stays
//! on the existing engine through the singular semantic-to-current-executor
//! adapter; mutations describe intent and stage through the existing detached
//! transaction state with no independent publication path.

pub mod descriptors;
pub mod effect;
pub mod explain;
pub mod lowering;
pub(crate) mod lowering_aggregate;
pub(crate) mod lowering_call;
pub(crate) mod lowering_catalog;
pub(crate) mod lowering_cost;
pub(crate) mod lowering_mutation;
pub(crate) mod lowering_query;
pub(crate) mod lowering_scan;
pub mod operator;
pub mod path;

pub use descriptors::{
    LogicalAggregate, LogicalCallDescriptor, LogicalCatalogKind, LogicalControlKind,
    LogicalMutationDescriptor, LogicalOrderKey, LogicalScanDescriptor,
};
pub use effect::{
    EffectSummary, LogicalEffect, check_gp18, classify_analyzed, classify_plan, verify_plan_effects,
};
pub use explain::explain;
pub use lowering::lower_logical;
pub use lowering_cost::measure_lowering_cost;
pub use operator::{
    LogicalMultiplicity, LogicalOp, LogicalOrdering, LogicalPageAmount, LogicalPlan,
};
pub use path::{
    AutomatonStats, BindingExposure, EdgeQuantifierKind, EdgeTest, LoweredPathSet, NodeTest,
    OrientationAcceptance, PATH_AUTOMATA_CONTRACT_VERSION, PathAutomaton, PathFeatureInventory,
    PathLoweringLimits, PathModeScope, PathSemanticElement, PathSemanticPattern, PathState,
    PathStateId, PathTransition, PathTransitionId, SelectorScope, TemporaryBinding, TransitionKind,
    acceptance_for, explain_automaton, explain_set, is_selective_selector, lower_path_automata,
    lower_path_automata_with_defaults, measure_path_lowering, supported_path_inventory,
};
