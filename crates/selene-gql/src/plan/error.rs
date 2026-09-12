//! Planner diagnostics.

use selene_core::DbString;

use crate::{GqlStatus, SourceSpan, analyze::BindingId};

/// Query-planning failure.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum PlannerError {
    /// The planner reached a syntactic surface it does not yet lower.
    #[error("planner cannot lower {feature}: not yet supported")]
    #[diagnostic(code(SLENE_P_010))]
    NotImplemented {
        /// Stable missing-feature tag asserted by tests.
        feature: &'static str,
        /// Source span requiring the missing planner capability.
        #[label("not implemented by the planner yet")]
        span: SourceSpan,
    },

    /// A binding reference produced by the analyzer no longer points at a
    /// declaration during lowering.
    #[error("binding {binding:?} resolved by analyzer is missing during lowering")]
    #[diagnostic(code(SLENE_P_011))]
    BindingResolutionLost {
        /// Lost analyzer binding.
        binding: BindingId,
        /// Source span where the binding was required.
        #[label("binding should resolve here")]
        span: SourceSpan,
    },

    /// A value expression in the analyzed AST has no expression-type cell.
    #[error("expression cell missing for value expression at {span:?}")]
    #[diagnostic(code(SLENE_P_012))]
    ExpressionTypeMissing {
        /// Source span of the expression with no analyzer type cell.
        #[label("missing expression type")]
        span: SourceSpan,
    },

    /// The procedure registry passed to planning no longer contains a
    /// procedure that was resolved during analysis.
    #[error("procedure {procedure:?} not found in registry during planning")]
    #[diagnostic(code(SLENE_P_013))]
    UnknownProcedure {
        /// Qualified procedure name.
        procedure: Box<[DbString]>,
        /// Source span of the procedure call.
        #[label("unknown procedure")]
        span: SourceSpan,
    },

    /// A mutation statement reached the planner without its analyzer write set.
    #[error("mutation statement reached planner without analyzer write set")]
    #[diagnostic(code(SLENE_P_014))]
    WriteSetMissing {
        /// Source span of the mutation pipeline.
        #[label("missing write set")]
        span: SourceSpan,
    },

    /// Analyzer write-set order and planner AST walk disagreed.
    #[error("write-set entry could not be paired with an INSERT AST element")]
    #[diagnostic(code(SLENE_P_015))]
    WriteSetPatternMismatch {
        /// Source span of the unmatched write site.
        #[label("unmatched write-set entry")]
        span: SourceSpan,
    },

    /// Procedure registry metadata changed between analysis and planning.
    #[error("procedure {procedure:?} metadata changed between analyze and plan: {detail}")]
    #[diagnostic(code(SLENE_P_016))]
    ProcedureMetadataMismatch {
        /// Qualified procedure name.
        procedure: Box<[DbString]>,
        /// Stable mismatch detail tag.
        detail: &'static str,
        /// Source span of the procedure call or yield item.
        #[label("metadata mismatch")]
        span: SourceSpan,
    },

    /// The planner could not construct a static database string during lowering.
    #[error("planner could not construct static database string during lowering: {detail}")]
    #[diagnostic(code(SLENE_P_017))]
    StaticStringConstructionFailed {
        /// Stable static identifier detail tag.
        detail: &'static str,
        /// Source span of the construct requiring the static identifier.
        #[label("string construction failed")]
        span: SourceSpan,
    },

    /// Set-composition arms are not column name-equal (ISO/IEC 39075:2024
    /// §14.2 SR v "FCQE and FLQE shall be column name-equal and
    /// column-combinable"). The binder binds each arm independently, so a
    /// `RETURN 1 AS x UNION ALL RETURN 2 AS y` would otherwise silently
    /// relabel the right arm to the left arm's schema.
    #[error("{op} arms are not column name-equal at column {position}: lhs={lhs:?}, rhs={rhs:?}")]
    #[diagnostic(code(SLENE_P_019))]
    SetOpArmsNotCombinable {
        /// Set operator keyword.
        op: &'static str,
        /// Zero-based column position of the first name mismatch.
        position: usize,
        /// Left arm column name (`None` for an unnamed column).
        lhs: Option<DbString>,
        /// Right arm column name (`None` for an unnamed column).
        rhs: Option<DbString>,
        /// Source span of the composite query.
        #[label("set-op arms must have equal column names")]
        span: SourceSpan,
    },

    /// A planner-visible implementation-defined limit would be exceeded.
    #[error("{limit_name} {actual} exceeds implementation-defined limit {limit}")]
    #[diagnostic(code(SLENE_P_018))]
    ProgramLimitExceeded {
        /// Stable limit name asserted by tests.
        limit_name: &'static str,
        /// Configured limit.
        limit: u32,
        /// Requested value.
        actual: u32,
        /// Source span of the construct exceeding the limit.
        #[label("limit exceeded")]
        span: SourceSpan,
    },

    /// An explicit `<...type key label set>` (Feature GG21) declared a key
    /// label set whose effective cardinality falls outside the
    /// implementation-defined (IL003) element-type key-label-set cardinality
    /// bounds (ISO/IEC 39075:2024 §18.2 SR10/SR11 for nodes, §18.3 SR11/SR12 for
    /// edges). selene-db sets the IL003 minimum and maximum both to 1, so any
    /// cardinality other than 1 (the empty bare `<implies>`, or a multi-label
    /// `:A & :B =>`) is rejected here with the spec-defined GQLSTATUS.
    #[error(
        "{element} type key label set cardinality {actual} is outside the supported range [{min}, {max}]"
    )]
    #[diagnostic(code(SLENE_P_020))]
    KeyLabelSetCardinality {
        /// `"node"` or `"edge"`.
        element: &'static str,
        /// Observed key-label-set cardinality.
        actual: usize,
        /// IL003 minimum cardinality.
        min: u32,
        /// IL003 maximum cardinality.
        max: u32,
        /// The spec-defined GQLSTATUS for this element/direction (42012/42013
        /// for nodes, 42014/42015 for edges).
        status: GqlStatus,
        /// Source span of the key-label-set production.
        #[label("key label set out of range")]
        span: SourceSpan,
    },

    /// An explicit `<...type key label set>` was followed by a separate implied
    /// `<...type label set>` (the `:Key => :Implied` shape). Per ISO/IEC
    /// 39075:2024 §18.2 SR7/SR8 the element type's label set is the union of the
    /// key label set and the implied label set, which requires key-label-set
    /// *containment* identification rather than exact equality. selene-db defers
    /// that semantics; this is an honest `FEATURE_NOT_SUPPORTED` rather than a
    /// silent mis-identification.
    #[error(
        "{element} type key label set with a separate implied label set is not yet supported \
         (key label set followed by a distinct implied label set requires containment identification)"
    )]
    #[diagnostic(code(SLENE_P_021))]
    SeparateImpliedLabelSet {
        /// `"node"` or `"edge"`.
        element: &'static str,
        /// Source span of the key-label-set production.
        #[label("separate implied label set not supported")]
        span: SourceSpan,
    },

    /// One statement carries both catalog and data effects.
    ///
    /// The selected GP18 policy forbids catalog/data mixing. This error keeps
    /// the mix from being silently split into separate commits.
    #[error("catalog and data effects cannot be mixed in one statement: {detail}")]
    #[diagnostic(code(SLENE_P_022))]
    EffectMixing {
        /// Stable mixing detail tag asserted by tests.
        detail: &'static str,
        /// Source span of the mixed statement.
        #[label("mixed catalog/data effects")]
        span: SourceSpan,
    },

    /// A lowered plan's metadata-resolved effects disagree with its category.
    ///
    /// A read-only plan carrying data, catalog, or maintenance effects is a
    /// registry drift between analysis and planning or a lowering bug.
    /// Execution must not proceed with query authority.
    #[error("plan effect mismatch: {detail}")]
    #[diagnostic(code(SLENE_P_023))]
    EffectMismatch {
        /// Stable mismatch detail tag asserted by tests.
        detail: &'static str,
        /// Source span of the mismatched plan.
        #[label("effect mismatch")]
        span: SourceSpan,
    },
}

impl PlannerError {
    /// Map this planner failure to its GQLSTATUS code.
    #[must_use]
    pub const fn gqlstatus(&self) -> GqlStatus {
        match self {
            Self::NotImplemented { .. } => GqlStatus::FEATURE_NOT_SUPPORTED,
            Self::BindingResolutionLost { .. }
            | Self::ExpressionTypeMissing { .. }
            | Self::UnknownProcedure { .. }
            | Self::WriteSetMissing { .. }
            | Self::WriteSetPatternMismatch { .. }
            | Self::ProcedureMetadataMismatch { .. }
            | Self::StaticStringConstructionFailed { .. } => {
                GqlStatus::IMPLEMENTATION_DEFINED_ERROR
            }
            Self::ProgramLimitExceeded { .. } => GqlStatus::PROGRAM_LIMIT_EXCEEDED,
            // ISO §14.2 SR v is a Syntax Rule; its violation is the syntax
            // error class (matching the analyzer's SR-violation precedents).
            Self::SetOpArmsNotCombinable { .. } => GqlStatus::SYNTAX_ERROR,
            // ISO §18.2 SR10/SR11 / §18.3 SR11/SR12 attach a specific
            // GQLSTATUS to the IL003 cardinality violation per element/direction.
            Self::KeyLabelSetCardinality { status, .. } => *status,
            // The separate-implied-label-set shape is a deferred feature.
            Self::SeparateImpliedLabelSet { .. } => GqlStatus::FEATURE_NOT_SUPPORTED,
            // GP18 mixing and plan/semantic effect disagreement are
            // transaction-state violations, matching the analyzer's
            // MutatingProcedureInReadPipeline precedent (25G02).
            Self::EffectMixing { .. } | Self::EffectMismatch { .. } => {
                GqlStatus::INVALID_TRANSACTION_STATE_MIXING
            }
        }
    }
}
