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

pub mod effect;
pub mod explain;
pub mod lowering;
pub mod operator;

pub use effect::{
    EffectSummary, LogicalEffect, check_gp18, classify_analyzed, classify_plan, verify_plan_effects,
};
pub use explain::explain;
pub use lowering::{lower_logical, measure_lowering_cost};
pub use operator::{
    LogicalCallDescriptor, LogicalMultiplicity, LogicalMutationDescriptor, LogicalOp,
    LogicalOrdering, LogicalPageAmount, LogicalPlan, LogicalScanDescriptor,
};
