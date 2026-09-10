//! Statement AST nodes.

use selene_core::DbString;

use crate::ast::{
    call::{InlineProcedureCall, ProcedureCall},
    catalog_ref::CatalogObjectReference,
    ddl::DdlStatement,
    expr::{CharacterStringLiteralKind, ValueExpr},
    mutation::MutationPipeline,
    pattern::MatchClause,
    span::SourceSpan,
    types::GqlType,
    util::NonEmpty,
};

/// Top-level GQL statement.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum Statement {
    /// Read query pipeline.
    Query(QueryPipeline),
    /// Set-composed read pipelines.
    Composite {
        /// First pipeline.
        first: QueryPipeline,
        /// Remaining pipelines paired with their set operator; type-enforced
        /// non-empty because a composite statement has at least one set op.
        rest: NonEmpty<(SetOp, QueryPipeline)>,
        /// Source span.
        span: SourceSpan,
    },
    /// `NEXT`-chained read pipelines.
    Chained {
        /// Chained pipeline blocks.
        blocks: Vec<QueryPipeline>,
        /// Source span.
        span: SourceSpan,
    },
    /// Write-side mutation pipeline.
    Mutate(MutationPipeline),
    /// Data-definition statement.
    Ddl(DdlStatement),
    /// Top-level procedure call.
    Call(ProcedureCall),
    /// `EXPLAIN <statement>`.
    ///
    /// Plans the inner statement and returns a textual plan without executing
    /// the inner statement.
    Explain {
        /// Inner statement to plan.
        inner: Box<Statement>,
        /// Source span.
        span: SourceSpan,
    },
    /// `START TRANSACTION`.
    StartTransaction {
        /// Source span.
        span: SourceSpan,
    },
    /// `COMMIT`.
    Commit {
        /// Source span.
        span: SourceSpan,
    },
    /// `ROLLBACK`.
    Rollback {
        /// Source span.
        span: SourceSpan,
    },
    /// `SESSION SET VALUE <param> [<type>] = <value expression>` (ISO feature GS03).
    ///
    /// Binds a session-local value parameter. The `if_not_exists` flag carries
    /// the `IF NOT EXISTS` qualifier from `<session set parameter name>`
    /// (ISO/IEC 39075:2024 section 7.4): when set, an existing binding is left
    /// untouched.
    SessionSetValue {
        /// Database-string parameter name without the leading `$`.
        param: DbString,
        /// Optional declared type for the target session parameter.
        declared_type: Option<GqlType>,
        /// Value expression bound to the parameter.
        value: Box<ValueExpr>,
        /// `IF NOT EXISTS` was present on the parameter specification.
        if_not_exists: bool,
        /// Source span.
        span: SourceSpan,
    },
    /// `SESSION SET TIME ZONE <time zone string>` (ISO feature GS15).
    SessionSetTimeZone {
        /// Decoded IANA region name or fixed-offset string.
        zone: String,
        /// Source spelling class for the time-zone character string literal.
        zone_source_kind: CharacterStringLiteralKind,
        /// Source span.
        span: SourceSpan,
    },
    /// `SESSION SET [PROPERTY] GRAPH <current graph>` (ISO/IEC 39075:2024 section 7.1).
    SessionSetGraph {
        /// Current-graph expression selected by the command.
        target: SessionSetGraphTarget,
        /// Source span.
        span: SourceSpan,
    },
    /// `SESSION RESET [ <session reset arguments> ]` (ISO features GS04/GS07/GS08/GS16).
    SessionReset {
        /// Reset target selected by the arguments (bare = all characteristics).
        target: SessionResetTarget,
        /// Source span.
        span: SourceSpan,
    },
    /// `SESSION CLOSE` (ISO/IEC 39075:2024 section 7.3).
    ///
    /// Sets the session termination flag; no ISO feature code (Conformance
    /// Rules: None).
    SessionClose {
        /// Source span.
        span: SourceSpan,
    },
}

/// Current graph expression selected by `SESSION SET [PROPERTY] GRAPH`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum SessionSetGraphTarget {
    /// `CURRENT_GRAPH`.
    CurrentGraph,
    /// `CURRENT_PROPERTY_GRAPH`.
    CurrentPropertyGraph,
    /// Absolute schema reference used by `SESSION SET SCHEMA`.
    SchemaReference(CatalogObjectReference),
    /// Absolute or current-schema-relative catalog graph reference.
    CatalogReference(CatalogObjectReference),
}

/// Target selected by `<session reset arguments>` (ISO/IEC 39075:2024 section 7.2).
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum SessionResetTarget {
    /// `SESSION RESET` (bare) or `RESET [ALL] CHARACTERISTICS`: reset every
    /// session characteristic (parameters and time zone). ISO feature GS04.
    AllCharacteristics,
    /// `SESSION RESET SCHEMA`: restore the home/generated schema authority.
    Schema,
    /// `SESSION RESET [PROPERTY] GRAPH`: restore the home/generated graph authority.
    Graph,
    /// `SESSION RESET [ALL] PARAMETERS`: clear all session parameters only.
    /// ISO feature GS08.
    Parameters,
    /// `SESSION RESET TIME ZONE`: reset the session time zone to the ID048
    /// default. ISO feature GS07.
    TimeZone,
    /// `SESSION RESET [PARAMETER] <param>`: clear a single named session
    /// parameter. ISO feature GS16.
    Parameter(DbString),
}

impl Statement {
    /// Return the source span for this statement.
    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        match self {
            Self::Query(pipeline) => pipeline.span,
            Self::Composite { span, .. } | Self::Chained { span, .. } => *span,
            Self::Mutate(pipeline) => pipeline.span,
            Self::Ddl(statement) => statement.span(),
            Self::Call(call) => call.span,
            Self::Explain { span, .. } => *span,
            Self::StartTransaction { span } | Self::Commit { span } | Self::Rollback { span } => {
                *span
            }
            Self::SessionSetValue { span, .. }
            | Self::SessionSetTimeZone { span, .. }
            | Self::SessionSetGraph { span, .. }
            | Self::SessionReset { span, .. }
            | Self::SessionClose { span } => *span,
        }
    }
}

/// Set operator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum SetOp {
    /// `UNION` (distinct).
    Union,
    /// `UNION ALL` (multiset).
    UnionAll,
    /// `INTERSECT` (distinct).
    Intersect,
    /// `INTERSECT ALL` (multiset).
    IntersectAll,
    /// `EXCEPT` (distinct).
    Except,
    /// `EXCEPT ALL` (multiset).
    ExceptAll,
    /// `OTHERWISE`.
    Otherwise,
}

/// Read query pipeline.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct QueryPipeline {
    /// Lexical prefixes and brace origins surrounding this linear query body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub working_scopes: Vec<super::working_scope::WorkingScopeClause>,
    /// Original SELECT origin when this pipeline was desugared by the parser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_origin: Option<SourceSpan>,
    /// Ordered pipeline statements.
    pub statements: Vec<PipelineStatement>,
    /// Source span.
    pub span: SourceSpan,
}

/// One read-side pipeline statement.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum PipelineStatement {
    /// `MATCH`.
    Match(MatchClause),
    /// `FILTER`.
    Filter(ValueExpr),
    /// `LET`.
    Let(Vec<LetBinding>),
    /// Row expansion (`FOR`).
    For(ForStatement),
    /// `ORDER BY`.
    Sorting(Vec<OrderTerm>),
    /// `LIMIT`.
    Limit(LimitValue),
    /// `OFFSET` / `SKIP`.
    Offset(LimitValue),
    /// `RETURN`.
    Return(ReturnClause),
    /// `WITH`.
    With(WithClause),
    /// `CALL`.
    Call(ProcedureCall),
    /// Inline `CALL { ... }`.
    CallSubquery(InlineProcedureCall),
}

impl PipelineStatement {
    /// Return this statement's source span.
    #[must_use]
    pub fn span(&self) -> SourceSpan {
        match self {
            Self::Match(value) => value.span,
            Self::Filter(value) => value.span(),
            Self::Let(values) => span_from_iter(values.iter().map(|value| value.span)),
            Self::For(value) => value.span,
            Self::Sorting(values) => span_from_iter(values.iter().map(|value| value.span)),
            Self::Limit(value) | Self::Offset(value) => value.span(),
            Self::Return(value) => value.span,
            Self::With(value) => value.span,
            Self::Call(value) => value.span,
            Self::CallSubquery(value) => value.span,
        }
    }
}

/// Variable binding in `LET`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LetBinding {
    /// Database-string alias.
    pub alias: DbString,
    /// Optional declared type for the value variable.
    pub declared_type: Option<GqlType>,
    /// Bound value expression.
    pub value: ValueExpr,
    /// Source span.
    pub span: SourceSpan,
}

/// Row-expansion statement.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ForStatement {
    /// Source expression.
    pub source: ValueExpr,
    /// Database-string alias.
    pub alias: DbString,
    /// Optional ISO position output (`WITH ORDINALITY` / `WITH OFFSET`).
    pub position: Option<RowExpansionPosition>,
    /// Source span.
    pub span: SourceSpan,
}

/// Optional position output for ISO `FOR`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RowExpansionPosition {
    /// Position value form.
    pub kind: RowExpansionPositionKind,
    /// Database-string alias.
    pub alias: DbString,
}

/// ISO `FOR` position output kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum RowExpansionPositionKind {
    /// `WITH ORDINALITY`, producing one-based positions.
    Ordinality,
    /// `WITH OFFSET`, producing zero-based offsets.
    Offset,
}

/// `ORDER BY` term.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OrderTerm {
    /// Sorted expression.
    pub expr: ValueExpr,
    /// Sort direction.
    pub direction: OrderDirection,
    /// Optional null-order policy.
    pub nulls: Option<NullsPolicy>,
    /// Source span.
    pub span: SourceSpan,
}

/// Sort direction.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize,
)]
pub enum OrderDirection {
    /// Ascending.
    #[default]
    Asc,
    /// Descending.
    Desc,
}

/// Null ordering policy.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum NullsPolicy {
    /// `NULLS FIRST`.
    NullsFirst,
    /// `NULLS LAST`.
    NullsLast,
}

/// `LIMIT` / `OFFSET` value.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum LimitValue {
    /// Literal count.
    Count(u64, SourceSpan),
    /// Parameter reference.
    Parameter {
        /// Database-string parameter name without the leading `$`.
        name: DbString,
        /// Optional inline declared parameter type.
        declared_type: Option<GqlType>,
        /// Source span of the parameter reference.
        span: SourceSpan,
    },
}

impl LimitValue {
    /// Return source span.
    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        match self {
            Self::Count(_, span) | Self::Parameter { span, .. } => *span,
        }
    }
}

/// `RETURN` clause.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ReturnClause {
    /// `DISTINCT`.
    pub distinct: bool,
    /// `RETURN *`.
    pub star: bool,
    /// Return-list items. Empty when `star` is true.
    pub items: Vec<ReturnItem>,
    /// Optional `GROUP BY` list.
    pub group_by: Option<Vec<ValueExpr>>,
    /// Optional `HAVING` condition.
    pub having: Option<ValueExpr>,
    /// Source span.
    pub span: SourceSpan,
}

/// One `RETURN` projection item.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ReturnItem {
    /// Returned expression.
    pub expr: ValueExpr,
    /// Optional `AS` alias.
    pub alias: Option<DbString>,
    /// Source span of the projection item.
    pub span: SourceSpan,
}

/// `WITH` clause.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct WithClause {
    /// `DISTINCT`.
    pub distinct: bool,
    /// Projected items.
    pub items: Vec<ReturnItem>,
    /// Optional `GROUP BY` list.
    pub group_by: Option<Vec<ValueExpr>>,
    /// Optional `HAVING` condition.
    pub having: Option<ValueExpr>,
    /// Optional post-projection `WHERE`.
    pub where_clause: Option<ValueExpr>,
    /// Source span.
    pub span: SourceSpan,
}

/// Helper for type-bearing DDL statements.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TypedBinding {
    /// Binding name.
    pub name: DbString,
    /// Parsed type.
    pub ty: GqlType,
    /// Source span.
    pub span: SourceSpan,
}

fn span_from_iter(mut spans: impl Iterator<Item = SourceSpan>) -> SourceSpan {
    let Some(first) = spans.next() else {
        return SourceSpan::default();
    };
    spans.fold(first, SourceSpan::merge)
}
