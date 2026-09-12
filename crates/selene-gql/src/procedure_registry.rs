#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Planner-facing procedure registry boundary for `selene-gql`.
//!
//! D16 places the single `ProcedureRegistry` trait in `selene-gql` because the
//! planner and executor are the upstream consumers of procedure metadata and
//! dispatch. The native [`BuiltinProcedureRegistry`](crate::BuiltinProcedureRegistry)
//! is the sole production implementor, and the embedder injects
//! `&dyn ProcedureRegistry` into plan and execute calls. See Spec 08 §7.

pub use selene_core::Value;

use std::time::Duration;

use selene_core::DbString;

use crate::{
    GqlStatus, GqlType,
    runtime::{BindingTableAllocationError, ProcedureContext},
};

/// Registry interface consumed by the GQL planner and executor.
///
/// Registration is intentionally not part of this trait. Concrete registries
/// own their startup-time loading APIs; `selene-gql` only needs plan-time
/// metadata lookup and runtime dispatch through an opaque handle.
pub trait ProcedureRegistry: Send + Sync {
    /// Look up procedure metadata by canonical CALL-time name.
    fn lookup(&self, name: &[DbString]) -> Option<ProcedureMetadata>;

    /// Return the registry epoch used by shared plan caches.
    ///
    /// Construct-once registries may keep the default `0`. Registries that can
    /// change lookup metadata or handle dispatch while a [`crate::CallPlanCache`]
    /// or [`crate::SharedPlanCache`] may still contain plans must return a value
    /// that changes whenever cached plans would need to be recompiled.
    fn registry_version(&self) -> u64 {
        0
    }

    /// Iterate registered procedure handles and metadata.
    ///
    /// Registries that cannot enumerate may keep the default empty iterator.
    /// SHOW PROCEDURES uses this cold-path surface for introspection.
    fn iter_handles(&self) -> Box<dyn Iterator<Item = (Vec<DbString>, ProcedureMetadata)> + '_> {
        Box::new(std::iter::empty())
    }

    /// Execute a previously planned procedure handle with evaluated arguments.
    fn execute(
        &self,
        handle: ProcedureHandle,
        args: &[Value],
        ctx: &mut ProcedureContext<'_, '_>,
    ) -> Result<ProcedureResult, ProcedureError>;
}

/// Planner-visible metadata for a registered procedure.
///
/// Owned by `selene-gql` so the planner can consume procedure metadata without
/// reaching outside the gql crate.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ProcedureMetadata {
    /// Exact durable declaration attached to this runtime implementation.
    /// Test-only registries without a catalog may leave this absent.
    pub declaration: Option<std::sync::Arc<selene_catalog::CatalogDescriptor>>,
    /// Opaque handle returned to the executor after successful planning.
    pub handle: ProcedureHandle,
    /// Human-readable procedure summary for catalog introspection.
    pub description: &'static str,
    /// Input signature used for argument type-checking.
    pub signature: ProcedureSignature,
    /// Output schema used for YIELD validation and binding-table construction.
    pub output_schema: ProcedureOutputSchema,
    /// Tier selected by the concrete registry for execution.
    pub tier: ProcedureTier,
    /// Side-effect class declared by the registered procedure.
    pub mutability: ProcedureMutability,
}

impl ProcedureMetadata {
    /// Construct planner-visible metadata for a registered procedure.
    #[must_use]
    pub const fn new(
        handle: ProcedureHandle,
        signature: ProcedureSignature,
        output_schema: ProcedureOutputSchema,
        tier: ProcedureTier,
        mutability: ProcedureMutability,
    ) -> Self {
        Self {
            declaration: None,
            handle,
            description: "",
            signature,
            output_schema,
            tier,
            mutability,
        }
    }

    /// Attach a human-readable procedure summary.
    #[must_use]
    pub const fn with_description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }
}

/// Opaque procedure handle.
///
/// The planner treats this as an uninterpreted token. The implementing registry
/// defines the internal handle encoding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProcedureHandle(u64);

impl ProcedureHandle {
    /// Construct an opaque handle from a registry-defined raw value.
    ///
    /// `selene-gql` does not interpret the raw value. The concrete registry
    /// chooses the encoding and receives the handle back unchanged through
    /// [`ProcedureRegistry::execute`].
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the opaque raw value.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Static signature used for plan-time argument validation.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ProcedureSignature {
    /// Positional parameters in declaration order.
    pub parameters: Vec<ProcedureParameter>,
    /// Version where this procedure became available.
    pub since_version: &'static str,
}

impl ProcedureSignature {
    /// Construct a signature from positional parameters.
    #[must_use]
    pub const fn new(parameters: Vec<ProcedureParameter>) -> Self {
        Self {
            parameters,
            since_version: "1.0.0",
        }
    }

    /// Attach the version where this procedure became available.
    #[must_use]
    pub const fn with_since_version(mut self, since_version: &'static str) -> Self {
        self.since_version = since_version;
        self
    }

    /// Return the accepted positional argument range for this signature.
    #[must_use]
    pub fn arity(&self) -> ProcedureArity {
        let maximum = self.parameters.len();
        let minimum = self
            .parameters
            .iter()
            .position(|parameter| parameter.default.is_some())
            .unwrap_or(maximum);
        debug_assert!(
            self.parameters[minimum..]
                .iter()
                .all(|parameter| parameter.default.is_some()),
            "procedure defaults must be a trailing suffix"
        );
        ProcedureArity { minimum, maximum }
    }
}

impl Default for ProcedureSignature {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// One declared procedure parameter.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ProcedureParameter {
    /// Parameter name. Diagnostic-only; arguments are currently positional.
    pub name: DbString,
    /// Expected static type for the corresponding positional argument.
    pub ty: GqlType,
    /// Whether a statically resolved `NULL` argument is accepted.
    pub nullable: bool,
    /// Human-readable parameter description for catalog introspection.
    pub description: &'static str,
    /// Documentation-only default value text for optional parameters.
    pub default_doc: Option<&'static str>,
    /// Executable default value for omitted trailing optional arguments.
    pub default: Option<ProcedureDefaultValue>,
}

impl ProcedureParameter {
    /// Construct a declared procedure parameter.
    #[must_use]
    pub const fn new(name: DbString, ty: GqlType, nullable: bool) -> Self {
        Self {
            name,
            ty,
            nullable,
            description: "",
            default_doc: None,
            default: None,
        }
    }

    /// Attach a human-readable parameter description.
    #[must_use]
    pub const fn with_description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// Attach documentation-only default value text.
    #[must_use]
    pub const fn with_default_doc(mut self, default_doc: &'static str) -> Self {
        self.default_doc = Some(default_doc);
        self
    }

    /// Attach an executable default value.
    #[must_use]
    pub const fn with_default(mut self, default: ProcedureDefaultValue) -> Self {
        self.default = Some(default);
        self
    }
}

/// Executable default value descriptor for optional procedure parameters.
///
/// This deliberately stays smaller than [`Value`]: procedure signatures are
/// static metadata, and source-side parameter structs derive `Eq`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProcedureDefaultValue {
    /// Boolean default value.
    Boolean(bool),
    /// NULL default value.
    Null,
    /// Signed integer default value.
    Integer(i64),
    /// Static string default value.
    String(&'static str),
}

/// Positional procedure argument range after accounting for trailing defaults.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcedureArity {
    /// Minimum caller-supplied argument count.
    pub minimum: usize,
    /// Maximum argument count after default materialization.
    pub maximum: usize,
}

impl ProcedureArity {
    /// Return true when `actual` is within this accepted argument range.
    #[must_use]
    pub const fn accepts(self, actual: usize) -> bool {
        self.minimum <= actual && actual <= self.maximum
    }

    /// Return true when the range accepts only one exact argument count.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        self.minimum == self.maximum
    }
}

/// Output schema as a relation of named columns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcedureOutputSchema {
    /// Output columns in declaration order.
    pub columns: Vec<ProcedureOutputColumn>,
}

/// One output column from a procedure call.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ProcedureOutputColumn {
    /// Column name matched against `YIELD col` references.
    pub name: DbString,
    /// Static type assigned to the YIELD binding.
    pub ty: GqlType,
    /// Whether the procedure may return NULL for this column.
    pub nullable: bool,
    /// Human-readable output-column description for catalog introspection.
    pub description: &'static str,
}

impl ProcedureOutputColumn {
    /// Construct a declared procedure output column.
    #[must_use]
    pub const fn new(name: DbString, ty: GqlType) -> Self {
        Self {
            name,
            ty,
            nullable: false,
            description: "",
        }
    }

    /// Set whether this output column accepts NULL values at runtime.
    #[must_use]
    pub const fn with_nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }

    /// Attach a human-readable output-column description.
    #[must_use]
    pub const fn with_description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }
}

/// Execution tier advertised by a procedure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcedureTier {
    /// Read-only graph-tier procedure.
    Graph,
    /// Mutation-tier procedure running inside a write transaction.
    Mutation,
    /// Engine-maintenance procedure running against the shared graph.
    Maintenance,
}

/// Side-effect class declared by the registered procedure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcedureMutability {
    /// Procedure cannot mutate graph or catalog state.
    Read,
    /// Procedure may mutate catalog/schema state.
    SchemaWrite,
    /// Procedure may rebuild derived engine state without emitting graph changes.
    MaintenanceWrite,
}

/// Result returned by procedure execution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcedureResult {
    /// Output rows aligned with [`ProcedureMetadata::output_schema`].
    pub rows: Vec<Vec<Value>>,
}

/// Procedure dispatch failure.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProcedureError {
    /// Native algorithm failure retaining its typed cause and procedure context.
    #[error("invalid procedure argument: {detail}")]
    Native {
        /// Procedure-qualified diagnostic, preserving the existing message.
        detail: String,
        /// Original algorithm error, available through the standard error chain.
        #[source]
        source: std::sync::Arc<dyn std::error::Error + Send + Sync>,
    },
    /// An operation accessed a deleted graph referent, distinct from copying it.
    #[error("procedure accessed a deleted graph reference")]
    InvalidReferenceValue,
    /// The procedure handle was unknown to the registry.
    #[error("unknown procedure")]
    UnknownProcedure {
        /// Best-effort procedure name. May be empty for defensive handle-only paths.
        name: Box<[DbString]>,
    },
    /// The registry rejected evaluated arguments.
    #[error("invalid procedure argument: {detail}")]
    InvalidArgument {
        /// Stable diagnostic detail.
        detail: String,
    },
    /// Procedure tier metadata was internally inconsistent.
    #[error("procedure tier mismatch: expected {expected:?}, actual {actual:?}")]
    TierMismatch {
        /// Tier implied by procedure mutability.
        expected: ProcedureTier,
        /// Tier reported by the registry.
        actual: ProcedureTier,
    },
    /// Registry-internal failure or contract violation.
    #[error("procedure internal error: {detail}")]
    Internal {
        /// Stable diagnostic detail.
        detail: String,
    },
    /// Procedure observed caller-requested cooperative cancellation.
    #[error("procedure cancelled")]
    Cancelled,
    /// Procedure observed that the statement deadline elapsed.
    #[error("procedure deadline exceeded")]
    Timeout {
        /// Duration since the deadline elapsed.
        elapsed: Duration,
    },
    /// Procedure observed that the deterministic node-scan budget was exceeded.
    #[error("procedure node scan budget exceeded ({scanned} > {limit})")]
    NodeScanBudgetExceeded {
        /// Maximum allowed scanned nodes.
        limit: usize,
        /// Observed scanned nodes after the batch that crossed the limit.
        scanned: usize,
    },
    /// Procedure output exhausted the request-scoped binding-table ID space.
    #[error("procedure program limit exceeded: {detail}")]
    ProgramLimitExceeded {
        /// Stable allocation-limit detail.
        detail: &'static str,
    },
}

impl ProcedureError {
    /// Map this procedure failure to a GQLSTATUS code.
    #[must_use]
    pub const fn gqlstatus(&self) -> GqlStatus {
        match self {
            Self::InvalidReferenceValue => GqlStatus::INVALID_REFERENCE_VALUE,
            Self::UnknownProcedure { .. } => GqlStatus::UNKNOWN_PROCEDURE,
            Self::InvalidArgument { .. } | Self::Native { .. } => {
                GqlStatus::INVALID_PROCEDURE_ARGUMENT
            }
            Self::TierMismatch { .. } | Self::Internal { .. } => {
                GqlStatus::IMPLEMENTATION_DEFINED_ERROR
            }
            Self::Cancelled => GqlStatus::OPERATION_CANCELLED,
            Self::Timeout { .. } => GqlStatus::DEADLINE_EXCEEDED,
            Self::NodeScanBudgetExceeded { .. } | Self::ProgramLimitExceeded { .. } => {
                GqlStatus::PROGRAM_LIMIT_EXCEEDED
            }
        }
    }
}

impl From<BindingTableAllocationError> for ProcedureError {
    fn from(error: BindingTableAllocationError) -> Self {
        Self::ProgramLimitExceeded {
            detail: error.program_limit_detail(),
        }
    }
}

/// Registry with no registered procedures.
///
/// Use this for analyzer call sites that do not exercise CALL. Any procedure
/// lookup returns `None`, and runtime execution is unreachable because no
/// handle can be planned from this registry.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyProcedureRegistry;

impl ProcedureRegistry for EmptyProcedureRegistry {
    fn lookup(&self, _name: &[DbString]) -> Option<ProcedureMetadata> {
        None
    }

    fn execute(
        &self,
        _handle: ProcedureHandle,
        _args: &[Value],
        _ctx: &mut ProcedureContext<'_, '_>,
    ) -> Result<ProcedureResult, ProcedureError> {
        Err(ProcedureError::UnknownProcedure { name: Box::new([]) })
    }
}
