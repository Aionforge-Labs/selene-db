//! Output-sensitive observations, separate from traversal results.

use crate::{BindingId, PathModeScope, PathTransitionId};
use selene_core::{EdgeId, NodeId, Value};

/// A cost-model projection only: no cheapest selector or cost expression ran.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CheapestCostProjection {
    /// One candidate cost per path in every complete clause binding.
    pub candidate_costs: u64,
    /// Edge-cost evaluations for independently costing every matched path,
    /// with no assumed memoization or shared-prefix discount.
    pub edge_cost_evaluations: u64,
}

/// Measured traversal work. Reservation counts/bytes are estimates, not heap profiling.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathExecutionStats {
    /// Popped product states, including states with distinct legal histories.
    pub product_states: u64,
    /// Candidate seed nodes and edge incidences examined, including rejections.
    pub incidences: u64,
    /// Visited legal hop states, indexed by path-local length (zero included).
    pub hop_lengths: Vec<u64>,
    /// Complete clause bindings; temporary reduction never deduplicates these.
    pub matched_rows: u64,
    /// Largest estimated search/output/debug reservation sum.
    pub peak_bytes: usize,
    /// Successful estimated reservation events, not allocator calls.
    pub reservations: u64,
    /// Projection for costing every matched path, without executing selection.
    pub cheapest_projection: CheapestCostProjection,
}

/// One selected traversal choice with the locals visible after that hop.
///
/// TEMPORARY debugging only: neither sequence order nor physical choice ordinal
/// is a public result-order contract. No internal graph row offsets appear here.
#[derive(Clone, Debug, PartialEq)]
pub struct PathObservation {
    /// Automaton position in this clause.
    pub pattern: usize,
    /// Landed automaton transition selected for this hop.
    pub transition: PathTransitionId,
    /// Local mode and explicit/default provenance copied from the automaton.
    pub mode: PathModeScope,
    /// Ordinal in the selected incidence iterator (debug only).
    pub choice: usize,
    /// Graph source of the selected hop.
    pub from: NodeId,
    /// Graph target of the selected hop.
    pub to: NodeId,
    /// Stable identity of the edge, distinguishing parallel edges.
    pub edge: EdgeId,
    /// Path-local length after this hop.
    pub hops: usize,
    /// Repetition count within this transition, never a merged quantifier.
    pub repetition: u32,
    /// Named query locals visible at this choice (including the current group prefix).
    pub locals: Vec<(BindingId, Value)>,
    /// Anonymous captures as (pattern index, temporary slot, value).
    pub temporaries: Vec<(usize, u32, Value)>,
}
