//! Unresolved ISO working-scope syntax and its original origins.

use super::{CatalogObjectReference, SourceSpan};

/// Prefix scopes surrounding one linear query body, in outer-to-inner order.
///
/// A nested query with one linear body is represented without an extra execution
/// operator. Its braces are retained as an origin, not discarded during this
/// parser desugaring. Sibling CALL bodies own separate prefix sequences.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum WorkingScopeClause {
    /// `AT <schema reference>` at a procedure-body head (§§9.2, 16.1).
    At {
        /// Unresolved schema reference.
        reference: CatalogObjectReference,
        /// Original full clause span.
        span: SourceSpan,
    },
    /// `USE <graph expression>` in a focused query (§§14.3, 16.2).
    Use {
        /// Unresolved graph expression.
        expression: GraphExpression,
        /// Original full clause span.
        span: SourceSpan,
    },
    /// Original brace-delimited nested query specification.
    Nested(SourceSpan),
}

/// Bounded graph expressions; source names are never replaced by numeric IDs.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum GraphExpression {
    /// A catalog reference, or a regular bare name subject to §11.1 SR2.
    Reference {
        /// Source reference.
        reference: CatalogObjectReference,
        /// Whether §11.1's regular-name binding lookup applies before catalog lookup.
        may_reference_binding: bool,
    },
    /// Explicit `VARIABLE <binding variable reference>`.
    Variable {
        /// Decoded binding name.
        name: selene_core::DbString,
        /// Original reference span.
        span: SourceSpan,
    },
    /// `CURRENT_GRAPH` or `CURRENT_PROPERTY_GRAPH`.
    Current {
        /// Preserve which ISO spelling appeared.
        property: bool,
        /// Original expression span.
        span: SourceSpan,
    },
}

impl GraphExpression {
    /// Original expression span, excluding the enclosing `USE` keyword.
    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        match self {
            Self::Reference { reference, .. } => reference.span,
            Self::Variable { span, .. } | Self::Current { span, .. } => *span,
        }
    }
}
