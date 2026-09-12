//! Complete-binding qualification and opt-in observations, separate from search.

use super::*;

impl Search<'_, '_, '_> {
    fn qualifies(
        &self,
        state: &SearchState,
        entity: &Value,
        phase: Phase,
    ) -> Result<bool, ExecutorError> {
        if let Some(qualify) = self.qualify {
            qualify(state, entity, phase)
        } else if self.program.paths[state.pattern].conditions[state.element].is_empty() {
            Ok(true)
        } else {
            Err(invalid("product path predicate evaluator missing"))
        }
    }

    pub(super) fn qualify_path(&self, state: &mut SearchState) -> Result<bool, ExecutorError> {
        let path = &self.program.paths[state.pattern];
        // Reused path identities are acceptance conditions, not a post-selection
        // filter that could discard the shortest and conceal a longer match.
        if let Some(id) = path.automaton.semantic.path_binding {
            let slot = self
                .program
                .bindings
                .iter()
                .position(|b| *b == id)
                .expect("path binding");
            if let Some(value) = &state.locals[slot] {
                let Value::Path(p) = value else {
                    return Ok(false);
                };
                if p.start != state.nodes[0]
                    || p.segments.len() != state.edges.len()
                    || p.segments.iter().enumerate().any(|(i, s)| {
                        s.edge != state.edges[i]
                            || s.node != state.nodes[i + 1]
                            || s.direction != state.directions[i]
                    })
                {
                    return Ok(false);
                }
            }
        }
        for (index, element) in path.automaton.semantic.elements.iter().enumerate() {
            state.element = index;
            match element {
                PathSemanticElement::Node(_) => {
                    let entity = Value::NodeRef(state.nodes[state.element_ends[index]]);
                    if !self.qualifies(state, &entity, Phase::Node)? {
                        return Ok(false);
                    }
                }
                PathSemanticElement::Edge(test) => {
                    let start = state.element_ends[index - 1];
                    let end = state.element_ends[index];
                    for &edge in &state.edges[start..end] {
                        if !self.qualifies(state, &Value::EdgeRef(edge), Phase::EdgeHop)? {
                            return Ok(false);
                        }
                    }
                    if (end > start || test.exposure.is_group())
                        && !self.qualifies(state, &Value::Null, Phase::EdgeExit)?
                    {
                        return Ok(false);
                    }
                }
            }
        }
        state.element = path.automaton.semantic.elements.len();
        Ok(true)
    }

    pub(super) fn observe(
        &mut self,
        state: &SearchState,
        (from, edge, choice): (selene_core::NodeId, selene_core::EdgeId, usize),
    ) -> Result<(), ExecutorError> {
        self.stats.hop_lengths[state.edges.len()] += 1;
        self.stats.peak_history_bytes = self
            .stats
            .peak_history_bytes
            .max((self.stack.len() + 1) * self.state_bytes);
        if !self.limits.observe {
            return Ok(());
        }
        if self.observations.len() >= self.limits.max_observations {
            return Err(self.limit("max_path_observations"));
        }
        self.reserve(self.state_bytes)?;
        let path = &self.program.paths[state.pattern];
        let PathSemanticElement::Edge(test) = &path.automaton.semantic.elements[state.element]
        else {
            unreachable!("hop targets edge transition")
        };
        let mut locals: Vec<_> = self
            .program
            .bindings
            .iter()
            .copied()
            .zip(&state.locals)
            .filter_map(|(id, value)| value.clone().map(|v| (id, v)))
            .collect();
        let value = state.edge_value(test);
        let mut temporaries = state.temporaries.clone();
        if let Some(id) = test.exposure.named() {
            locals.retain(|(bound, _)| *bound != id);
            locals.push((id, value));
        } else if let Some(slot) = hidden(test.exposure) {
            temporaries.push((state.pattern, slot, value));
        }
        self.observations.push(PathObservation {
            pattern: state.pattern,
            transition: path.transitions[state.element],
            mode: path.automaton.mode,
            choice,
            from,
            to: state.current.expect("hop target"),
            edge,
            hops: state.edges.len(),
            repetition: state.depth,
            locals,
            temporaries,
        });
        Ok(())
    }
}
