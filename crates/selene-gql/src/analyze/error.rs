//! Analyzer diagnostics.
//!
//! # `#[diagnostic(code(..))]` prefix taxonomy
//!
//! The `code(..)` attribute is the miette/`thiserror` *display* code only; the
//! authoritative GQLSTATUS for every variant is [`AnalysisError::gqlstatus`]
//! (ISO/IEC 39075:2024 §23.1 Table 8), which downstream surfaces use. Two
//! prefixes coexist intentionally:
//!
//! - `SLENE_GQL_<status>` — embeds the GQLSTATUS code directly (e.g.
//!   `SLENE_GQL_42N03` for `UNDEFINED_REFERENCE`). Used by every variant whose
//!   GQLSTATUS is a single, stable, public code.
//! - `SLENE_A_0NN` — opaque analyzer-local ordinals (`010`..`018`). Used by the
//!   nine closed-graph (GG02) static-schema variants
//!   ([`AnalysisError::SchemaUnknownNodeType`] through
//!   [`AnalysisError::SchemaRequiredEdgeLabelRemoved`]). These all map to the
//!   same `GqlStatus::GRAPH_TYPE_VIOLATION` (G2000) class, so embedding the
//!   status in the display code would make all nine collide on one string; the
//!   `SLENE_A_*` ordinals keep them distinguishable in diagnostics while
//!   `gqlstatus()` reports the correct shared G2000 to callers. The ordinals
//!   are display-only and are *not* a contract — do not parse them.

use selene_core::{DbString, LabelSet, PropertyValueType};

mod context;

pub use context::{ConditionClause, ExpectedType, PatternElementKind, Side, TypeMismatchContext};

use crate::{
    GqlStatus, GqlType, PathMode, PathSelector, ProcedureMutability, SourceSpan,
    analyze::binding::BindingDeclKind,
};

/// Semantic-analysis failure.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum AnalysisError {
    /// A request would access a second graph under unsupported GT03.
    #[error("accessing multiple graphs is not supported: {first:?} then {requested:?}")]
    #[diagnostic(code(SLENE_GQL_25G04))]
    MultipleGraphs {
        /// The transaction's first or pinned graph.
        first: selene_catalog::GraphId,
        /// The second graph selected by source.
        requested: selene_catalog::GraphId,
        /// Original second-access span.
        #[label("second graph in the transaction")]
        span: SourceSpan,
    },
    /// A reference does not resolve to any binding in the enclosing scopes.
    #[error("undefined reference: {name}")]
    #[diagnostic(code(SLENE_GQL_42N03))]
    UndefinedReference {
        /// Unresolved binding name.
        name: DbString,
        /// Source span of the unresolved reference.
        #[label("not bound in scope")]
        span: SourceSpan,
        /// Optional repair hint.
        #[help]
        hint: Option<String>,
    },

    /// A strict declaration site redeclared a binding already present in its scope.
    #[error("binding {name} is already declared in this scope")]
    #[diagnostic(code(SLENE_GQL_42N10))]
    Shadow {
        /// Redeclared binding name.
        name: DbString,
        /// Source span of the redeclaration.
        #[label("conflicts with an earlier binding")]
        span: SourceSpan,
        /// Source span of the prior declaration.
        #[label("first declared here")]
        prior_span: SourceSpan,
    },

    /// A pattern variable is reused with an element kind incompatible with
    /// its prior declaration (e.g. a node variable later used as an edge,
    /// or a path binding aliased over an existing node variable).
    #[error(
        "pattern variable {name} is already bound as a {prior} and cannot be reused as a {current}"
    )]
    #[diagnostic(code(SLENE_GQL_42N10))]
    PatternKindMismatch {
        /// Reused binding name.
        name: DbString,
        /// Element kind of the prior declaration.
        prior: PatternElementKind,
        /// Element kind of the new occurrence.
        current: PatternElementKind,
        /// Source span of the new occurrence.
        #[label("incompatible reuse")]
        span: SourceSpan,
        /// Source span of the prior declaration.
        #[label("first declared here")]
        prior_span: SourceSpan,
    },

    /// A value alias was reused where GQL requires a graph pattern binding.
    #[error(
        "binding {name} is already bound as {prior_kind:?} and cannot be reused as a {new_kind}"
    )]
    #[diagnostic(code(SLENE_GQL_42N10))]
    AliasReusedAsPatternBinding {
        /// Reused binding name.
        name: DbString,
        /// Prior non-pattern declaration kind.
        prior_kind: BindingDeclKind,
        /// New graph-pattern occurrence kind.
        new_kind: PatternElementKind,
        /// Source span of the new occurrence.
        #[label("alias cannot be reused as a pattern binding")]
        span: SourceSpan,
    },

    /// The analyzer encountered an AST surface it does not route yet.
    #[error("not implemented: {message}")]
    #[diagnostic(code(SLENE_GQL_42N01))]
    NotImplemented {
        /// Human-readable missing capability.
        message: String,
        /// Source span requiring the missing analyzer capability.
        #[label("not implemented yet")]
        span: SourceSpan,
        /// Optional implementation hint.
        #[help]
        hint: Option<String>,
    },

    /// ISO 16.4 forbids unbounded quantifiers without a restrictive or selective gate.
    #[error(
        "unbounded variable-length edge pattern requires a restrictive path mode, selective path selector, or DIFFERENT EDGES match mode"
    )]
    #[diagnostic(code(SLENE_GQL_42001))]
    UnboundedRequiresGate {
        /// Path mode in scope for the offending pattern.
        mode: PathMode,
        /// Path selector in scope for the offending pattern.
        selector: Option<PathSelector>,
        /// Source span of the unbounded quantifier.
        #[label("unbounded quantifier requires an ISO 16.4 gate")]
        span: SourceSpan,
    },

    /// ISO 20.6 scalar value query expression shape violation.
    #[error("invalid VALUE subquery shape: {message}")]
    #[diagnostic(code(SLENE_GQL_42001))]
    ValueSubqueryShapeViolation {
        /// Human-readable ISO 20.6 rule failure.
        message: String,
        /// Source span of the invalid VALUE subquery shape.
        #[label("violates ISO 20.6 scalar value query expression shape")]
        span: SourceSpan,
    },

    /// ISO 20.9 forbids aggregate functions directly containing aggregate
    /// functions.
    #[error("invalid aggregate expression: {message}")]
    #[diagnostic(code(SLENE_GQL_42001))]
    AggregateNestingViolation {
        /// Human-readable ISO 20.9 rule failure.
        message: String,
        /// Source span of the nested aggregate expression.
        #[label("aggregate cannot contain another aggregate")]
        span: SourceSpan,
    },

    /// ISO 14.11 requires every grouped non-aggregate projection item to be
    /// one of the grouping keys.
    #[error("grouped projection item must be a grouping key or aggregate expression")]
    #[diagnostic(code(SLENE_GQL_42001))]
    GroupedProjectionItemNotGrouped {
        /// Source span of the invalid projection item.
        #[label("not a grouping key or aggregate expression")]
        span: SourceSpan,
    },

    /// ISO 14.11 forbids `RETURN *` over a unit incoming binding table.
    #[error("RETURN * requires a non-unit incoming binding table")]
    #[diagnostic(code(SLENE_GQL_42001))]
    ReturnStarRequiresInput {
        /// Source span of the invalid `RETURN *`.
        #[label("no incoming bindings to expand")]
        span: SourceSpan,
    },

    /// ISO 14.10 forbids nested query specifications inside sort keys.
    #[error("ORDER BY sort key cannot contain a nested query specification")]
    #[diagnostic(code(SLENE_GQL_42001))]
    SortKeyContainsNestedQuery {
        /// Source span of the invalid sort key.
        #[label("nested query specification is not allowed in a sort key")]
        span: SourceSpan,
    },

    /// ISO 14.10 restricts aggregate functions in post-`RETURN` sort keys.
    #[error("ORDER BY sort key cannot contain an aggregate function in this RETURN context")]
    #[diagnostic(code(SLENE_GQL_42001))]
    SortKeyContainsAggregate {
        /// Source span of the invalid sort key.
        #[label("aggregate function is not allowed in this sort key")]
        span: SourceSpan,
    },

    /// ISO 14.10 SR IV: a sort key referenced a binding outside ORDER_REFS.
    ///
    /// Reached only when `GROUP BY`, `DISTINCT`, or an aggregate return item
    /// has closed the set to the return aliases (plus grouping keys). A plain
    /// `RETURN` keeps the incoming working table in scope and the planner
    /// carries the referenced binding across the projection instead.
    #[error(
        "ORDER BY sort key references `{name}`, which this RETURN discards; \
         GROUP BY, DISTINCT, and aggregate projections can only be ordered by \
         their output columns"
    )]
    #[diagnostic(code(SLENE_GQL_42001))]
    SortKeyReferenceNotInScope {
        /// The out-of-scope binding variable name.
        name: String,
        /// Source span of the invalid sort key.
        #[label("this binding does not survive the projection")]
        span: SourceSpan,
    },

    /// A reference is syntactically resolved but not valid in this expression context.
    #[error("invalid reference: {message}")]
    #[diagnostic(code(SLENE_GQL_42002))]
    InvalidReference {
        /// Human-readable rule failure.
        message: String,
        /// Source span of the invalid reference.
        #[label("invalid reference here")]
        span: SourceSpan,
    },

    /// Analyzer expression recursion exceeded the implementation-defined cap.
    #[error("expression nesting depth {depth} exceeds analyzer limit")]
    #[diagnostic(code(SLENE_GQL_5GQL1))]
    RecursionLimitExceeded {
        /// Depth observed when the limit was exceeded.
        depth: u32,
    },

    /// A statically-decidable type mismatch.
    #[error("{context}: expected {expected}, found {found:?}")]
    #[diagnostic(code(SLENE_GQL_22G03))]
    TypeMismatch {
        /// Operation or clause that required a different type.
        context: TypeMismatchContext,
        /// Expected type category.
        expected: ExpectedType,
        /// Resolved type that violated the expectation.
        found: GqlType,
        /// Source span of the incompatible expression.
        #[label("incompatible type")]
        span: SourceSpan,
    },
    /// Conflicting inline types were declared for one parameter.
    #[error("conflicting declared types for parameter ${name}")]
    #[diagnostic(code(SLENE_GQL_22G03))]
    ConflictingParameterTypes {
        /// Name without the leading `$`.
        name: DbString,
        /// Conflicts in encounter order.
        declarations: Vec<(GqlType, SourceSpan)>,
    },
    /// Procedure name was not registered.
    #[error("unknown procedure: {}", display_qualified_name(name))]
    #[diagnostic(code(SLENE_GQL_42N04))]
    UnknownProcedure {
        /// Qualified procedure name.
        name: Box<[DbString]>,
        /// Source span of the procedure call.
        #[label("procedure is not registered")]
        span: SourceSpan,
    },

    /// Procedure argument arity mismatch.
    #[error(
        "wrong argument count for {}: expected {}, found {actual}",
        display_qualified_name(procedure),
        display_argument_range(*minimum, *expected)
    )]
    #[diagnostic(code(SLENE_GQL_22G03))]
    WrongArgumentCount {
        /// Qualified procedure name.
        procedure: Box<[DbString]>,
        /// Maximum expected argument count.
        expected: usize,
        /// Minimum expected argument count.
        minimum: usize,
        /// Actual argument count.
        actual: usize,
        /// Source span of the procedure call.
        #[label("wrong number of arguments")]
        span: SourceSpan,
    },

    /// `YIELD col` referenced a column not in the procedure output schema.
    #[error(
        "unknown YIELD column {column} for procedure {}",
        display_qualified_name(procedure)
    )]
    #[diagnostic(code(SLENE_GQL_42N03))]
    UnknownYieldColumn {
        /// Qualified procedure name.
        procedure: Box<[DbString]>,
        /// Requested output column.
        column: DbString,
        /// Source span of the YIELD item.
        #[label("column is not produced by this procedure")]
        span: SourceSpan,
    },

    /// A read-only pipeline invoked a procedure declared as graph-writing,
    /// schema-writing, or administrative.
    #[error(
        "mutating procedure {} cannot be invoked in a read pipeline",
        display_qualified_name(procedure)
    )]
    #[diagnostic(code(SLENE_GQL_25G02))]
    MutatingProcedureInReadPipeline {
        /// Qualified procedure name.
        procedure: Box<[DbString]>,
        /// Declared procedure mutability.
        mutability: ProcedureMutability,
        /// Source span of the procedure call.
        #[label("read pipelines cannot invoke mutating procedures")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found no matching node type.
    #[error("{labels:?} does not match any node type in graph type {graph_type}")]
    #[diagnostic(code(SLENE_A_010))]
    SchemaUnknownNodeType {
        /// Observed static label set.
        labels: LabelSet,
        /// Bound graph type name.
        graph_type: DbString,
        /// Source span of the offending label expression or pattern.
        #[label("unknown node type")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found no matching edge type.
    #[error("edge label {label} does not match any edge type in graph type {graph_type}")]
    #[diagnostic(code(SLENE_A_011))]
    SchemaUnknownEdgeType {
        /// Edge label.
        label: DbString,
        /// Bound graph type name.
        graph_type: DbString,
        /// Source span of the offending edge label.
        #[label("unknown edge type")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found an edge endpoint mismatch.
    #[error(
        "edge label {label}: declared as {expected_source} -> {expected_target} but used as {observed_source:?} -> {observed_target:?}"
    )]
    #[diagnostic(code(SLENE_A_012))]
    SchemaEdgeEndpointMismatch {
        /// Edge label.
        label: DbString,
        /// Expected source node type name.
        expected_source: String,
        /// Expected target node type name.
        expected_target: String,
        /// Observed source label set.
        ///
        /// Boxed (together with `observed_target`) so this variant does not
        /// inflate `AnalysisError` past clippy's `result_large_err` threshold:
        /// `LabelSet` wraps `SmallVec<[DbString; 3]>`, so two inline copies
        /// dominated the variant. The `Box` moves both onto the cold
        /// error-construction path.
        observed_source: Box<LabelSet>,
        /// Observed target label set. Boxed for the same reason as
        /// `observed_source`.
        observed_target: Box<LabelSet>,
        /// Source span of the offending edge pattern.
        #[label("endpoint types do not match edge declaration")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found an undeclared property.
    #[error("property {property} is not declared by {declared_in} in graph type {graph_type}")]
    #[diagnostic(code(SLENE_A_013))]
    SchemaUndeclaredProperty {
        /// Undeclared property key.
        property: DbString,
        /// Node or edge type name that was checked.
        declared_in: DbString,
        /// Bound graph type name.
        graph_type: DbString,
        /// Source span of the property write.
        #[label("property is not declared")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found a property value type mismatch.
    #[error("property {property} of {declared_in} declared {expected} but value is {found:?}")]
    #[diagnostic(code(SLENE_A_014))]
    SchemaPropertyTypeMismatch {
        /// Property key.
        property: DbString,
        /// Node or edge type name that declared the property.
        declared_in: DbString,
        /// Expected runtime storage type.
        expected: PropertyValueType,
        /// Statically inferred GQL type.
        found: GqlType,
        /// Source span of the offending value expression.
        #[label("value type is incompatible with property declaration")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found a missing required property.
    #[error("required property {property} of {declared_in} missing at INSERT site")]
    #[diagnostic(code(SLENE_A_015))]
    SchemaRequiredPropertyMissing {
        /// Required property key.
        property: DbString,
        /// Node or edge type name that declared the property.
        declared_in: DbString,
        /// Source span of the insert pattern.
        #[label("required property is not supplied")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found a required property removal.
    #[error("required property {property} of {declared_in} cannot be REMOVE'd")]
    #[diagnostic(code(SLENE_A_016))]
    SchemaRequiredPropertyRemoved {
        /// Required property key.
        property: DbString,
        /// Node or edge type name that declared the property.
        declared_in: DbString,
        /// Source span of the remove item.
        #[label("required property cannot be removed")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found an invalid INSERT label expression.
    #[error("INSERT requires a single label or label conjunction; {form} is not allowed")]
    #[diagnostic(code(SLENE_A_017))]
    SchemaInvalidInsertLabelExpr {
        /// Invalid label-expression form.
        form: InvalidLabelForm,
        /// Source span of the invalid pattern.
        #[label("invalid INSERT label expression")]
        span: SourceSpan,
    },

    /// Static closed-graph validation found removal of an edge's required label.
    #[error("required edge label {label} of {declared_in} cannot be REMOVE'd")]
    #[diagnostic(code(SLENE_A_018))]
    SchemaRequiredEdgeLabelRemoved {
        /// Required edge label.
        label: DbString,
        /// Edge type name that declared the label.
        declared_in: DbString,
        /// Source span of the remove item.
        #[label("edge label cannot be removed")]
        span: SourceSpan,
    },
}

/// Label-expression forms that cannot identify a fresh closed-graph INSERT type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InvalidLabelForm {
    /// Label disjunction, such as `:Person|Company`.
    Disjunction,
    /// Label negation, such as `:!Person`.
    Negation,
    /// Label wildcard, such as `:%`.
    Wildcard,
    /// No label expression was present.
    Missing,
}

impl std::fmt::Display for InvalidLabelForm {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Disjunction => "label disjunction",
            Self::Negation => "label negation",
            Self::Wildcard => "label wildcard",
            Self::Missing => "missing label",
        })
    }
}

fn display_qualified_name(name: &[DbString]) -> QualifiedNameDisplay<'_> {
    QualifiedNameDisplay(name)
}

struct QualifiedNameDisplay<'a>(&'a [DbString]);

impl std::fmt::Display for QualifiedNameDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_qualified_name(f, self.0)
    }
}

fn fmt_qualified_name(f: &mut std::fmt::Formatter<'_>, name: &[DbString]) -> std::fmt::Result {
    let mut first = true;
    for segment in name {
        if !first {
            f.write_str(".")?;
        }
        let text = segment.as_str();
        if text.contains('.') || text.contains('"') {
            write!(f, "\"{}\"", text.replace('"', "\"\""))?;
        } else {
            f.write_str(text)?;
        }
        first = false;
    }
    Ok(())
}

fn display_argument_range(minimum: usize, maximum: usize) -> String {
    if minimum == maximum {
        maximum.to_string()
    } else {
        format!("{minimum}..={maximum}")
    }
}

impl AnalysisError {
    /// Return this error's ISO GQLSTATUS code.
    #[must_use]
    pub const fn gqlstatus(&self) -> GqlStatus {
        match self {
            Self::MultipleGraphs { .. } => GqlStatus::MULTIPLE_GRAPHS_NOT_SUPPORTED,
            Self::UndefinedReference { .. } => GqlStatus::UNDEFINED_REFERENCE,
            Self::Shadow { .. }
            | Self::PatternKindMismatch { .. }
            | Self::AliasReusedAsPatternBinding { .. } => GqlStatus::DUPLICATE_OBJECT,
            Self::NotImplemented { .. } => GqlStatus::FEATURE_NOT_SUPPORTED,
            Self::UnboundedRequiresGate { .. } => GqlStatus::SYNTAX_ERROR,
            Self::ValueSubqueryShapeViolation { .. } => GqlStatus::SYNTAX_ERROR,
            Self::AggregateNestingViolation { .. } => GqlStatus::SYNTAX_ERROR,
            Self::GroupedProjectionItemNotGrouped { .. } => GqlStatus::SYNTAX_ERROR,
            Self::ReturnStarRequiresInput { .. } => GqlStatus::SYNTAX_ERROR,
            Self::SortKeyContainsNestedQuery { .. } => GqlStatus::SYNTAX_ERROR,
            Self::SortKeyContainsAggregate { .. } => GqlStatus::SYNTAX_ERROR,
            Self::SortKeyReferenceNotInScope { .. } => GqlStatus::SYNTAX_ERROR,
            Self::InvalidReference { .. } => GqlStatus::INVALID_REFERENCE,
            Self::RecursionLimitExceeded { .. } => GqlStatus::PROGRAM_LIMIT_EXCEEDED,
            Self::TypeMismatch { .. } | Self::ConflictingParameterTypes { .. } => {
                GqlStatus::DATATYPE_MISMATCH
            }
            Self::UnknownProcedure { .. } => GqlStatus::UNKNOWN_PROCEDURE,
            Self::WrongArgumentCount { .. } => GqlStatus::DATATYPE_MISMATCH,
            Self::UnknownYieldColumn { .. } => GqlStatus::UNDEFINED_REFERENCE,
            Self::MutatingProcedureInReadPipeline { .. } => {
                GqlStatus::INVALID_TRANSACTION_STATE_MIXING
            }
            Self::SchemaUnknownNodeType { .. }
            | Self::SchemaUnknownEdgeType { .. }
            | Self::SchemaEdgeEndpointMismatch { .. }
            | Self::SchemaUndeclaredProperty { .. }
            | Self::SchemaPropertyTypeMismatch { .. }
            | Self::SchemaRequiredPropertyMissing { .. }
            | Self::SchemaRequiredPropertyRemoved { .. }
            | Self::SchemaInvalidInsertLabelExpr { .. }
            | Self::SchemaRequiredEdgeLabelRemoved { .. } => GqlStatus::GRAPH_TYPE_VIOLATION,
        }
    }

    pub(crate) fn undefined_reference(name: DbString, span: SourceSpan) -> Self {
        Self::UndefinedReference {
            name,
            span,
            hint: Some("declare the variable before this reference".into()),
        }
    }
}
