//! Inventory of supported path syntax and profile features.
//!
//! The contract source of truth for what F05-PR01 lowers. Profile feature IDs
//! come from `spec/gql-profile/profile.json`; syntax coverage names the AST
//! shapes the lowerer accepts. Anything else — parenthesized group
//! alternation (`(a | b)`), path-pattern `|` alternation, group-level
//! quantifiers — is explicitly unsupported here and must fail loudly in the
//! lowerer, never silently degrade. No grammar is added by this inventory.

/// Profile features lowered by the path-automata contract.
pub const SUPPORTED_PATH_FEATURES: &[&str] = &[
    "G002", "G003", "G010", "G011", "G012", "G013", "G014", "G015", "G016", "G017", "G018", "G019",
    "G020", "G036", "G037", "G043", "G044", "G045", "G060", "G061", "GH02",
];

/// Supported syntax shapes, named for stable diagnostics and tests.
pub const SUPPORTED_PATH_SYNTAX: &[&str] = &[
    "node_test",
    "edge_test",
    "concatenation",
    "label_conjunction",
    "label_disjunction",
    "label_negation",
    "label_wildcard",
    "questioned_path_primary",
    "bounded_quantifier",
    "unbounded_quantifier_gated",
    "path_binding",
    "comma_pattern_list",
    "path_mode_prefix",
    "match_mode_prefix",
    "selective_prefix",
    "inline_property_predicate",
    "inline_where",
];

/// Syntax deliberately out of scope for this contract (no fallback).
pub const UNSUPPORTED_PATH_SYNTAX: &[&str] = &[
    "parenthesized_group_alternation",
    "path_pattern_pipe_alternation",
    "group_level_quantifier",
];

/// Stable inventory of what the path lowerer accepts.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PathFeatureInventory {
    /// Supported profile feature IDs in [`SUPPORTED_PATH_FEATURES`] order.
    pub features: Vec<String>,
    /// Supported syntax shapes in [`SUPPORTED_PATH_SYNTAX`] order.
    pub syntax: Vec<String>,
    /// Explicitly unsupported shapes from [`UNSUPPORTED_PATH_SYNTAX`].
    pub unsupported: Vec<String>,
}

impl PathFeatureInventory {
    /// Return true when `feature` is a supported profile feature ID.
    #[must_use]
    pub fn supports_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|item| item == feature)
    }

    /// Return true when `shape` is a supported syntax shape name.
    #[must_use]
    pub fn supports_syntax(&self, shape: &str) -> bool {
        self.syntax.iter().any(|item| item == shape)
    }
}

impl Default for PathFeatureInventory {
    fn default() -> Self {
        supported_path_inventory()
    }
}

/// Build the canonical supported-path inventory.
///
/// Profile IDs mirror the tracked `spec/gql-profile/profile.json` selection
/// for path work (G002/G003 match modes, G010–G020 modes and selectors, G036/
/// G037/G060/G061 quantifiers and questioned primaries, G043–G045/GH02 edge
/// orientations). The lists are literal so a profile drift shows up as a test
/// failure rather than a silent acceptance change.
#[must_use]
pub fn supported_path_inventory() -> PathFeatureInventory {
    PathFeatureInventory {
        features: SUPPORTED_PATH_FEATURES
            .iter()
            .map(ToString::to_string)
            .collect(),
        syntax: SUPPORTED_PATH_SYNTAX
            .iter()
            .map(ToString::to_string)
            .collect(),
        unsupported: UNSUPPORTED_PATH_SYNTAX
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
}
