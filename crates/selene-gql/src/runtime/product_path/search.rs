//! Iterative history-sensitive DFS; never merges graph/automaton positions.

use super::super::{
    Binding, BindingTable, ExecutorError,
    batch::{budget::MemoryBudget, operator::BatchExecutionContext},
    edge_access, scan,
};
use super::{
    BoundedPathProgram, PathExecutionLimits, PathExecutionStats, PathObservation,
    compile::invalid,
    state::{SearchState, hidden},
};
use crate::{EdgeQuantifierKind, EdgeTest, NodeTest, PathSemanticElement};
use selene_core::Value;
use std::mem::size_of;

pub(super) struct SearchResult {
    pub(super) table: BindingTable,
    pub(super) stats: PathExecutionStats,
    pub(super) observations: Vec<PathObservation>,
    pub(super) reserved: usize,
}

pub(super) fn execute(
    program: &BoundedPathProgram<'_>,
    limits: PathExecutionLimits,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<SearchResult, ExecutorError> {
    ctx.ensure_generation()?;
    ctx.check_cancel(program.paths[0].automaton.origin)?;
    for path in &program.paths {
        if path.upper > limits.max_hops {
            return Err(limit("max_path_hops", path.automaton.origin));
        }
    }
    // A conservative capacity envelope per state/observation/result, including
    // Vec growth slack and all edge-list payloads. It bounds this executor's
    // retained storage; it is deliberately not an allocator/RSS measurement.
    let hops = program
        .paths
        .iter()
        .try_fold(0usize, |sum, p| sum.checked_add(p.upper as usize))
        .ok_or_else(|| invalid("product path memory estimate overflow"))?;
    let elements: usize = program
        .paths
        .iter()
        .map(|p| p.automaton.semantic.elements.len())
        .sum();
    let state_bytes = hops
        .checked_add(elements)
        .and_then(|n| n.checked_add(program.bindings.len()))
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(size_of::<Value>() * 16))
        .ok_or_else(|| invalid("product path memory estimate overflow"))?;
    let budget = *ctx.budget_mut();
    let mut run = Search {
        program,
        limits,
        ctx,
        budget,
        reserved: 0,
        state_bytes,
        stats: PathExecutionStats::default(),
        observations: Vec::new(),
        stack: Vec::new(),
        rows: Vec::new(),
    };
    let outcome = run.run();
    if outcome.is_err() {
        run.budget.release(run.reserved);
        run.reserved = 0;
    }
    let Search {
        budget,
        stats,
        observations,
        rows,
        reserved,
        ..
    } = run;
    *ctx.budget_mut() = budget;
    outcome?;
    Ok(SearchResult {
        table: BindingTable::new(program.schema.clone(), rows),
        stats,
        observations,
        reserved,
    })
}

struct Search<'a, 'p, 'g> {
    program: &'a BoundedPathProgram<'p>,
    limits: PathExecutionLimits,
    ctx: &'a BatchExecutionContext<'g>,
    budget: MemoryBudget,
    reserved: usize,
    state_bytes: usize,
    stats: PathExecutionStats,
    observations: Vec<PathObservation>,
    stack: Vec<SearchState>,
    rows: Vec<Binding>,
}

impl Search<'_, '_, '_> {
    fn reserve(&mut self, bytes: usize) -> Result<(), ExecutorError> {
        let next = self
            .reserved
            .checked_add(bytes)
            .ok_or_else(|| invalid("product path memory estimate overflow"))?;
        if next > self.limits.max_bytes {
            return Err(self.limit("max_path_bytes"));
        }
        self.budget
            .reserve(bytes)
            .map_err(|e| e.into_executor_error(self.span()))?;
        self.reserved = next;
        self.stats.peak_bytes = self.stats.peak_bytes.max(next);
        self.stats.reservations += 1;
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.reserved -= bytes;
        self.budget.release(bytes);
    }
    fn span(&self) -> crate::SourceSpan {
        self.program.paths[0].automaton.origin
    }
    fn limit(&self, name: &'static str) -> ExecutorError {
        limit(name, self.span())
    }

    fn work(&self) -> Result<(), ExecutorError> {
        self.ctx.check_cancel(self.span())?;
        if self
            .stats
            .product_states
            .saturating_add(self.stats.incidences)
            >= self.limits.max_work
        {
            return Err(self.limit("max_path_work"));
        }
        Ok(())
    }

    fn push(&mut self, state: &SearchState) -> Result<(), ExecutorError> {
        self.reserve(self.state_bytes)?;
        self.stack.push(state.clone());
        Ok(())
    }

    fn run(&mut self) -> Result<(), ExecutorError> {
        // Covers the single live scratch state and length histogram, in addition
        // to charged stack clones. Charge before any execution-sized allocation.
        self.reserve(self.state_bytes)?;
        let max_hops = self
            .program
            .paths
            .iter()
            .map(|p| p.upper)
            .max()
            .unwrap_or(0) as usize;
        self.stats.hop_lengths = vec![0; max_hops + 1];
        self.push(&SearchState::new(self.program.bindings.len()))?;
        while let Some(mut state) = self.stack.pop() {
            self.release(self.state_bytes);
            self.work()?;
            self.stats.product_states += 1;
            let path = &self.program.paths[state.pattern];
            if let Some(choice) = state.choice.take() {
                self.observe(&state, choice)?;
            }
            if state.element == path.automaton.semantic.elements.len() {
                if state.pattern + 1 == self.program.paths.len() {
                    self.emit(&state)?;
                } else {
                    state.clause_edges.append(&mut state.edges);
                    state.nodes.clear();
                    state.current = None;
                    state.element = 0;
                    state.pattern += 1;
                    self.push(&state)?;
                }
                continue;
            }
            match &path.automaton.semantic.elements[state.element] {
                PathSemanticElement::Node(node) => self.node(state, node)?,
                PathSemanticElement::Edge(edge) => self.edge(state, edge)?,
            }
        }
        self.release(self.state_bytes);
        Ok(())
    }

    fn node(&mut self, mut state: SearchState, test: &NodeTest) -> Result<(), ExecutorError> {
        if let Some(node) = state.current {
            if self
                .ctx
                .snapshot()?
                .node_labels(node)
                .is_some_and(|labels| {
                    test.label
                        .as_ref()
                        .is_none_or(|label| scan::label_matches_node(label, labels))
                })
                && state.bind(
                    self.program,
                    test.binding,
                    test.temporary.map(|t| t.slot),
                    Value::NodeRef(node),
                )
            {
                state.element += 1;
                self.push(&state)?;
            }
            return Ok(());
        }
        // Stable typed candidates; no row-id arithmetic or global visited set.
        let count = self.ctx.snapshot()?.node_count();
        self.ctx.note_nodes_scanned(count, test.origin)?;
        if self
            .stats
            .product_states
            .saturating_add(self.stats.incidences)
            .saturating_add(count as u64)
            > self.limits.max_work
        {
            return Err(self.limit("max_path_work"));
        }
        let candidate_bytes = count
            .checked_mul(64)
            .ok_or_else(|| invalid("product path candidate estimate overflow"))?;
        self.reserve(candidate_bytes)?;
        let candidates = self
            .ctx
            .snapshot()?
            .live_node_candidates()
            .map_err(|_| invalid("product path node candidates unavailable"))?;
        let start = self.stack.len();
        for node in candidates.iter() {
            self.work()?;
            self.stats.incidences += 1;
            if !self
                .ctx
                .snapshot()?
                .node_labels(node)
                .is_some_and(|labels| {
                    test.label
                        .as_ref()
                        .is_none_or(|label| scan::label_matches_node(label, labels))
                })
            {
                continue;
            }
            self.reserve(self.state_bytes)?;
            let mut next = state.clone();
            if next.bind(
                self.program,
                test.binding,
                test.temporary.map(|t| t.slot),
                Value::NodeRef(node),
            ) {
                next.current = Some(node);
                next.nodes.push(node);
                next.element += 1;
                self.stats.hop_lengths[0] += 1;
                self.stack.push(next);
            } else {
                self.release(self.state_bytes);
            }
        }
        self.stack[start..].reverse();
        self.release(candidate_bytes);
        Ok(())
    }

    fn edge(&mut self, mut state: SearchState, test: &EdgeTest) -> Result<(), ExecutorError> {
        let (min, max) = match test.quantifier {
            EdgeQuantifierKind::Single => (1, 1),
            EdgeQuantifierKind::Questioned => (0, 1),
            EdgeQuantifierKind::Bounded { min, max } => (min, max),
            EdgeQuantifierKind::Unbounded { .. } => {
                return Err(invalid("unvalidated open path bound"));
            }
        };
        if state.depth < max {
            let current = state
                .current
                .expect("validated alternating shape has a source node");
            let start = self.stack.len();
            let graph = self.ctx.snapshot()?;
            for (choice, adjacent) in
                edge_access::adjacent_edges(graph, current, test.orientation.declared).enumerate()
            {
                self.work()?;
                self.stats.incidences += 1;
                if !graph.edge_label(adjacent.edge_id).is_some_and(|label| {
                    test.label
                        .as_ref()
                        .is_none_or(|test| scan::label_matches_edge(test, label))
                }) || !state.legal(
                    self.program.paths[state.pattern].automaton.mode.mode,
                    self.program.different_edges,
                    adjacent.edge_id,
                    adjacent.neighbor,
                ) {
                    continue;
                }
                self.reserve(self.state_bytes)?;
                let mut next = state.clone();
                next.current = Some(adjacent.neighbor);
                next.edges.push(adjacent.edge_id);
                next.nodes.push(adjacent.neighbor);
                next.depth += 1;
                next.choice = Some((current, adjacent.edge_id, choice));
                self.stack.push(next);
            }
            self.stack[start..].reverse();
        }
        // Exit first, then DFS successors in incidence order. Binding a group
        // happens at transition exit: a reused group compares the WHOLE list,
        // never a prefix. Sibling histories live in independent stack frames.
        if state.depth >= min {
            let value = state.edge_value(test);
            if state.bind(
                self.program,
                test.exposure.named(),
                hidden(test.exposure),
                value,
            ) {
                state.depth = 0;
                state.element += 1;
                self.push(&state)?;
            }
        }
        Ok(())
    }

    fn observe(
        &mut self,
        state: &SearchState,
        (from, edge, choice): (selene_core::NodeId, selene_core::EdgeId, usize),
    ) -> Result<(), ExecutorError> {
        self.stats.hop_lengths[state.edges.len()] += 1;
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

    fn emit(&mut self, state: &SearchState) -> Result<(), ExecutorError> {
        if self.rows.len() >= self.limits.max_rows {
            return Err(self.limit("max_path_rows"));
        }
        // Includes materialized row, batch column copies and tracer result.
        self.reserve(self.state_bytes)?;
        self.rows.push(Binding::new(
            state
                .locals
                .iter()
                .map(|v| v.clone().unwrap_or(Value::Null)),
        ));
        self.stats.matched_rows += 1;
        self.stats.cheapest_projection.candidate_costs += self.program.paths.len() as u64;
        self.stats.cheapest_projection.edge_cost_evaluations +=
            (state.clause_edges.len() + state.edges.len()) as u64;
        Ok(())
    }
}

fn limit(detail: &'static str, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::ProgramLimitExceeded { detail, span }
}
