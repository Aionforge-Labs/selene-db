//! Path automata: states and transitions over semantic element tests.
//!
//! One [`PathAutomaton`] lowers one graph pattern (concatenation of
//! node/edge tests). Transitions preserve the binding scope and the
//! duplicate-row contract of the element they consume; the automaton carries
//! the local path-mode scope, the graph match mode, and the selective-prefix
//! metadata without selecting a traversal algorithm (no access paths, join
//! order, batch sizes, or parallelism appear here).
//!
//! Shape conventions (stable, asserted by tests):
//!
//! * single edge: `node -edge-> node` is three states with one node test, one
//!   edge-traverse, one node test transition;
//! * questioned (`?`): the automaton holds an epsilon skip transition *and* an
//!   edge-take transition from the same fork state, and the edge test carries
//!   [`crate::plan::logical::path::BindingExposure::ConditionalSingleton`];
//! * bounded `{0,1}`: a single quantified edge transition with
//!   `min = 0, max = 1` and a
//!   [`crate::plan::logical::path::BindingExposure::GroupList`] exposure —
//!   never an epsilon skip, never a conditional singleton;
//! * quantified (`{m,n}` / unbounded): a single quantified edge transition
//!   carrying its own `(min, max)`; nested quantifiers keep per-transition
//!   bounds and are never merged into one global restriction;
//! * flat label disjunction (`:(A|B)`): the supported selected-alternation
//!   form, carried as one opaque test transition whose label predicate is the
//!   source disjunction (scope and multiplicity preserved on the transition;
//!   no exponential branch expansion — the disjunct-arity cap in the lowering
//!   limits fails pathological sources before any such allocation). Other
//!   label forms likewise stay as one opaque test transition. Epsilon
//!   fork/join notes are reserved for the questioned skip;
//! * parenthesized group alternation and pipe alternation stay out of scope
//!   (see `inventory.rs`) and never silently degrade.
//!
//! The automaton never encodes an automaton-state-only visited rule and never
//! traverses the graph; F05-PR02 owns execution.

use crate::{
    MatchMode, PathMode, PathSelector, SourceSpan,
    analyze::ScopeId,
    plan::logical::{LogicalMultiplicity, operator::LogicalOrdering},
};

use super::semantic::PathSemanticPattern;

/// Stable identifier for one automaton state.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct PathStateId(pub u32);

impl PathStateId {
    /// Return the zero-based state index.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Stable identifier for one automaton transition.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct PathTransitionId(pub u32);

impl PathTransitionId {
    /// Return the zero-based transition index.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One automaton state: a position between element tests.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PathState {
    /// State identifier (position in [`PathAutomaton::states`]).
    pub id: PathStateId,
    /// Lexical scope visible at this position.
    pub scope: ScopeId,
    /// Source origin of the element boundary (previous element end merged
    /// with the next element start; entry uses the pattern start).
    pub origin: SourceSpan,
    /// True for the accepting state.
    pub accept: bool,
}

/// What one transition consumes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum TransitionKind {
    /// Consume one node test (index into the pattern's node tests in order).
    NodeTest {
        /// Index of the node test in automaton element order.
        test: u32,
    },
    /// Traverse one edge test (index into the pattern's edge tests in order).
    EdgeTraverse {
        /// Index of the edge test in automaton element order.
        test: u32,
    },
    /// Quantified traversal with its own bounds (never merged across edges).
    QuantifiedEdge {
        /// Index of the edge test in automaton element order.
        test: u32,
        /// Minimum repetitions.
        min: u32,
        /// Maximum repetitions, or `None` for gated unbounded.
        max: Option<u32>,
    },
    /// Empty move for questioned skips and alternation fork/join.
    Epsilon {
        /// Stable note tag (`"questioned_skip"`, `"alternation_fork"`,
        /// `"alternation_join"`).
        note: &'static str,
    },
}

/// One automaton transition with preserved scope and multiplicity.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PathTransition {
    /// Transition identifier (position in [`PathAutomaton::transitions`]).
    pub id: PathTransitionId,
    /// Source state.
    pub from: PathStateId,
    /// Target state.
    pub to: PathStateId,
    /// What the transition consumes.
    pub kind: TransitionKind,
    /// Lexical scope in which the consumed bindings resolve.
    pub scope: ScopeId,
    /// Duplicate-row contract (always preserves duplicates in this layer).
    pub multiplicity: LogicalMultiplicity,
    /// Source origin of the consumed element (or the fork element for
    /// epsilon branches, so spans stay useful through normalization).
    pub origin: SourceSpan,
}

/// Local path-mode scope for one automaton (never a global restriction).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub struct PathModeScope {
    /// Path mode in scope for this pattern.
    pub mode: PathMode,
    /// True when the mode was written explicitly in source.
    pub explicit: bool,
    /// Source span of the MATCH clause carrying the mode.
    pub origin: SourceSpan,
}

/// Graph match mode carried with one automaton (pattern-wide per ISO §16.4).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub struct MatchModeScope {
    /// Match mode of the owning MATCH clause, if any.
    pub mode: Option<MatchMode>,
    /// Source span of the MATCH clause.
    pub origin: SourceSpan,
}

/// Selective-prefix metadata carried with one automaton.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub struct SelectorScope {
    /// Path selector of the owning MATCH clause, if any.
    pub selector: Option<PathSelector>,
    /// True for every selector except `ALL` (ISO §16.6 SR4: a prefix other
    /// than `<all path search>` is selective).
    pub selective: bool,
    /// Source span of the MATCH clause.
    pub origin: SourceSpan,
}

/// Return true for a selective path selector.
///
/// Mirrors the analyzer's gate: every selector except `ALL` is selective —
/// including the counted G019/G020 forms, which are the primary reason to
/// write an unbounded pattern.
#[must_use]
pub const fn is_selective_selector(selector: Option<PathSelector>) -> bool {
    match selector {
        None | Some(PathSelector::All) => false,
        Some(
            PathSelector::Any { .. }
            | PathSelector::AnyShortest
            | PathSelector::AllShortest
            | PathSelector::CountedShortest { .. }
            | PathSelector::CountedShortestGroup { .. },
        ) => true,
    }
}

/// One lowered path automaton over semantic element tests.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PathAutomaton {
    /// Index of the owning MATCH clause in lowering order.
    pub clause_index: usize,
    /// Index of the graph pattern within its MATCH clause.
    pub pattern_index: usize,
    /// Semantic concatenation this automaton was built from.
    pub semantic: PathSemanticPattern,
    /// States in identifier order.
    pub states: Vec<PathState>,
    /// Transitions in identifier order.
    pub transitions: Vec<PathTransition>,
    /// Entry state.
    pub entry: PathStateId,
    /// Accepting state.
    pub accept: PathStateId,
    /// Local path-mode scope (per pattern, never global).
    pub mode: PathModeScope,
    /// Graph match mode of the owning clause.
    pub match_mode: MatchModeScope,
    /// Selective-prefix metadata of the owning clause.
    pub selector: SelectorScope,
    /// Duplicate-row contract of the whole automaton.
    pub multiplicity: LogicalMultiplicity,
    /// Source span of the graph pattern.
    pub origin: SourceSpan,
}

impl PathAutomaton {
    /// Return the number of states.
    #[must_use]
    pub fn state_count(&self) -> usize {
        self.states.len()
    }

    /// Return the number of transitions.
    #[must_use]
    pub fn transition_count(&self) -> usize {
        self.transitions.len()
    }

    /// Return the row-order contract (automata never reorder in this layer).
    #[must_use]
    pub const fn ordering(&self) -> LogicalOrdering {
        LogicalOrdering::Preserved
    }

    /// Return true when every transition preserves duplicates.
    #[must_use]
    pub fn preserves_duplicates(&self) -> bool {
        self.multiplicity == LogicalMultiplicity::PreservesDuplicates
            && self.transitions.iter().all(|transition| {
                transition.multiplicity == LogicalMultiplicity::PreservesDuplicates
            })
    }

    /// Return the quantified-edge bounds carried by edge transitions, in
    /// transition order. Each entry keeps its own scope; callers must not
    /// merge them into one global restriction.
    #[must_use]
    pub fn quantifier_bounds(&self) -> Vec<(u32, Option<u32>)> {
        self.transitions
            .iter()
            .filter_map(|transition| match transition.kind {
                TransitionKind::QuantifiedEdge { min, max, .. } => Some((min, max)),
                TransitionKind::NodeTest { .. }
                | TransitionKind::EdgeTraverse { .. }
                | TransitionKind::Epsilon { .. } => None,
            })
            .collect()
    }
}

/// State/transition counts for one automaton (perf evidence, never an SLO).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct AutomatonStats {
    /// Number of states.
    pub states: usize,
    /// Number of transitions.
    pub transitions: usize,
}

impl AutomatonStats {
    /// Build stats for one automaton.
    #[must_use]
    pub fn of(automaton: &PathAutomaton) -> Self {
        Self {
            states: automaton.state_count(),
            transitions: automaton.transition_count(),
        }
    }
}
