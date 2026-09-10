//! Immutable resolved applications, independent of parser-owned syntax.

use super::{BindingId, ExprId, ExprIdLookup, ScopeId};
use crate::{ProcedureMetadata, SourceSpan, ValueExpr};

/// Resolved expression operation. Binding and parameter namespaces are disjoint.
#[derive(Clone, Debug, PartialEq)]
pub enum ExpressionKind {
    /// Source literal, with its original spelling category and span.
    Literal(crate::Literal),
    /// Resolved lexical binding identity (never a parameter lookup).
    Binding(BindingId),
    /// Request/session parameter name (never a binding lookup).
    Parameter(selene_core::DbString),
    /// Property selector applied to the first child.
    Property(selene_core::DbString),
    /// List construction.
    List,
    /// Record construction, with field names in source order.
    Record(Vec<selene_core::DbString>),
    /// Path construction.
    Path,
    /// Binary operation on two children.
    Binary(crate::BinaryOp),
    /// Unary operation on one child.
    Unary(crate::UnaryOp),
    /// Scalar/aggregate-looking function application.
    Function {
        /// Qualified function name.
        name: crate::NonEmpty<selene_core::DbString>,
        /// Star argument spelling.
        star: bool,
        /// Duplicate elimination requested.
        distinct: bool,
    },
    /// Duration subtraction.
    Duration(crate::TemporalDurationQualifier),
    /// Typed predicate; the unchanged source predicate payload is read by the adapter.
    Predicate,
    /// Membership in an explicit list.
    InList(bool),
    /// Membership in a list expression.
    InExpression(bool),
    /// Pairwise distinct element references.
    AllDifferent,
    /// Element identity comparison.
    Same,
    /// Property existence check.
    PropertyExists(selene_core::DbString),
    /// Conditional branches in child-pair order, followed by an optional ELSE.
    Case,
    /// Boolean query, whose bindings live in a child lexical scope.
    Exists(bool),
    /// Scalar query, whose bindings live in a child lexical scope.
    ValueQuery,
    /// Unicode normalization.
    Normalize(Option<crate::NormalForm>),
    /// String trimming.
    Trim(crate::TrimSpec),
    /// Explicit cast to the source-declared type.
    Cast(crate::GqlType),
}

/// One node in the independently owned semantic expression tree.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticExpression {
    /// Identity indexing the tree's expression/type cells.
    pub id: ExprId,
    /// Original source occurrence (also retained for synthetic call defaults).
    pub origin: SourceSpan,
    /// Lexical scope used to resolve this occurrence.
    pub scope: ScopeId,
    /// Resolved operation and namespace.
    pub kind: ExpressionKind,
    /// Direct expression children in source order.
    pub children: Vec<ExprId>,
}

impl SemanticExpression {
    pub(crate) fn new(
        id: ExprId,
        expr: &ValueExpr,
        scope: ScopeId,
        binding: Option<BindingId>,
        lookup: &ExprIdLookup,
    ) -> Self {
        let kind = match expr {
            ValueExpr::Literal(value) => ExpressionKind::Literal(value.clone()),
            ValueExpr::Variable { .. } => ExpressionKind::Binding(binding.expect("bound variable")),
            ValueExpr::Parameter { name, .. } => ExpressionKind::Parameter(name.clone()),
            ValueExpr::PropertyAccess { key, .. } => ExpressionKind::Property(key.clone()),
            ValueExpr::ListLiteral { .. } => ExpressionKind::List,
            ValueExpr::RecordLiteral { fields, .. } => {
                ExpressionKind::Record(fields.iter().map(|(name, _)| name.clone()).collect())
            }
            ValueExpr::PathConstructor { .. } => ExpressionKind::Path,
            ValueExpr::BinaryOp { op, .. } => ExpressionKind::Binary(*op),
            ValueExpr::UnaryOp { op, .. } => ExpressionKind::Unary(*op),
            ValueExpr::FunctionCall {
                name,
                star,
                distinct,
                ..
            } => ExpressionKind::Function {
                name: name.clone(),
                star: *star,
                distinct: *distinct,
            },
            ValueExpr::DurationBetween { qualifier, .. } => ExpressionKind::Duration(*qualifier),
            ValueExpr::IsCheck { .. } => ExpressionKind::Predicate,
            ValueExpr::InList { negated, .. } => ExpressionKind::InList(*negated),
            ValueExpr::InListExpression { negated, .. } => ExpressionKind::InExpression(*negated),
            ValueExpr::AllDifferent { .. } => ExpressionKind::AllDifferent,
            ValueExpr::Same { .. } => ExpressionKind::Same,
            ValueExpr::PropertyExists { key, .. } => ExpressionKind::PropertyExists(key.clone()),
            ValueExpr::Case { .. } => ExpressionKind::Case,
            ValueExpr::Exists { negated, .. } => ExpressionKind::Exists(*negated),
            ValueExpr::ValueSubquery { .. } => ExpressionKind::ValueQuery,
            ValueExpr::Normalize { form, .. } => ExpressionKind::Normalize(*form),
            ValueExpr::Trim { spec, .. } => ExpressionKind::Trim(*spec),
            ValueExpr::Cast { target_type, .. } => ExpressionKind::Cast((**target_type).clone()),
        };
        let mut children = Vec::new();
        expr.for_each_child(&mut |child| {
            if let Some(id) = lookup.get(child) {
                children.push(id);
            }
        });
        Self {
            id,
            origin: expr.span(),
            scope,
            kind,
            children,
        }
    }
}

/// One resolved procedure application and its synthesized default arguments.
///
/// Defaults carry the call's source origin, but are never appended to source
/// syntax. The current-plan adapter consumes them alongside the explicit args.
#[derive(Clone, Debug)]
pub struct ResolvedCall {
    pub(crate) span: SourceSpan,
    pub(crate) metadata: ProcedureMetadata,
    pub(crate) defaults: Vec<ValueExpr>,
}

impl ResolvedCall {
    /// Original procedure-call span.
    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        self.span
    }

    /// Signature and runtime handle resolved during analysis.
    #[must_use]
    pub const fn metadata(&self) -> &ProcedureMetadata {
        &self.metadata
    }

    /// Synthesized arguments, in signature order after the explicit arguments.
    #[must_use]
    pub fn defaults(&self) -> &[ValueExpr] {
        &self.defaults
    }

    pub(crate) fn same_signature(&self, other: &ProcedureMetadata) -> bool {
        let expected = &self.metadata;
        expected.signature.parameters.len() == other.signature.parameters.len()
            && expected
                .signature
                .parameters
                .iter()
                .zip(&other.signature.parameters)
                .all(|(left, right)| {
                    left.name == right.name
                        && left.ty == right.ty
                        && left.nullable == right.nullable
                        && left.default == right.default
                })
            && expected.output_schema.columns.len() == other.output_schema.columns.len()
            && expected
                .output_schema
                .columns
                .iter()
                .zip(&other.output_schema.columns)
                .all(|(left, right)| left.name == right.name && left.ty == right.ty)
    }
}
