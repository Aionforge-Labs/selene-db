//! Policy-neutral, maintained in-memory graph candidate sets.
//! Provider-owned members are rebuildable; the facade persists only catalog rules.

use crate::{
    CandidateSet, IndexProvider, Node, ProviderError, ProviderTag, SeleneGraph, VectorCandidateSet,
    VectorCandidateStateInfo,
};
use parking_lot::Mutex;
use selene_core::{Change, DbString, NodeId};
#[cfg(test)]
use selene_core::{EdgeId, LabelSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
mod state;
use state::{
    CandidateState, TrackedEdge, canonicalize_labels, inconsistent, insert_sorted_unique,
    validate_unique_specs, watches_label,
};

/// Fixed registration tag for the maintained candidate-state observer.
pub const CANDIDATE_STATE_PROVIDER_TAG: [u8; 4] = *b"CSET";

/// Declarative rule for one maintained in-memory candidate set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CandidateStateSpec {
    /// Stable set name.
    pub name: DbString,
    /// Required node label.
    pub required_label: Option<DbString>,
    /// Required outgoing labels.
    pub require_outgoing: Vec<DbString>,
    /// Required incoming labels.
    pub require_incoming: Vec<DbString>,
    /// Disqualifying outgoing labels.
    pub exclude_outgoing: Vec<DbString>,
    /// Disqualifying incoming labels.
    pub exclude_incoming: Vec<DbString>,
}
impl CandidateStateSpec {
    /// Construct an unconstrained named set.
    #[must_use]
    pub fn new(name: DbString) -> Self {
        Self {
            name,
            required_label: None,
            require_outgoing: Vec::new(),
            require_incoming: Vec::new(),
            exclude_outgoing: Vec::new(),
            exclude_incoming: Vec::new(),
        }
    }
    /// Require a node label.
    #[must_use]
    pub fn require_label(mut self, label: DbString) -> Self {
        self.required_label = Some(label);
        self
    }
    /// Require an outgoing directed-edge label.
    #[must_use]
    pub fn require_outgoing(mut self, label: DbString) -> Self {
        insert_sorted_unique(&mut self.require_outgoing, label);
        self
    }
    /// Require an incoming directed-edge label.
    #[must_use]
    pub fn require_incoming(mut self, label: DbString) -> Self {
        insert_sorted_unique(&mut self.require_incoming, label);
        self
    }
    /// Exclude an outgoing directed-edge label.
    #[must_use]
    pub fn exclude_outgoing(mut self, label: DbString) -> Self {
        insert_sorted_unique(&mut self.exclude_outgoing, label);
        self
    }
    /// Exclude an incoming directed-edge label.
    #[must_use]
    pub fn exclude_incoming(mut self, label: DbString) -> Self {
        insert_sorted_unique(&mut self.exclude_incoming, label);
        self
    }
}

/// First-party observer maintaining named graph-derived candidate sets.
pub struct MaintainedCandidateStateProvider {
    specs: Vec<CandidateStateSpec>,
    runtime: Mutex<CandidateStateRuntime>,
}
struct CandidateStateRuntime {
    graph: Option<selene_core::GraphId>,
    live: CandidateState,
    live_typed: BTreeMap<DbString, CandidateSet<Node>>,
}
impl MaintainedCandidateStateProvider {
    /// Construct an empty observer, rejecting duplicate names.
    pub fn new(specs: impl IntoIterator<Item = CandidateStateSpec>) -> Result<Self, ProviderError> {
        let mut specs: Vec<_> = specs.into_iter().collect();
        for spec in &mut specs {
            canonicalize_labels(&mut spec.require_outgoing);
            canonicalize_labels(&mut spec.require_incoming);
            canonicalize_labels(&mut spec.exclude_outgoing);
            canonicalize_labels(&mut spec.exclude_incoming);
        }
        validate_unique_specs(&specs)?;
        Ok(Self {
            runtime: Mutex::new(CandidateStateRuntime {
                graph: None,
                live: CandidateState::new(&specs),
                live_typed: BTreeMap::new(),
            }),
            specs,
        })
    }
    /// Construct and initialize against a pinned graph snapshot.
    pub fn from_graph(
        specs: impl IntoIterator<Item = CandidateStateSpec>,
        graph: &SeleneGraph,
    ) -> Result<Self, ProviderError> {
        let provider = Self::new(specs)?;
        provider.rebuild_from_graph(graph)?;
        Ok(provider)
    }
    /// Rebuild completely before replacing live state; reject inconsistent identity maps.
    pub fn rebuild_from_graph(&self, graph: &SeleneGraph) -> Result<(), ProviderError> {
        let mut rebuilt = CandidateState::new(&self.specs);
        for id in graph
            .live_node_candidates()
            .map_err(provider_graph_error)?
            .iter()
        {
            let labels = graph
                .node_labels(id)
                .ok_or_else(|| inconsistent(format!("live node {id} has no label column entry")))?;
            rebuilt.node_labels.insert(id, labels.clone());
        }
        for id in graph
            .live_edge_candidates()
            .map_err(provider_graph_error)?
            .iter()
        {
            let label = graph
                .edge_label(id)
                .ok_or_else(|| inconsistent(format!("live edge {id} has no label")))?;
            if graph.edge_directionality(id) != Some(selene_core::EdgeDirectionality::Directed)
                || !watches_label(&self.specs, label)
            {
                continue;
            }
            let (source, target) = graph
                .edge_endpoints(id)
                .ok_or_else(|| inconsistent(format!("live edge {id} has no endpoints")))?;
            rebuilt.edges.insert(
                id,
                TrackedEdge {
                    label: label.clone(),
                    source,
                    target,
                },
            );
        }
        rebuilt.rebuild_derived(&self.specs);
        rebuilt.generation = graph.meta.generation;
        let mut runtime = self.runtime.lock();
        runtime.graph = Some(graph.graph_id());
        runtime.live = rebuilt;
        runtime.live_typed.clear();
        Ok(())
    }
    /// Look up a configured rule.
    #[must_use]
    pub fn spec(&self, name: &DbString) -> Option<&CandidateStateSpec> {
        self.specs.iter().find(|spec| &spec.name == name)
    }
    /// Return current raw vector candidates.
    #[must_use]
    pub fn candidate_set(&self, name: &DbString) -> Option<VectorCandidateSet> {
        self.runtime
            .lock()
            .live
            .members
            .get_mut(name)
            .map(|members| VectorCandidateSet::from_canonical_nodes(members.candidate_nodes()))
    }
    /// Applied generation watermark.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.runtime.lock().live.generation
    }
    /// Return candidates only when every mutation through the generation was applied.
    pub fn candidate_set_at_generation(
        &self,
        name: &DbString,
        generation: u64,
    ) -> Result<Option<VectorCandidateSet>, ProviderError> {
        let mut runtime = self.runtime.lock();
        check_generation(&runtime, generation)?;
        Ok(runtime
            .live
            .members
            .get_mut(name)
            .map(|members| VectorCandidateSet::from_canonical_nodes(members.candidate_nodes())))
    }
    /// Discover configured sets only at a matching generation.
    pub fn candidate_state_infos_at_generation(
        &self,
        generation: u64,
    ) -> Result<Vec<VectorCandidateStateInfo>, ProviderError> {
        let runtime = self.runtime.lock();
        check_generation(&runtime, generation)?;
        Ok(self
            .specs
            .iter()
            .map(|spec| VectorCandidateStateInfo {
                name: spec.name.clone(),
                generation,
                candidate_count: runtime
                    .live
                    .members
                    .get(&spec.name)
                    .map_or(0, |members| members.len()),
                required_label: spec.required_label.clone(),
                require_outgoing: spec.require_outgoing.clone(),
                require_incoming: spec.require_incoming.clone(),
                exclude_outgoing: spec.exclude_outgoing.clone(),
                exclude_incoming: spec.exclude_incoming.clone(),
            })
            .collect())
    }
    /// Whether a node is currently a member.
    #[must_use]
    pub fn contains(&self, name: &DbString, node: NodeId) -> bool {
        self.runtime
            .lock()
            .live
            .members
            .get(name)
            .is_some_and(|members| members.contains(node))
    }
}

impl IndexProvider for MaintainedCandidateStateProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(CANDIDATE_STATE_PROVIDER_TAG)
    }
    fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        runtime.live_typed.clear();
        runtime.live.apply_change(&self.specs, change)
    }
    fn handles_change_batches(&self) -> bool {
        true
    }
    fn on_changes(&self, changes: &[Change]) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        runtime.live_typed.clear();
        for change in changes {
            runtime.live.apply_change(&self.specs, change)?;
        }
        Ok(())
    }
    fn rebuild_from_graph(&self, graph: &SeleneGraph) -> Result<(), ProviderError> {
        Self::rebuild_from_graph(self, graph)
    }
    fn on_commit_applied(&self, generation: u64) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        runtime.live_typed.clear();
        runtime.live.generation = generation;
        Ok(())
    }
    fn node_candidate_set(
        &self,
        name: &DbString,
        graph: &SeleneGraph,
    ) -> Result<Option<CandidateSet<Node>>, ProviderError> {
        let mut runtime = self.runtime.lock();
        check_generation(&runtime, graph.meta.generation)?;
        if runtime.graph.is_some_and(|id| id != graph.graph_id()) {
            return Err(inconsistent(
                "candidate-state belongs to another graph".into(),
            ));
        }
        if runtime
            .live_typed
            .get(name)
            .is_some_and(|set| set.validate_identity_for(graph).is_ok())
        {
            return Ok(runtime.live_typed.get(name).cloned());
        }
        runtime.live_typed.remove(name);
        let Some(nodes) = runtime
            .live
            .members
            .get_mut(name)
            .map(|members| members.candidate_nodes())
        else {
            return Ok(None);
        };
        let candidates = graph
            .bind_node_candidates(nodes.iter().copied())
            .map_err(provider_graph_error)?;
        if candidates.len() != nodes.len() {
            return Err(inconsistent(format!(
                "candidate set {name} contains nodes not live in the supplied graph snapshot"
            )));
        }
        runtime.live_typed.insert(name.clone(), candidates.clone());
        Ok(Some(candidates))
    }
    fn vector_candidate_set(
        &self,
        name: &DbString,
        generation: u64,
    ) -> Result<Option<VectorCandidateSet>, ProviderError> {
        self.candidate_set_at_generation(name, generation)
    }
    fn vector_candidate_state_infos(
        &self,
        generation: u64,
    ) -> Result<Vec<VectorCandidateStateInfo>, ProviderError> {
        self.candidate_state_infos_at_generation(generation)
    }
}
fn check_generation(runtime: &CandidateStateRuntime, generation: u64) -> Result<(), ProviderError> {
    if runtime.live.generation != generation {
        return Err(inconsistent(format!(
            "candidate-state generation {} does not match graph generation {generation}",
            runtime.live.generation
        )));
    }
    Ok(())
}
fn provider_graph_error(error: crate::GraphError) -> ProviderError {
    inconsistent(format!("graph candidate binding failed: {error}"))
}
#[cfg(test)]
mod tests;
