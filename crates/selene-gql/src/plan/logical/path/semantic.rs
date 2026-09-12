//! Path semantic IR: element tests with explicit binding exposure.
//!
//! Nodes carry analyzer identities — [`crate::analyze::BindingId`],
//! [`crate::analyze::ScopeId`], [`crate::analyze::AnalyzedType`], and semantic
//! [`crate::analyze::ExprId`] predicate identities — plus the source
//! [`crate::SourceSpan`] origin of each element. Syntax supplies only ordering
//! and spans; every identity, type, and predicate comes from the frozen
//! semantic tree. There is no second parser and no second type resolver here.
//!
//! Binding exposure is explicit per ISO §4.11.3: a questioned path primary
//! (`?`) exposes a *conditional singleton* (edge `NULL` or one `EdgeRef`),
//! while a `{0,1}`-quantified primary exposes a *group* (`LIST` of zero or one
//! edge). The two traverse equivalent lengths but bind differently; the IR
//! keeps them as distinct variants so normalization can never erase the
//! distinction. Anonymous elements receive [`TemporaryBinding`] slots whose
//! origin is the source element span, keeping spans useful after
//! normalization.

use crate::{
    EdgeDirection, LabelExpr, SourceSpan,
    analyze::{AnalyzedType, BindingId, ExprId, ScopeId},
};

/// Executor-independent temporary slot for an anonymous pattern element.
///
/// F05-PR02 maps temporaries onto executor hidden slots; this layer only names
/// them. The `origin` is always the source element span that introduced the
/// temporary, so diagnostics stay anchored after normalization.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub struct TemporaryBinding {
    /// Per-automaton temporary index.
    pub slot: u32,
    /// Source element span that introduced this temporary.
    pub origin: SourceSpan,
    /// Stable reason tag asserted by tests (for example,
    /// `"anonymous node"`, `"anonymous edge"`, `"anonymous group"`).
    pub reason: &'static str,
}

/// How one edge element exposes its binding to downstream operators.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum BindingExposure {
    /// Single unquantified edge binding (`(a)-[r]->(b)`).
    Singleton {
        /// Named binding, or `None` for an anonymous edge (carried by `hidden`).
        binding: Option<BindingId>,
        /// Temporary slot when the edge is anonymous.
        hidden: Option<TemporaryBinding>,
    },
    /// Questioned path primary (`[r?]`): conditional singleton.
    ///
    /// The skipped row binds the edge to `NULL` and unifies the final node
    /// with the source node; the taken row binds one `EdgeRef`. This is NOT
    /// a group, even though it also admits zero or one hop.
    ConditionalSingleton {
        /// Named edge binding, or `None` for anonymous.
        binding: Option<BindingId>,
        /// Temporary slot when the edge is anonymous.
        hidden: Option<TemporaryBinding>,
    },
    /// Quantified edge group (`[r*1..3]`, including `{0,1}`).
    ///
    /// Binds `LIST<EdgeRef>` with length in `[min, max]`. A `{0,1}` group
    /// binds an empty or singleton list — observably different from the
    /// `NULL`-or-`EdgeRef` conditional singleton above.
    GroupList {
        /// Named group binding, or `None` for anonymous.
        binding: Option<BindingId>,
        /// Temporary slot when the group is anonymous.
        hidden: Option<TemporaryBinding>,
    },
}

impl BindingExposure {
    /// Return true for the questioned conditional-singleton exposure.
    #[must_use]
    pub const fn is_conditional_singleton(self) -> bool {
        matches!(self, Self::ConditionalSingleton { .. })
    }

    /// Return true for the quantified group exposure.
    #[must_use]
    pub const fn is_group(self) -> bool {
        matches!(self, Self::GroupList { .. })
    }

    /// Return the named binding when one is exposed.
    #[must_use]
    pub const fn named(self) -> Option<BindingId> {
        match self {
            Self::Singleton { binding, .. }
            | Self::ConditionalSingleton { binding, .. }
            | Self::GroupList { binding, .. } => binding,
        }
    }
}

/// Quantifier shape carried on one edge test.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum EdgeQuantifierKind {
    /// Unquantified single edge.
    Single,
    /// Questioned path primary (`?`): conditional singleton, never a group.
    Questioned,
    /// Bounded quantifier (`*`, `+` with a cap, `{m,n}`, `{n}`, `{0,1}`).
    ///
    /// `max` is always `Some` here; unbounded forms use [`Self::Unbounded`].
    /// `{0,1}` stays in this variant so it can never collapse into
    /// [`Self::Questioned`].
    Bounded {
        /// Minimum repetitions.
        min: u32,
        /// Maximum repetitions.
        max: u32,
    },
    /// Unbounded quantifier (`*`, `+`, `{m,}`, `*m..`) admitted only under an
    /// ISO §16.4 gate (restrictive path mode, selective prefix, or
    /// `DIFFERENT EDGES`). The lowerer rejects ungated forms; it never
    /// assigns an arbitrary runtime hop cap.
    Unbounded {
        /// Minimum repetitions.
        min: u32,
    },
}

impl EdgeQuantifierKind {
    /// Return true for the questioned primary.
    #[must_use]
    pub const fn is_questioned(self) -> bool {
        matches!(self, Self::Questioned)
    }
}

/// Accepted traversal orientations derived from one edge-direction token.
///
/// Intrinsic edge directionality (directed vs undirected storage) is a runtime
/// property of the edge; this struct records only what the *pattern* accepts.
/// The `declared` token is preserved alongside the derived mask so debug
/// fixtures can assert the token-to-acceptance mapping without re-deriving it.
/// See `docs/gql/mixed-edge-orientation.md`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub struct OrientationAcceptance {
    /// Source direction token.
    pub declared: EdgeDirection,
    /// Whether left-directed traversal is accepted.
    pub accept_left: bool,
    /// Whether right-directed traversal is accepted.
    pub accept_right: bool,
    /// Whether intrinsically-undirected traversal is accepted.
    pub accept_undirected: bool,
    /// Source span of the edge pattern carrying the token.
    pub origin: SourceSpan,
}

/// Derive the accepted-orientation mask for one direction token.
///
/// The table mirrors `EdgeDirection::{includes_left,includes_right,
/// includes_undirected}` and the mixed-edge orientation contract; it is stored
/// explicitly on each edge test so consumers never recompute it from the token
/// at traversal time.
#[must_use]
pub const fn acceptance_for(declared: EdgeDirection, origin: SourceSpan) -> OrientationAcceptance {
    match declared {
        EdgeDirection::Right => OrientationAcceptance {
            declared,
            accept_left: false,
            accept_right: true,
            accept_undirected: false,
            origin,
        },
        EdgeDirection::Left => OrientationAcceptance {
            declared,
            accept_left: true,
            accept_right: false,
            accept_undirected: false,
            origin,
        },
        EdgeDirection::Undirected => OrientationAcceptance {
            declared,
            accept_left: false,
            accept_right: false,
            accept_undirected: true,
            origin,
        },
        EdgeDirection::LeftOrUndirected => OrientationAcceptance {
            declared,
            accept_left: true,
            accept_right: false,
            accept_undirected: true,
            origin,
        },
        EdgeDirection::UndirectedOrRight => OrientationAcceptance {
            declared,
            accept_left: false,
            accept_right: true,
            accept_undirected: true,
            origin,
        },
        EdgeDirection::LeftOrRight => OrientationAcceptance {
            declared,
            accept_left: true,
            accept_right: true,
            accept_undirected: false,
            origin,
        },
        EdgeDirection::Any => OrientationAcceptance {
            declared,
            accept_left: true,
            accept_right: true,
            accept_undirected: true,
            origin,
        },
    }
}

/// One node element test with its semantic identities and source origin.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct NodeTest {
    /// Named node binding, or `None` for anonymous.
    pub binding: Option<BindingId>,
    /// Temporary slot when the node is anonymous.
    pub temporary: Option<TemporaryBinding>,
    /// Analyzer-inferred binding type.
    pub ty: AnalyzedType,
    /// Static label predicate from the declaring pattern, when present.
    pub label: Option<LabelExpr>,
    /// Semantic predicate identities for inline property values.
    pub property_predicates: Vec<ExprId>,
    /// Semantic predicate identity for inline `WHERE`, when present.
    pub inline_where: Option<ExprId>,
    /// Lexical scope that declares (or reuses) the binding.
    pub scope: ScopeId,
    /// Element position within the pattern.
    pub element_index: usize,
    /// Source span of the node pattern.
    pub origin: SourceSpan,
}

/// One edge element test with its semantic identities and source origin.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct EdgeTest {
    /// Binding exposure (singleton vs conditional singleton vs group).
    pub exposure: BindingExposure,
    /// Quantifier shape; [`EdgeQuantifierKind::Questioned`] pairs only with
    /// [`BindingExposure::ConditionalSingleton`], bounded forms only with
    /// [`BindingExposure::GroupList`] or [`BindingExposure::Singleton`].
    pub quantifier: EdgeQuantifierKind,
    /// Accepted traversal orientations with the declared token preserved.
    pub orientation: OrientationAcceptance,
    /// Whether the source used bracket-free abbreviated syntax (§16.7).
    pub abbreviated: bool,
    /// Static label predicate for each traversed edge, when present.
    pub label: Option<LabelExpr>,
    /// Semantic predicate identities for inline property values.
    pub property_predicates: Vec<ExprId>,
    /// Semantic predicate identity for inline `WHERE`, when present.
    pub inline_where: Option<ExprId>,
    /// Lexical scope that declares (or reuses) the binding.
    pub scope: ScopeId,
    /// Element position within the pattern.
    pub element_index: usize,
    /// Source span of the edge pattern.
    pub origin: SourceSpan,
}

/// One element of a path semantic pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PathSemanticElement {
    /// Node element test.
    Node(NodeTest),
    /// Edge element test.
    Edge(EdgeTest),
}

impl PathSemanticElement {
    /// Return this element's source origin.
    #[must_use]
    pub const fn origin(&self) -> SourceSpan {
        match self {
            Self::Node(test) => test.origin,
            Self::Edge(test) => test.origin,
        }
    }

    /// Return this element's position within the pattern.
    #[must_use]
    pub const fn element_index(&self) -> usize {
        match self {
            Self::Node(test) => test.element_index,
            Self::Edge(test) => test.element_index,
        }
    }
}

/// One concatenation of alternating node/edge tests with its origins.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PathSemanticPattern {
    /// Optional path binding (`p = (...)`).
    pub path_binding: Option<BindingId>,
    /// Alternating node/edge/node tests in source order (concatenation).
    pub elements: Vec<PathSemanticElement>,
    /// Lexical scope visible for the pattern.
    pub scope: ScopeId,
    /// Source span of the graph pattern.
    pub origin: SourceSpan,
}

impl PathSemanticPattern {
    /// Return the named bindings exposed by this pattern in element order.
    #[must_use]
    pub fn named_bindings(&self) -> Vec<BindingId> {
        let mut out = Vec::new();
        if let Some(binding) = self.path_binding {
            out.push(binding);
        }
        for element in &self.elements {
            match element {
                PathSemanticElement::Node(test) => {
                    if let Some(binding) = test.binding {
                        out.push(binding);
                    }
                }
                PathSemanticElement::Edge(test) => {
                    if let Some(binding) = test.exposure.named() {
                        out.push(binding);
                    }
                }
            }
        }
        out
    }
}
