//! Graph-pattern AST nodes.

use std::fmt;

use selene_core::DbString;

use crate::ast::{expr::ValueExpr, span::SourceSpan, util::Vec2OrMore};

/// Path traversal mode for `MATCH`.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize,
)]
pub enum PathMode {
    /// Edges and nodes may repeat.
    #[default]
    Walk,
    /// No repeated edges.
    Trail,
    /// No repeated nodes.
    Acyclic,
    /// No repeated nodes except the first and last node may be equal.
    Simple,
}

/// Path selector.
#[derive(Clone, Copy, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum PathSelector {
    /// ISO §16.6 G016 any path search: `ANY [N] [PATHS]`.
    ///
    /// Retains up to N implementation-dependent path bindings per endpoint
    /// partition (§22.4 GR13a). Bare `ANY` defaults to `paths = 1`.
    Any {
        /// Number of path bindings to retain per endpoint partition.
        paths: u32,
    },
    /// `ALL`.
    All,
    /// `ANY SHORTEST`.
    AnyShortest,
    /// `ALL SHORTEST`.
    AllShortest,
    /// ISO §16.6 G019 counted shortest path search: `SHORTEST N [PATHS]`.
    /// Retains up to N individually-shortest path bindings per endpoint
    /// partition (§22.4 GR13b-i). `ANY SHORTEST` == `CountedShortest { paths: 1 }`.
    CountedShortest {
        /// Number of shortest path bindings to retain per endpoint partition.
        paths: u32,
    },
    /// ISO §16.6 G020 counted shortest group search: `SHORTEST [N] GROUP[S]`.
    /// Retains all bindings within the N smallest distinct-length groups per
    /// endpoint partition (§22.4 GR13b-ii). `ALL SHORTEST` == `CountedShortestGroup { groups: 1 }`.
    CountedShortestGroup {
        /// Number of smallest distinct-length groups to retain per endpoint partition.
        groups: u32,
    },
}

impl fmt::Debug for PathSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any { paths: 1 } => f.write_str("Any"),
            Self::Any { paths } => f.debug_struct("Any").field("paths", paths).finish(),
            Self::All => f.write_str("All"),
            Self::AnyShortest => f.write_str("AnyShortest"),
            Self::AllShortest => f.write_str("AllShortest"),
            Self::CountedShortest { paths } => f
                .debug_struct("CountedShortest")
                .field("paths", paths)
                .finish(),
            Self::CountedShortestGroup { groups } => f
                .debug_struct("CountedShortestGroup")
                .field("groups", groups)
                .finish(),
        }
    }
}

/// Match-mode modifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum MatchMode {
    /// `DIFFERENT EDGES`.
    DifferentEdges,
    /// `REPEATABLE ELEMENTS`.
    RepeatableElements,
}

/// Label expression.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum LabelExpr {
    /// One label name.
    Single(DbString),
    /// Conjunction.
    Conjunction(Vec2OrMore<LabelExpr>),
    /// Disjunction.
    Disjunction(Vec2OrMore<LabelExpr>),
    /// Negation.
    Negation(Box<LabelExpr>),
    /// Wildcard label.
    Wildcard,
}

/// Direction requested by an edge pattern.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum EdgeDirection {
    /// `-[]->`.
    Right,
    /// `<-[]-`.
    Left,
    /// `~[]~`, matching only intrinsically undirected edges.
    Undirected,
    /// `<~[]~`, matching left-directed or undirected edges.
    LeftOrUndirected,
    /// `~[]~>`, matching undirected or right-directed edges.
    UndirectedOrRight,
    /// `<-[]->`, matching directed edges in either direction.
    LeftOrRight,
    /// `-[]-`, matching all three orientations.
    Any,
}

impl EdgeDirection {
    pub(crate) const fn includes_right(self) -> bool {
        matches!(
            self,
            Self::Right | Self::UndirectedOrRight | Self::LeftOrRight | Self::Any
        )
    }

    pub(crate) const fn includes_left(self) -> bool {
        matches!(
            self,
            Self::Left | Self::LeftOrUndirected | Self::LeftOrRight | Self::Any
        )
    }

    pub(crate) const fn includes_undirected(self) -> bool {
        matches!(
            self,
            Self::Undirected | Self::LeftOrUndirected | Self::UndirectedOrRight | Self::Any
        )
    }
}

/// Edge-pattern quantifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum Quantifier {
    /// ISO graph-pattern quantifier (`*`, `+`, `{n}`, `{m,n}`, etc.).
    GraphPattern {
        /// Minimum number of repetitions.
        min: u32,
        /// Maximum number of repetitions, or `None` for unbounded.
        max: Option<u32>,
    },
    /// ISO questioned path primary (`?`), which preserves singleton exposure.
    Questioned,
}

/// Node pattern.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct NodePattern {
    /// Optional binding.
    pub binding: Option<DbString>,
    /// Optional label expression.
    pub label_expr: Option<LabelExpr>,
    /// Inline property predicates in source order.
    pub properties: Vec<(DbString, ValueExpr)>,
    /// Optional inline `WHERE`.
    pub inline_where: Option<ValueExpr>,
    /// Source span.
    pub span: SourceSpan,
}

/// Edge pattern.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EdgePattern {
    /// Optional binding.
    pub binding: Option<DbString>,
    /// Direction.
    pub direction: EdgeDirection,
    /// Whether the source used the bracket-free abbreviated syntax (§16.7).
    pub abbreviated: bool,
    /// Optional label expression.
    pub label_expr: Option<LabelExpr>,
    /// Inline property predicates in source order.
    pub properties: Vec<(DbString, ValueExpr)>,
    /// Optional variable-length quantifier.
    pub quantifier: Option<Quantifier>,
    /// Optional inline `WHERE`.
    pub inline_where: Option<ValueExpr>,
    /// Source span.
    pub span: SourceSpan,
}

/// One graph-pattern element.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum PatternElement {
    /// Node pattern.
    Node(NodePattern),
    /// Edge pattern.
    Edge(EdgePattern),
}

/// One graph pattern.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GraphPattern {
    /// Optional path binding.
    pub path_binding: Option<DbString>,
    /// Alternating node/edge/node elements.
    pub elements: Vec<PatternElement>,
    /// Source span.
    pub span: SourceSpan,
}

/// `MATCH` clause.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct MatchClause {
    /// Whether this is `OPTIONAL MATCH`.
    pub optional: bool,
    /// Optional path selector.
    pub selector: Option<PathSelector>,
    /// Optional match mode.
    pub match_mode: Option<MatchMode>,
    /// Path mode; defaults to [`PathMode::Walk`].
    pub path_mode: PathMode,
    /// Whether the path mode was written explicitly in source.
    pub path_mode_explicit: bool,
    /// Whether an explicit `PATH` / `PATHS` keyword was written in source
    /// (ISO/IEC 39075:2024 §16.6 `<path or paths>`, Feature G014).
    ///
    /// Pure surface sugar per ISO §1.2.4: it has no semantic effect over the
    /// no-keyword spelling. The flagger stamps `FeatureId::G014` iff this is
    /// `true`; the runtime treats it as inert. `false` when absent.
    pub path_or_paths: bool,
    /// Graph patterns.
    pub patterns: Vec<GraphPattern>,
    /// Optional statement-level `WHERE`.
    pub where_clause: Option<ValueExpr>,
    /// Source span.
    pub span: SourceSpan,
}
