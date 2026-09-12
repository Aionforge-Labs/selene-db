//! Internal state and snapshot helpers for maintained candidate sets.

use std::collections::{BTreeMap, BTreeSet};

use super::CandidateStateSpec;
use crate::index_provider::ProviderError;
use selene_core::{Change, DbString, EdgeId, LabelSet, NodeId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TrackedEdge {
    pub(super) label: DbString,
    pub(super) source: NodeId,
    pub(super) target: NodeId,
}

#[derive(Clone, Debug)]
pub(super) struct CandidateState {
    pub(super) generation: u64,
    pub(super) node_labels: BTreeMap<NodeId, LabelSet>,
    pub(super) edges: BTreeMap<EdgeId, TrackedEdge>,
    outgoing_counts: BTreeMap<(NodeId, DbString), usize>,
    incoming_counts: BTreeMap<(NodeId, DbString), usize>,
    pub(super) members: BTreeMap<DbString, CandidateMembers>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CandidateMembers {
    set: BTreeSet<NodeId>,
    cached: Vec<NodeId>,
    dirty: bool,
}

impl CandidateMembers {
    pub(super) fn candidate_nodes(&mut self) -> Vec<NodeId> {
        if self.dirty {
            self.cached.clear();
            self.cached.extend(self.set.iter().copied());
            self.dirty = false;
        }
        self.cached.clone()
    }

    pub(super) fn len(&self) -> usize {
        self.set.len()
    }

    pub(super) fn contains(&self, node: NodeId) -> bool {
        self.set.contains(&node)
    }

    fn set_membership(&mut self, node: NodeId, include: bool) {
        let changed = if include {
            self.set.insert(node)
        } else {
            self.set.remove(&node)
        };
        self.dirty |= changed;
    }
}

impl CandidateState {
    pub(super) fn new(specs: &[CandidateStateSpec]) -> Self {
        Self {
            generation: 0,
            node_labels: BTreeMap::new(),
            edges: BTreeMap::new(),
            outgoing_counts: BTreeMap::new(),
            incoming_counts: BTreeMap::new(),
            members: empty_members(specs),
        }
    }

    pub(super) fn apply_change(
        &mut self,
        specs: &[CandidateStateSpec],
        change: &Change,
    ) -> Result<(), ProviderError> {
        match change {
            Change::NodeCreated { id, labels, .. } => {
                if self.node_labels.insert(*id, labels.clone()).is_some() {
                    return Err(inconsistent(format!("duplicate node create for {id}")));
                }
                self.recompute_node(specs, *id);
            }
            Change::NodeUpdated {
                id, labels_diff, ..
            } => {
                let labels = self
                    .node_labels
                    .get_mut(id)
                    .ok_or_else(|| inconsistent(format!("label update for unknown node {id}")))?;
                for label in &labels_diff.removed {
                    labels.remove(label);
                }
                for label in &labels_diff.added {
                    labels.insert(label.clone());
                }
                self.recompute_node(specs, *id);
            }
            Change::NodeDeleted { id } => {
                self.node_labels.remove(id);
                self.remove_incident_edges(specs, *id);
                self.recompute_node(specs, *id);
            }
            Change::NodeLabelRemoved { id, label } => {
                let labels = self
                    .node_labels
                    .get_mut(id)
                    .ok_or_else(|| inconsistent(format!("label removal for unknown node {id}")))?;
                labels.remove(label);
                self.recompute_node(specs, *id);
            }
            Change::EdgeCreated {
                id,
                label,
                source,
                target,
                directionality,
                ..
            } => {
                // These predicates require directed incoming/outgoing edges;
                // canonical undirected endpoint order must not satisfy either.
                if *directionality == selene_core::EdgeDirectionality::Directed
                    && watches_label(specs, label)
                {
                    let edge = TrackedEdge {
                        label: label.clone(),
                        source: *source,
                        target: *target,
                    };
                    if self.edges.insert(*id, edge.clone()).is_some() {
                        return Err(inconsistent(format!("duplicate edge create for {id}")));
                    }
                    self.increment_edge(&edge);
                    self.recompute_node(specs, *source);
                    self.recompute_node(specs, *target);
                }
            }
            Change::EdgeDeleted { id } => {
                if let Some(edge) = self.edges.remove(id) {
                    self.decrement_edge(&edge);
                    self.recompute_node(specs, edge.source);
                    self.recompute_node(specs, edge.target);
                }
            }
            Change::GraphReset {} => {
                *self = Self::new(specs);
            }
            Change::NodesOfTypeTruncated { label } => {
                let removed = self
                    .node_labels
                    .iter()
                    .filter_map(|(id, labels)| labels.contains(label).then_some(*id))
                    .collect::<BTreeSet<_>>();
                if !removed.is_empty() {
                    self.node_labels.retain(|id, _| !removed.contains(id));
                    self.edges.retain(|_, edge| {
                        !removed.contains(&edge.source) && !removed.contains(&edge.target)
                    });
                    self.rebuild_derived(specs);
                }
            }
            Change::EdgesOfTypeTruncated { label } => {
                if watches_label(specs, label) {
                    self.edges.retain(|_, edge| edge.label != *label);
                    self.rebuild_derived(specs);
                }
            }
            Change::EdgeUpdated { .. }
            | Change::EdgePropertyRemoved { .. }
            | Change::NodePropertyRemoved { .. }
            | Change::SchemaChanged { .. } => {}
        }
        Ok(())
    }

    pub(super) fn rebuild_derived(&mut self, specs: &[CandidateStateSpec]) {
        self.outgoing_counts.clear();
        self.incoming_counts.clear();
        self.members = empty_members(specs);
        for edge in self.edges.values().cloned().collect::<Vec<_>>() {
            self.increment_edge(&edge);
        }
        for id in self.node_labels.keys().copied().collect::<Vec<_>>() {
            self.recompute_node(specs, id);
        }
    }

    fn increment_edge(&mut self, edge: &TrackedEdge) {
        *self
            .outgoing_counts
            .entry((edge.source, edge.label.clone()))
            .or_insert(0) += 1;
        *self
            .incoming_counts
            .entry((edge.target, edge.label.clone()))
            .or_insert(0) += 1;
    }

    fn decrement_edge(&mut self, edge: &TrackedEdge) {
        decrement_count(&mut self.outgoing_counts, (edge.source, edge.label.clone()));
        decrement_count(&mut self.incoming_counts, (edge.target, edge.label.clone()));
    }

    fn remove_incident_edges(&mut self, specs: &[CandidateStateSpec], node: NodeId) {
        let incident = self
            .edges
            .iter()
            .filter_map(|(id, edge)| {
                (edge.source == node || edge.target == node).then_some((*id, edge.clone()))
            })
            .collect::<Vec<_>>();
        for (id, edge) in incident {
            self.edges.remove(&id);
            self.decrement_edge(&edge);
            if edge.source != node {
                self.recompute_node(specs, edge.source);
            }
            if edge.target != node {
                self.recompute_node(specs, edge.target);
            }
        }
    }

    fn recompute_node(&mut self, specs: &[CandidateStateSpec], node: NodeId) {
        for spec in specs {
            let include = self.node_labels.get(&node).is_some_and(|labels| {
                spec.required_label
                    .as_ref()
                    .is_none_or(|required| labels.contains(required))
                    && spec
                        .require_outgoing
                        .iter()
                        .all(|label| has_count(&self.outgoing_counts, node, label))
                    && spec
                        .require_incoming
                        .iter()
                        .all(|label| has_count(&self.incoming_counts, node, label))
                    && spec
                        .exclude_outgoing
                        .iter()
                        .all(|label| !has_count(&self.outgoing_counts, node, label))
                    && spec
                        .exclude_incoming
                        .iter()
                        .all(|label| !has_count(&self.incoming_counts, node, label))
            });
            let members = self.members.entry(spec.name.clone()).or_default();
            members.set_membership(node, include);
        }
    }
}

pub(super) fn validate_unique_specs(specs: &[CandidateStateSpec]) -> Result<(), ProviderError> {
    let mut seen = BTreeSet::new();
    for spec in specs {
        if !seen.insert(spec.name.clone()) {
            return Err(inconsistent(format!(
                "duplicate candidate-state spec name {}",
                spec.name.as_str()
            )));
        }
    }
    Ok(())
}

fn empty_members(specs: &[CandidateStateSpec]) -> BTreeMap<DbString, CandidateMembers> {
    specs
        .iter()
        .map(|spec| (spec.name.clone(), CandidateMembers::default()))
        .collect()
}

pub(super) fn watches_label(specs: &[CandidateStateSpec], label: &DbString) -> bool {
    specs.iter().any(|spec| {
        spec.require_outgoing.binary_search(label).is_ok()
            || spec.require_incoming.binary_search(label).is_ok()
            || spec.exclude_outgoing.binary_search(label).is_ok()
            || spec.exclude_incoming.binary_search(label).is_ok()
    })
}

fn has_count(counts: &BTreeMap<(NodeId, DbString), usize>, node: NodeId, label: &DbString) -> bool {
    counts
        .get(&(node, label.clone()))
        .is_some_and(|count| *count > 0)
}

fn decrement_count(counts: &mut BTreeMap<(NodeId, DbString), usize>, key: (NodeId, DbString)) {
    if let Some(count) = counts.get_mut(&key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counts.remove(&key);
        }
    }
}

pub(super) fn insert_sorted_unique(labels: &mut Vec<DbString>, label: DbString) {
    match labels.binary_search(&label) {
        Ok(_) => {}
        Err(index) => labels.insert(index, label),
    }
}

pub(super) fn canonicalize_labels(labels: &mut Vec<DbString>) {
    labels.sort_unstable();
    labels.dedup();
}

pub(super) fn inconsistent(reason: String) -> ProviderError {
    ProviderError::Inconsistent { reason }
}
