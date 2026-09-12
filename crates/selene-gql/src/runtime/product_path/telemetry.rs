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
    /// Conservative endpoint-completion certificate work (not distance lookups).
    pub completion_work: u64,
    /// Visited legal hop states, indexed by path-local length (zero included).
    pub hop_lengths: Vec<u64>,
    /// Complete clause bindings; temporary reduction never deduplicates these.
    pub matched_rows: u64,
    /// Complete path bindings qualified before endpoint-partitioned selection.
    pub qualified_paths: u64,
    /// Typed path values constructed after selection (not endpoint adapters).
    pub materialized_paths: u64,
    /// History-frame clones, not shared predecessors or allocator events.
    pub history_clones: u64,
    /// Peak estimated live frontier/scratch history bytes sampled at hops.
    pub peak_history_bytes: usize,
    /// Peak estimated qualified-candidate retention before selection.
    pub peak_candidate_bytes: usize,
    /// Traversal and predicate wall time; excludes the completion certificate.
    pub discovery_time: std::time::Duration,
    /// Endpoint partitioning and selective-choice wall time.
    pub selection_time: std::time::Duration,
    /// Final typed path construction wall time, excluding table/batch copies.
    pub materialization_time: std::time::Duration,
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
#[derive(Clone, PartialEq)]
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

impl std::fmt::Debug for PathObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathObservation")
            .field("pattern", &self.pattern)
            .field("transition", &self.transition)
            .field("mode", &self.mode)
            .field("choice", &self.choice)
            .field("from", &self.from)
            .field("to", &self.to)
            .field("edge", &self.edge)
            .field("hops", &self.hops)
            .field("repetition", &self.repetition)
            .field("local_count", &self.locals.len())
            .field("temporary_count", &self.temporaries.len())
            .finish_non_exhaustive()
    }
}
