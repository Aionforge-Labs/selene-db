//! Path semantic IR and automata lowering contract (F05-PR01).
//!
//! This module defines the single automata contract consumed by F03-PR04
//! (compiler cutover) and F05-PR02 (path execution):
//!
//! * [`inventory`] names the supported path syntax and profile features;
//! * [`semantic`] carries source origins into element tests with explicit
//!   element identities, temporaries, conditional singletons, and group
//!   references;
//! * [`automaton`] lowers concatenation, selected alternation, and
//!   quantified/grouped patterns into states and transitions that preserve
//!   binding scope and multiplicity;
//! * [`lowering`] builds automata from the frozen semantic tree with ISO
//!   §16.4 finite-result enforcement and explicit resource limits;
//! * [`explain`] renders stable debug fixtures without runtime addresses.
//!
//! Intrinsic edge directionality (directed vs undirected storage) stays a
//! runtime property; patterns record only the declared token plus the derived
//! accepted-orientation mask. Local path-mode scope, graph match mode, and
//! selective-prefix metadata ride each automaton without selecting a
//! traversal algorithm. There is no runtime traversal here, no grammar
//! expansion, and no automaton-state-only visited rule.
//!
//! Physical planning transports this contract into `JoinTree::Paths`. The
//! product-path batch operator is the only path evaluator; generic row callers
//! enter the same operator until their own deletion in F04-PR09.

pub mod automaton;
mod builder;
pub mod explain;
mod gates;
pub(crate) use gates::collect_match_clauses;
pub mod inventory;
pub mod limits;
pub mod lowering;
pub mod semantic;

pub use automaton::{
    AutomatonStats, MatchModeScope, PathAutomaton, PathModeScope, PathState, PathStateId,
    PathTransition, PathTransitionId, SelectorScope, TransitionKind, is_selective_selector,
};
pub use explain::{explain_automaton, explain_set};
pub use inventory::{
    PathFeatureInventory, SUPPORTED_PATH_FEATURES, SUPPORTED_PATH_SYNTAX, UNSUPPORTED_PATH_SYNTAX,
    supported_path_inventory,
};
pub use limits::PathLoweringLimits;
pub use lowering::{
    LoweredPathSet, PATH_AUTOMATA_CONTRACT_VERSION, lower_path_automata,
    lower_path_automata_with_defaults, measure_path_lowering,
};
pub use semantic::{
    BindingExposure, EdgeQuantifierKind, EdgeTest, NodeTest, OrientationAcceptance,
    PathSemanticElement, PathSemanticPattern, TemporaryBinding, acceptance_for,
};
