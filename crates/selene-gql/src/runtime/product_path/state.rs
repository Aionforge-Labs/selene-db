//! Product state retains every dimension affecting legality or observable bindings.

use super::compile::BoundedPathProgram;
use crate::{BindingExposure, BindingId, EdgeTest, PathMode};
use selene_core::{EdgeId, NodeId, Value};

#[derive(Clone)]
pub(super) struct SearchState {
    pub(super) pattern: usize,
    pub(super) element: usize,
    pub(super) depth: u32,
    pub(super) current: Option<NodeId>,
    // None is unbound; Some(Null) is a bound questioned skip, not a wildcard.
    pub(super) locals: Vec<Option<Value>>,
    pub(super) temporaries: Vec<(usize, u32, Value)>,
    pub(super) nodes: Vec<NodeId>,
    pub(super) edges: Vec<EdgeId>,
    pub(super) clause_edges: Vec<EdgeId>,
    pub(super) choice: Option<(NodeId, EdgeId, usize)>,
}

impl SearchState {
    pub(super) fn new(width: usize) -> Self {
        Self {
            pattern: 0,
            element: 0,
            depth: 0,
            current: None,
            locals: vec![None; width],
            temporaries: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            clause_edges: Vec::new(),
            choice: None,
        }
    }

    pub(super) fn bind(
        &mut self,
        program: &BoundedPathProgram<'_>,
        binding: Option<BindingId>,
        temporary: Option<u32>,
        value: Value,
    ) -> bool {
        if let Some(binding) = binding {
            let slot = program
                .bindings
                .iter()
                .position(|id| *id == binding)
                .expect("compiled binding");
            if let Some(previous) = &self.locals[slot] {
                return previous == &value;
            }
            self.locals[slot] = Some(value);
        } else if let Some(slot) = temporary {
            self.temporaries.push((self.pattern, slot, value));
        }
        true
    }

    pub(super) fn edge_value(&self, test: &EdgeTest) -> Value {
        let segment = &self.edges[self.edges.len() - self.depth as usize..];
        if test.exposure.is_group() {
            Value::List(segment.iter().copied().map(Value::EdgeRef).collect())
        } else {
            segment.first().copied().map_or(Value::Null, Value::EdgeRef)
        }
    }

    pub(super) fn legal(
        &self,
        mode: PathMode,
        different: bool,
        edge: EdgeId,
        next: NodeId,
    ) -> bool {
        if different && (self.clause_edges.contains(&edge) || self.edges.contains(&edge)) {
            return false;
        }
        match mode {
            PathMode::Walk => true,
            PathMode::Trail => !self.edges.contains(&edge),
            PathMode::Acyclic => !self.nodes.contains(&next),
            PathMode::Simple => {
                // A closed simple prefix may take zero-length transitions but
                // can never take another hop. SIMPLE does not imply TRAIL.
                let closed = self.nodes.len() > 1 && self.nodes.first() == self.nodes.last();
                !closed && (!self.nodes.contains(&next) || self.nodes.first() == Some(&next))
            }
        }
    }
}

pub(super) fn hidden(exposure: BindingExposure) -> Option<u32> {
    match exposure {
        BindingExposure::Singleton { hidden, .. }
        | BindingExposure::ConditionalSingleton { hidden, .. }
        | BindingExposure::GroupList { hidden, .. } => hidden.map(|t| t.slot),
    }
}
