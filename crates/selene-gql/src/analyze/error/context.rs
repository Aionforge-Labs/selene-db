//! Analyzer type-mismatch context diagnostics.

use selene_core::DbString;

use crate::{BinaryOp, GqlType, analyze::binding::BindingDeclKind};

/// Operation or clause that produced a type mismatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypeMismatchContext {
    /// Binary arithmetic operator.
    BinaryArithmetic {
        /// Operator.
        op: BinaryOp,
        /// Offending operand side.
        side: Side,
    },
    /// Binary comparison operator.
    BinaryComparison {
        /// Operator.
        op: BinaryOp,
        /// Offending operand side.
        side: Side,
    },
    /// Binary boolean operator.
    BinaryBoolean {
        /// Operator.
        op: BinaryOp,
        /// Offending operand side.
        side: Side,
    },
    /// String/list/bytes/path concatenation.
    BinaryConcat {
        /// Offending operand side.
        side: Side,
    },
    /// String predicate operator.
    BinaryStringPredicate {
        /// Operator.
        op: BinaryOp,
        /// Offending operand side.
        side: Side,
    },
    /// Unary numeric negation.
    UnaryNegate,
    /// Unary boolean negation.
    UnaryNot,
    /// `IS TRUE` / `IS FALSE` / `IS UNKNOWN` operand.
    IsTruthValue,
    /// Unsupported `IS TYPED` target.
    IsTypedTarget,
    /// `IS NORMALIZED` operand.
    IsNormalized,
    /// `NORMALIZE` source operand.
    NormalizeFunction,
    /// Explicit TRIM source operand.
    TrimSource,
    /// Explicit TRIM character operand.
    TrimCharacter,
    /// `PATH[...]` element expression.
    PathConstructorElement {
        /// Zero-based path element-list position.
        position: usize,
    },
    /// CASE branch result unification failed.
    CaseBranchUnification,
    /// List literal element unification failed.
    ListLiteralUnification,
    /// IN-list value unification failed.
    InListUnification,
    /// Boolean condition clause check failed.
    Condition {
        /// Condition clause kind.
        clause: ConditionClause,
    },
    /// Procedure argument did not match the registered parameter type.
    ProcedureArgument {
        /// Qualified procedure name.
        procedure: Box<[DbString]>,
        /// Declared parameter name.
        parameter: DbString,
        /// Zero-based positional argument index.
        position: usize,
    },
    /// `LIMIT` / `OFFSET` parameter type declaration cannot produce an amount.
    LimitAmount,
    /// A graph expression resolved to a non-graph binding.
    GraphExpression,
}

impl std::fmt::Display for TypeMismatchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GraphExpression => f.write_str("graph expression"),
            Self::BinaryArithmetic { op, side } => {
                write!(f, "{side} operand of arithmetic operator {op:?}")
            }
            Self::BinaryComparison { op, side } => {
                write!(f, "{side} operand of comparison operator {op:?}")
            }
            Self::BinaryBoolean { op, side } => {
                write!(f, "{side} operand of boolean operator {op:?}")
            }
            Self::BinaryConcat { side } => write!(f, "{side} operand of concat operator"),
            Self::BinaryStringPredicate { op, side } => {
                write!(f, "{side} operand of string predicate {op:?}")
            }
            Self::UnaryNegate => f.write_str("operand of unary negate"),
            Self::UnaryNot => f.write_str("operand of unary NOT"),
            Self::IsTruthValue => f.write_str("IS <truth value> operand"),
            Self::IsTypedTarget => f.write_str("IS TYPED target"),
            Self::IsNormalized => f.write_str("IS NORMALIZED operand"),
            Self::NormalizeFunction => f.write_str("NORMALIZE operand"),
            Self::TrimSource => f.write_str("TRIM source operand"),
            Self::TrimCharacter => f.write_str("TRIM character operand"),
            Self::PathConstructorElement { position } => {
                write!(f, "PATH element {position}")
            }
            Self::CaseBranchUnification => f.write_str("CASE branch result"),
            Self::ListLiteralUnification => f.write_str("list literal element"),
            Self::InListUnification => f.write_str("IN-list value"),
            Self::Condition { clause } => write!(f, "{clause} condition"),
            Self::ProcedureArgument {
                procedure,
                parameter,
                position,
            } => {
                write!(f, "argument {position} ({parameter}) of ")?;
                super::fmt_qualified_name(f, procedure)
            }
            Self::LimitAmount => f.write_str("LIMIT/OFFSET parameter"),
        }
    }
}

/// Expected type category for a type mismatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExpectedType {
    /// Any numeric type.
    Numeric,
    /// Boolean.
    Boolean,
    /// String.
    String,
    /// String or bytes type.
    StringOrBytes,
    /// Comparable type.
    Comparable,
    /// List, string, bytes, or path type.
    ListStringBytesOrPath,
    /// List type.
    List,
    /// Non-negative integer amount.
    LimitAmount,
    /// One specific GQL type.
    Specific(GqlType),
}

impl std::fmt::Display for ExpectedType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Numeric => f.write_str("numeric"),
            Self::Boolean => f.write_str("boolean"),
            Self::String => f.write_str("string"),
            Self::StringOrBytes => f.write_str("string or bytes"),
            Self::Comparable => f.write_str("comparable"),
            Self::ListStringBytesOrPath => f.write_str("list, string, bytes, or path"),
            Self::List => f.write_str("list"),
            Self::LimitAmount => f.write_str("non-negative integer"),
            Self::Specific(ty) => write!(f, "{ty:?}"),
        }
    }
}

/// Operand side for binary type diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    /// Left-hand side.
    Lhs,
    /// Right-hand side.
    Rhs,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Lhs => "left",
            Self::Rhs => "right",
        })
    }
}

/// Boolean condition clause kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConditionClause {
    /// `MATCH ... WHERE`.
    MatchWhere,
    /// Node/edge pattern inline `WHERE`.
    InlineWhere,
    /// `FILTER`.
    Filter,
    /// `HAVING`.
    Having,
    /// `WITH ... WHERE`.
    WithWhere,
    /// `CASE WHEN`.
    CaseWhen,
}

impl std::fmt::Display for ConditionClause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MatchWhere => "MATCH WHERE",
            Self::InlineWhere => "inline WHERE",
            Self::Filter => "FILTER",
            Self::Having => "HAVING",
            Self::WithWhere => "WITH WHERE",
            Self::CaseWhen => "CASE WHEN",
        })
    }
}

/// Pattern element categories used by [`super::AnalysisError::PatternKindMismatch`].
///
/// The bind pass groups declaration sites by the graph element they introduce
/// (node, edge, path, value). Cross-category reuse via the same name is a
/// semantic error; same-category reuse is allowed (e.g., `MATCH (n)` followed
/// by `INSERT (n)-[:K]->(m)` legitimately reuses `n` as a node variable).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternElementKind {
    /// `MATCH (n)` / `INSERT (n)`.
    Node,
    /// `MATCH ()-[e]->()` / `INSERT ()-[e]->()`.
    Edge,
    /// `path = (...)`.
    Path,
}

impl PatternElementKind {
    /// Categorize a [`BindingDeclKind`] for compatibility checks.
    #[must_use]
    pub const fn from_decl_kind(kind: BindingDeclKind) -> Option<Self> {
        match kind {
            BindingDeclKind::NodePattern | BindingDeclKind::InsertNode => Some(Self::Node),
            BindingDeclKind::EdgePattern | BindingDeclKind::InsertEdge => Some(Self::Edge),
            BindingDeclKind::PathBinding => Some(Self::Path),
            BindingDeclKind::LetAlias
            | BindingDeclKind::ForAlias
            | BindingDeclKind::ProjectionAlias
            | BindingDeclKind::YieldColumn => None,
        }
    }
}

impl std::fmt::Display for PatternElementKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Node => "node variable",
            Self::Edge => "edge variable",
            Self::Path => "path variable",
        })
    }
}
