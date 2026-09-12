//! Explicit resource limits for path-automata lowering.
//!
//! Lowering must check these bounds *before* allocating exponentially sized
//! structures (label-alternation diamonds, state/transition vectors). A
//! pathological source pattern fails with [`crate::plan::PlannerError`]
//! instead of exhausting memory. The bounds never replace language semantics:
//! unbounded quantifiers are still gated by the ISO §16.4 finite-result rule,
//! not by an arbitrary hop cap.

/// Resource limits for one path-automata lowering pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PathLoweringLimits {
    /// Maximum states in one automaton.
    pub max_states_per_automaton: u32,
    /// Maximum transitions in one automaton.
    pub max_transitions_per_automaton: u32,
    /// Maximum pattern elements in one graph pattern.
    pub max_elements_per_pattern: u32,
    /// Maximum flat label-disjunction branches expanded per element test.
    pub max_label_branches: u32,
}

impl PathLoweringLimits {
    /// Default limits: generous for real queries, tight enough to fail fast
    /// on pathological source expansion before allocation.
    pub const DEFAULT: Self = Self {
        max_states_per_automaton: 512,
        max_transitions_per_automaton: 1024,
        max_elements_per_pattern: 128,
        max_label_branches: 16,
    };

    /// Return a copy with a different per-automaton state cap.
    #[must_use]
    pub const fn with_max_states(mut self, max: u32) -> Self {
        self.max_states_per_automaton = max;
        self
    }

    /// Return a copy with a different per-automaton transition cap.
    #[must_use]
    pub const fn with_max_transitions(mut self, max: u32) -> Self {
        self.max_transitions_per_automaton = max;
        self
    }

    /// Return a copy with a different per-pattern element cap.
    #[must_use]
    pub const fn with_max_elements(mut self, max: u32) -> Self {
        self.max_elements_per_pattern = max;
        self
    }

    /// Return a copy with a different label-branch cap.
    #[must_use]
    pub const fn with_max_label_branches(mut self, max: u32) -> Self {
        self.max_label_branches = max;
        self
    }
}

impl Default for PathLoweringLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}
