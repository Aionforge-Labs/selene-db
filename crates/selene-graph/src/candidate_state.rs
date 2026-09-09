//! Maintained graph-derived candidate sets.
//!
//! This module owns small, policy-neutral maintained node sets for graph/vector
//! retrieval. A set can require node labels, require incoming/outgoing edge
//! evidence, and exclude nodes that have disqualifying incoming/outgoing edges.
//! That is enough to model active/current/unresolved memory subsets without
//! hard-coding those application labels into the engine.

use std::collections::BTreeMap;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use selene_core::{Change, DbString, NodeId};
#[cfg(test)]
use selene_core::{EdgeId, LabelSet};

use crate::index_provider::{
    IndexProvider, ProviderError, ProviderTag, SubTag, VectorCandidateStateInfo,
};
use crate::{CandidateSet, Node, SeleneGraph, VectorCandidateSet};

#[path = "candidate_state/state.rs"]
mod state;

use state::{
    CandidateState, CandidateStateSnapshot, TrackedEdge, canonicalize_labels, ensure_state_subtag,
    inconsistent, insert_sorted_unique, invalid_payload, validate_unique_specs, watches_label,
};

/// Provider tag for maintained graph candidate-state sections.
pub const CANDIDATE_STATE_PROVIDER_TAG: [u8; 4] = *b"CSET";

/// Provider-owned snapshot section for maintained candidate-state data.
pub const CANDIDATE_STATE_SUB: [u8; 4] = *b"STAT";

const SNAPSHOT_VERSION: u8 = 1;
const SUB_TAGS: &[SubTag] = &[SubTag(CANDIDATE_STATE_SUB)];

/// Declarative rule for one maintained candidate set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CandidateStateSpec {
    /// Stable set name used by callers to retrieve candidates.
    pub name: DbString,
    /// Optional node label required for membership.
    pub required_label: Option<DbString>,
    /// Outgoing edge labels required on the source node.
    pub require_outgoing: Vec<DbString>,
    /// Incoming edge labels required on the target node.
    pub require_incoming: Vec<DbString>,
    /// Outgoing edge labels that disqualify the source node.
    pub exclude_outgoing: Vec<DbString>,
    /// Incoming edge labels that disqualify the target node.
    pub exclude_incoming: Vec<DbString>,
}

impl CandidateStateSpec {
    /// Construct an unconstrained named candidate set.
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

    /// Require `label` for candidate membership.
    #[must_use]
    pub fn require_label(mut self, label: DbString) -> Self {
        self.required_label = Some(label);
        self
    }

    /// Require an outgoing edge carrying `label`.
    #[must_use]
    pub fn require_outgoing(mut self, label: DbString) -> Self {
        insert_sorted_unique(&mut self.require_outgoing, label);
        self
    }

    /// Require an incoming edge carrying `label`.
    #[must_use]
    pub fn require_incoming(mut self, label: DbString) -> Self {
        insert_sorted_unique(&mut self.require_incoming, label);
        self
    }

    /// Exclude nodes with an outgoing edge carrying `label`.
    #[must_use]
    pub fn exclude_outgoing(mut self, label: DbString) -> Self {
        insert_sorted_unique(&mut self.exclude_outgoing, label);
        self
    }

    /// Exclude nodes with an incoming edge carrying `label`.
    #[must_use]
    pub fn exclude_incoming(mut self, label: DbString) -> Self {
        insert_sorted_unique(&mut self.exclude_incoming, label);
        self
    }
}

/// First-party provider maintaining named graph-derived candidate sets.
pub struct MaintainedCandidateStateProvider {
    specs: Vec<CandidateStateSpec>,
    runtime: Mutex<CandidateStateRuntime>,
}

struct CandidateStateRuntime {
    live: CandidateState,
    live_typed: BTreeMap<DbString, CandidateSet<Node>>,
    recovery: Option<RecoveryCandidateState>,
}

struct StagedCandidateState {
    state: CandidateState,
    typed: BTreeMap<DbString, CandidateSet<Node>>,
    prepared: bool,
}

enum RecoveryCandidateState {
    Staged(StagedCandidateState),
    Promoted {
        prior_state: CandidateState,
        prior_typed: BTreeMap<DbString, CandidateSet<Node>>,
    },
}

impl RecoveryCandidateState {
    fn staged_mut(&mut self) -> Result<&mut StagedCandidateState, ProviderError> {
        match self {
            Self::Staged(staged) => Ok(staged),
            Self::Promoted { .. } => Err(inconsistent(
                "candidate-state recovery attachment is already promoted".to_owned(),
            )),
        }
    }
}

impl MaintainedCandidateStateProvider {
    /// Construct an empty provider for `specs`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when two specs use the same name.
    pub fn new(specs: impl IntoIterator<Item = CandidateStateSpec>) -> Result<Self, ProviderError> {
        let mut specs = specs.into_iter().collect::<Vec<_>>();
        for spec in &mut specs {
            canonicalize_labels(&mut spec.require_outgoing);
            canonicalize_labels(&mut spec.require_incoming);
            canonicalize_labels(&mut spec.exclude_outgoing);
            canonicalize_labels(&mut spec.exclude_incoming);
        }
        validate_unique_specs(&specs)?;
        Ok(Self {
            runtime: Mutex::new(CandidateStateRuntime {
                live: CandidateState::new(&specs),
                live_typed: BTreeMap::new(),
                recovery: None,
            }),
            specs,
        })
    }

    /// Construct a provider and initialize it from a graph snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when specs are invalid or the graph snapshot is
    /// internally inconsistent.
    pub fn from_graph(
        specs: impl IntoIterator<Item = CandidateStateSpec>,
        graph: &SeleneGraph,
    ) -> Result<Self, ProviderError> {
        let provider = Self::new(specs)?;
        provider.rebuild_from_graph(graph)?;
        Ok(provider)
    }

    /// Rebuild all maintained state from `graph`.
    ///
    /// This is the safe attachment path when a provider is registered against an
    /// already-populated graph instead of observing mutations from graph birth.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if live row-to-id mappings are inconsistent.
    pub fn rebuild_from_graph(&self, graph: &SeleneGraph) -> Result<(), ProviderError> {
        let rebuilt = self.build_state_from_graph(graph)?;
        let mut runtime = self.runtime.lock();
        if let Some(recovery) = runtime.recovery.as_mut() {
            let staged = recovery.staged_mut()?;
            staged.state = rebuilt;
            staged.typed.clear();
            staged.prepared = false;
        } else {
            runtime.live = rebuilt;
            runtime.live_typed.clear();
        }
        Ok(())
    }

    fn build_state_from_graph(&self, graph: &SeleneGraph) -> Result<CandidateState, ProviderError> {
        let mut rebuilt = CandidateState::new(&self.specs);
        let nodes = graph.live_node_candidates().map_err(provider_graph_error)?;
        for id in nodes.iter() {
            let labels = graph
                .node_labels(id)
                .ok_or_else(|| inconsistent(format!("live node {id} has no label column entry")))?;
            rebuilt.node_labels.insert(id, labels.clone());
        }
        let edges = graph.live_edge_candidates().map_err(provider_graph_error)?;
        for id in edges.iter() {
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
        Ok(rebuilt)
    }

    /// Return the configured spec named `name`.
    #[must_use]
    pub fn spec(&self, name: &DbString) -> Option<&CandidateStateSpec> {
        self.specs.iter().find(|spec| &spec.name == name)
    }

    /// Return the current candidate set for `name`.
    #[must_use]
    pub fn candidate_set(&self, name: &DbString) -> Option<VectorCandidateSet> {
        let mut runtime = self.runtime.lock();
        runtime
            .live
            .members
            .get_mut(name)
            .map(|members| VectorCandidateSet::from_canonical_nodes(members.candidate_nodes()))
    }

    /// Return the provider generation watermark.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.runtime.lock().live.generation
    }

    /// Return the current candidate set for `name` if it matches `generation`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when this provider has not applied every
    /// mutation through `generation`.
    pub fn candidate_set_at_generation(
        &self,
        name: &DbString,
        generation: u64,
    ) -> Result<Option<VectorCandidateSet>, ProviderError> {
        let mut runtime = self.runtime.lock();
        if runtime.live.generation != generation {
            return Err(inconsistent(format!(
                "candidate-state generation {} does not match graph generation {generation}",
                runtime.live.generation
            )));
        }
        Ok(runtime
            .live
            .members
            .get_mut(name)
            .map(|members| VectorCandidateSet::from_canonical_nodes(members.candidate_nodes())))
    }

    /// Return generation-checked metadata for every configured candidate set.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when this provider has not applied every
    /// mutation through `generation`.
    pub fn candidate_state_infos_at_generation(
        &self,
        generation: u64,
    ) -> Result<Vec<VectorCandidateStateInfo>, ProviderError> {
        let runtime = self.runtime.lock();
        if runtime.live.generation != generation {
            return Err(inconsistent(format!(
                "candidate-state generation {} does not match graph generation {generation}",
                runtime.live.generation
            )));
        }
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

    /// Return true when `node` is currently a member of the named set.
    #[must_use]
    pub fn contains(&self, name: &DbString, node: NodeId) -> bool {
        self.runtime
            .lock()
            .live
            .members
            .get(name)
            .is_some_and(|members| members.contains(node))
    }

    fn encode_section(
        &self,
        sub_tag: SubTag,
        expected_generation: Option<u64>,
    ) -> Result<Vec<u8>, ProviderError> {
        ensure_state_subtag(sub_tag)?;
        let runtime = self.runtime.lock();
        if let Some(generation) = expected_generation
            && runtime.live.generation != generation
        {
            return Err(inconsistent(format!(
                "candidate-state generation {} does not match checkpoint generation {generation}",
                runtime.live.generation
            )));
        }
        let state = &runtime.live;
        let snapshot = CandidateStateSnapshot {
            version: SNAPSHOT_VERSION,
            generation: state.generation,
            specs: self.specs.clone(),
            node_labels: state
                .node_labels
                .iter()
                .map(|(id, labels)| (*id, labels.clone()))
                .collect(),
            edges: state
                .edges
                .iter()
                .map(|(id, edge)| (*id, edge.clone()))
                .collect(),
        };
        postcard::to_stdvec(&snapshot).map_err(|error| ProviderError::SerializationFailed {
            reason: format!("CSET/STAT postcard encode failed: {error}"),
        })
    }
}

impl IndexProvider for MaintainedCandidateStateProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(CANDIDATE_STATE_PROVIDER_TAG)
    }

    fn read_section(&self, sub_tag: SubTag, bytes: &[u8]) -> Result<(), ProviderError> {
        ensure_state_subtag(sub_tag)?;
        let snapshot: CandidateStateSnapshot = postcard::from_bytes(bytes).map_err(|error| {
            invalid_payload(format!("CSET/STAT postcard decode failed: {error}"))
        })?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(invalid_payload(format!(
                "unsupported CSET/STAT version {}",
                snapshot.version
            )));
        }
        if snapshot.specs != self.specs {
            return Err(invalid_payload(
                "CSET/STAT specs differ from provider configuration".to_owned(),
            ));
        }
        let mut state = CandidateState::new(&self.specs);
        state.generation = snapshot.generation;
        for (id, labels) in snapshot.node_labels {
            if state.node_labels.insert(id, labels).is_some() {
                return Err(invalid_payload(format!(
                    "duplicate node id {id} in CSET/STAT"
                )));
            }
        }
        for (id, edge) in snapshot.edges {
            if !watches_label(&self.specs, &edge.label) {
                return Err(invalid_payload(format!(
                    "unwatched edge label {} in CSET/STAT",
                    edge.label.as_str()
                )));
            }
            if !state.node_labels.contains_key(&edge.source)
                || !state.node_labels.contains_key(&edge.target)
            {
                return Err(invalid_payload(format!(
                    "tracked edge {id} references missing endpoint in CSET/STAT"
                )));
            }
            if state.edges.insert(id, edge).is_some() {
                return Err(invalid_payload(format!(
                    "duplicate edge id {id} in CSET/STAT"
                )));
            }
        }
        state.rebuild_derived(&self.specs);
        let mut runtime = self.runtime.lock();
        if let Some(recovery) = runtime.recovery.as_mut() {
            let staged = recovery.staged_mut()?;
            staged.state = state;
            staged.typed.clear();
            staged.prepared = false;
        } else {
            runtime.live = state;
            runtime.live_typed.clear();
        }
        Ok(())
    }

    fn write_section(&self, sub_tag: SubTag) -> Result<Vec<u8>, ProviderError> {
        self.encode_section(sub_tag, None)
    }

    fn write_section_at_generation(
        &self,
        sub_tag: SubTag,
        generation: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        self.encode_section(sub_tag, Some(generation))
    }

    fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        if let Some(recovery) = runtime.recovery.as_mut() {
            let staged = recovery.staged_mut()?;
            staged.typed.clear();
            staged.prepared = false;
            staged.state.apply_change(&self.specs, change)
        } else {
            runtime.live_typed.clear();
            runtime.live.apply_change(&self.specs, change)
        }
    }

    fn handles_change_batches(&self) -> bool {
        true
    }

    fn on_changes(&self, changes: &[Change]) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        if let Some(recovery) = runtime.recovery.as_mut() {
            let staged = recovery.staged_mut()?;
            staged.typed.clear();
            staged.prepared = false;
            for change in changes {
                staged.state.apply_change(&self.specs, change)?;
            }
        } else {
            runtime.live_typed.clear();
            for change in changes {
                runtime.live.apply_change(&self.specs, change)?;
            }
        }
        Ok(())
    }

    fn rebuild_from_graph(&self, graph: &SeleneGraph) -> Result<(), ProviderError> {
        MaintainedCandidateStateProvider::rebuild_from_graph(self, graph)
    }

    fn on_commit_applied(&self, generation: u64) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        if let Some(recovery) = runtime.recovery.as_mut() {
            let staged = recovery.staged_mut()?;
            staged.typed.clear();
            staged.prepared = false;
            staged.state.generation = generation;
        } else {
            runtime.live_typed.clear();
            runtime.live.generation = generation;
        }
        Ok(())
    }

    fn reserve_recovery_attachment(&self) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        if runtime.recovery.is_some() {
            return Err(inconsistent(
                "candidate-state recovery attachment is already reserved".to_owned(),
            ));
        }
        runtime.recovery = Some(RecoveryCandidateState::Staged(StagedCandidateState {
            state: CandidateState::new(&self.specs),
            typed: BTreeMap::new(),
            prepared: false,
        }));
        Ok(())
    }

    fn prepare_recovery_attachment(&self, graph: &SeleneGraph) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        let recovery = runtime.recovery.as_mut().ok_or_else(|| {
            inconsistent("candidate-state recovery attachment was not reserved".to_owned())
        })?;
        let recovery = recovery.staged_mut()?;
        recovery.typed.clear();
        recovery.prepared = false;
        if recovery.state.generation != graph.meta.generation {
            return Err(inconsistent(format!(
                "staged candidate-state generation {} does not match recovered graph generation {}",
                recovery.state.generation, graph.meta.generation
            )));
        }
        for spec in &self.specs {
            let nodes = recovery
                .state
                .members
                .get_mut(&spec.name)
                .ok_or_else(|| {
                    inconsistent(format!(
                        "staged candidate state has no members for configured set {}",
                        spec.name
                    ))
                })?
                .candidate_nodes();
            let candidates = graph
                .bind_node_candidates(nodes.iter().copied())
                .map_err(provider_graph_error)?;
            if candidates.len() != nodes.len() {
                return Err(inconsistent(format!(
                    "staged candidate set {} contains non-live recovered nodes",
                    spec.name
                )));
            }
            recovery.typed.insert(spec.name.clone(), candidates);
        }
        recovery.prepared = true;
        Ok(())
    }

    fn commit_recovery_attachment(&self) -> Result<(), ProviderError> {
        let mut runtime = self.runtime.lock();
        let recovery = runtime.recovery.take().ok_or_else(|| {
            inconsistent("candidate-state recovery attachment was not reserved".to_owned())
        })?;
        let staged = match recovery {
            RecoveryCandidateState::Staged(staged) => staged,
            promoted @ RecoveryCandidateState::Promoted { .. } => {
                runtime.recovery = Some(promoted);
                return Err(inconsistent(
                    "candidate-state recovery attachment is already promoted".to_owned(),
                ));
            }
        };
        if !staged.prepared {
            runtime.recovery = Some(RecoveryCandidateState::Staged(staged));
            return Err(inconsistent(
                "candidate-state recovery attachment was not prepared".to_owned(),
            ));
        }
        let prior_state = std::mem::replace(&mut runtime.live, staged.state);
        let prior_typed = std::mem::replace(&mut runtime.live_typed, staged.typed);
        runtime.recovery = Some(RecoveryCandidateState::Promoted {
            prior_state,
            prior_typed,
        });
        Ok(())
    }

    fn finalize_recovery_attachment(&self) {
        let mut runtime = self.runtime.lock();
        if matches!(
            runtime.recovery,
            Some(RecoveryCandidateState::Promoted { .. })
        ) {
            runtime.recovery = None;
        }
    }

    fn abort_recovery_attachment(&self) {
        let mut runtime = self.runtime.lock();
        if let Some(RecoveryCandidateState::Promoted {
            prior_state,
            prior_typed,
        }) = runtime.recovery.take()
        {
            runtime.live = prior_state;
            runtime.live_typed = prior_typed;
        }
    }

    fn node_candidate_set(
        &self,
        name: &DbString,
        graph: &SeleneGraph,
    ) -> Result<Option<CandidateSet<Node>>, ProviderError> {
        let mut runtime = self.runtime.lock();
        if runtime.live.generation != graph.meta.generation {
            return Err(inconsistent(format!(
                "candidate-state generation {} does not match graph generation {}",
                runtime.live.generation, graph.meta.generation
            )));
        }
        let cached_is_current = runtime
            .live_typed
            .get(name)
            .is_some_and(|candidates| candidates.validate_identity_for(graph).is_ok());
        if cached_is_current {
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

    fn declared_sub_tags(&self) -> &[SubTag] {
        SUB_TAGS
    }
}

fn provider_graph_error(error: crate::GraphError) -> ProviderError {
    inconsistent(format!("graph candidate binding failed: {error}"))
}

#[cfg(test)]
#[path = "candidate_state/tests.rs"]
mod tests;
