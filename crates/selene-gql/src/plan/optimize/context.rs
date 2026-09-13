//! Optimizer context.

use std::sync::OnceLock;

use crate::plan::{ImplDefinedCaps, optimize::IndexCatalog};

/// Context shared by all optimizer rules.
///
/// Marked `#[non_exhaustive]` so future optimizer work can add fields without
/// a breaking change.
#[non_exhaustive]
pub struct OptimizeContext<'a> {
    /// Implementation-defined planner limits.
    pub impl_defined_caps: &'a ImplDefinedCaps,
    /// Optional query-time index catalog.
    ///
    /// Carries both index discovery (label / typed / composite lookups) and the
    /// OPT-5 cost statistics (`total_rows`, `label_cardinality`, …). When
    /// `None`, every index rule and the selectivity estimator fall back to their
    /// structural / heuristic behavior, producing pre-OPT-5 plans verbatim.
    pub index_catalog: Option<&'a dyn IndexCatalog>,
    /// Frozen semantic authority for expression-index equivalence proofs.
    pub analyzed: Option<&'a crate::AnalyzedStatement>,
}

impl<'a> OptimizeContext<'a> {
    /// Construct a context using explicit implementation-defined limits.
    #[must_use]
    pub const fn new(impl_defined_caps: &'a ImplDefinedCaps) -> Self {
        Self {
            impl_defined_caps,
            index_catalog: None,
            analyzed: None,
        }
    }

    /// Attach an index catalog to this context.
    #[must_use]
    pub const fn with_index_catalog(mut self, catalog: &'a dyn IndexCatalog) -> Self {
        self.index_catalog = Some(catalog);
        self
    }

    /// Attach the same semantic tree from which this plan was lowered.
    #[must_use]
    pub const fn with_analyzed(mut self, analyzed: &'a crate::AnalyzedStatement) -> Self {
        self.analyzed = Some(analyzed);
        self
    }
}

static DEFAULT_CAPS: OnceLock<ImplDefinedCaps> = OnceLock::new();

fn default_caps() -> &'static ImplDefinedCaps {
    DEFAULT_CAPS.get_or_init(ImplDefinedCaps::default)
}

impl Default for OptimizeContext<'static> {
    fn default() -> Self {
        Self::new(default_caps())
    }
}
