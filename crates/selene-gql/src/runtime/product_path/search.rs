//! Iterative history-sensitive DFS; never merges graph/automaton positions.

use super::super::{
    Binding, BindingTable, ExecutorError,
    batch::{budget::MemoryBudget, operator::BatchExecutionContext},
    edge_access, scan,
};
use super::{
    BoundedPathProgram, PathExecutionLimits, PathExecutionStats, PathObservation,
    compile::invalid,
    conditions::{Phase, Qualifier},
    materialize, selection,
    state::{SearchState, hidden},
    termination,
};
use crate::{EdgeQuantifierKind, EdgeTest, NodeTest, PathSemanticElement};
use selene_core::Value;
use std::collections::VecDeque;
use std::mem::size_of;
use std::time::Instant;

mod setup;
mod support;
pub(super) use setup::execute;

pub(super) struct SearchResult {
    pub(super) table: BindingTable,
    pub(super) stats: PathExecutionStats,
    pub(super) observations: Vec<PathObservation>,
    pub(super) reserved: usize,
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
    stack: VecDeque<SearchState>,
    rows: Vec<Binding>,
    candidates: Vec<SearchState>,
    path_hops: Vec<u32>,
    qualify: Option<&'a Qualifier<'a>>,
    breadth: bool,
    cutoff: bool,
    pairs: Option<termination::Pairs>,
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
            .saturating_add(self.stats.completion_work)
            >= self.limits.max_work
        {
            return Err(self.limit("max_path_work"));
        }
        Ok(())
    }

    fn push(&mut self, state: &SearchState) -> Result<(), ExecutorError> {
        self.reserve(self.state_bytes)?;
        self.stats.history_clones += 1;
        if self.breadth {
            self.stack.push_front(state.clone());
        } else {
            self.stack.push_back(state.clone());
        }
        Ok(())
    }

    fn run(&mut self, seed: Option<Vec<Option<Value>>>) -> Result<(), ExecutorError> {
        // Covers the single live scratch state and length histogram, in addition
        // to charged stack clones. Charge before any execution-sized allocation.
        self.reserve(self.state_bytes)?;
        let max_hops = self.path_hops.iter().copied().max().unwrap_or(0) as usize;
        self.stats.hop_lengths = vec![0; max_hops + 1];
        self.reserve(self.state_bytes)?;
        let mut initial = SearchState::new(self.program.schema.columns.len());
        if let Some(seed) = seed {
            initial.locals = seed;
        }
        let mut pending = vec![initial];
        while let Some(seed) = pending.pop() {
            let pattern = seed.pattern;
            let path = &self.program.paths[pattern];
            let quota = termination::quota(path.automaton.selector.selector);
            if quota.is_some_and(|q| q.0 == 0) {
                self.release(self.state_bytes);
                continue;
            }
            self.breadth = path.open
                && path.automaton.mode.mode == crate::PathMode::Walk
                && !self.program.different_edges
                && quota.is_some();
            self.cutoff = false;
            let certificate_bytes = if self.breadth {
                let n = self.ctx.snapshot()?.node_count();
                let bytes = n
                    .checked_mul(n)
                    .and_then(|n| n.checked_add(1))
                    .and_then(|n| n.checked_mul(256))
                    .ok_or_else(|| invalid("path completion memory estimate overflow"))?;
                self.reserve(bytes)?;
                self.ctx.note_nodes_scanned(n, self.span())?;
                let remaining = self
                    .limits
                    .max_work
                    .saturating_sub(self.stats.product_states + self.stats.incidences);
                self.pairs = Some(termination::possible_pairs(
                    self.program,
                    &seed,
                    self.ctx,
                    &mut self.stats.completion_work,
                    remaining,
                )?);
                bytes
            } else {
                0
            };
            // Move the seed reservation into the DFS stack. Each correlated
            // incoming binding gets its own endpoint partitions, before joins.
            self.stack.push_back(seed);
            let started = Instant::now();
            self.discover()?;
            self.stats.discovery_time += started.elapsed();
            self.pairs = None;
            self.release(certificate_bytes);
            let count = self.candidates.len();
            let span = self.span();
            let started = Instant::now();
            selection::select(
                &mut self.candidates,
                self.program.paths[pattern].automaton.selector.selector,
                self.ctx,
                span,
            )?;
            self.stats.selection_time += started.elapsed();
            self.release((count - self.candidates.len()) * self.state_bytes);
            let mut selected = std::mem::take(&mut self.candidates);
            // Keep deterministic discovery order for subsequent automata.
            if pattern + 1 < self.program.paths.len() {
                selected.reverse();
            }
            for mut state in selected {
                self.ctx.check_cancel(self.span())?;
                if let Some(id) = self.program.paths[pattern].automaton.semantic.path_binding {
                    let started = Instant::now();
                    let value = materialize::path_value(&state, self.ctx.snapshot()?.graph_id());
                    if !state.bind(self.program, Some(id), None, value) {
                        self.release(self.state_bytes);
                        continue;
                    }
                    self.stats.materialized_paths += 1;
                    self.stats.materialization_time += started.elapsed();
                }
                if pattern + 1 == self.program.paths.len() {
                    self.emit(&state)?;
                    self.release(self.state_bytes);
                } else {
                    state.clause_edges.append(&mut state.edges);
                    state.nodes.clear();
                    state.directions.clear();
                    state.element_ends.clear();
                    state.current = None;
                    state.element = 0;
                    state.pattern += 1;
                    pending.push(state);
                }
            }
        }
        self.release(self.state_bytes);
        Ok(())
    }

    fn discover(&mut self) -> Result<(), ExecutorError> {
        let mut layer = 0;
        loop {
            if self.breadth && self.stack.front().is_none_or(|s| s.edges.len() > layer) {
                if self.complete() {
                    let count = self.stack.len();
                    self.stack.clear();
                    self.release(count * self.state_bytes);
                    return Ok(());
                }
                if let Some(front) = self.stack.front() {
                    layer = front.edges.len();
                }
            }
            let state = if self.breadth {
                self.stack.pop_front()
            } else {
                self.stack.pop_back()
            };
            let Some(mut state) = state else {
                break;
            };
            self.release(self.state_bytes);
            self.work()?;
            self.stats.product_states += 1;
            let path = &self.program.paths[state.pattern];
            if let Some(choice) = state.choice.take() {
                self.observe(&state, choice)?;
            }
            if state.element == path.automaton.semantic.elements.len() {
                if !self.qualify_path(&mut state)? {
                    continue;
                }
                if self.candidates.len() >= self.limits.max_rows {
                    return Err(self.limit("max_path_rows"));
                }
                self.reserve(self.state_bytes)?;
                self.candidates.push(state);
                self.stats.peak_candidate_bytes = self
                    .stats
                    .peak_candidate_bytes
                    .max(self.candidates.len() * self.state_bytes);
                self.stats.qualified_paths += 1;
                continue;
            }
            match &path.automaton.semantic.elements[state.element] {
                PathSemanticElement::Node(node) => self.node(state, node)?,
                PathSemanticElement::Edge(edge) => self.edge(state, edge)?,
            }
        }
        if self.cutoff {
            return Err(self.limit("max_path_hops"));
        }
        Ok(())
    }

    fn complete(&self) -> bool {
        let Some(pairs) = &self.pairs else {
            return false;
        };
        let pattern = self
            .candidates
            .first()
            .map(|s| s.pattern)
            .or_else(|| self.stack.front().map(|s| s.pattern));
        let Some(pattern) = pattern else {
            return pairs.is_empty();
        };
        termination::complete(
            pairs,
            &self.candidates,
            termination::quota(self.program.paths[pattern].automaton.selector.selector)
                .expect("selective BFS"),
        )
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
                state.element_ends.push(state.edges.len());
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
            self.stats.history_clones += 1;
            let mut next = state.clone();
            if next.bind(
                self.program,
                test.binding,
                test.temporary.map(|t| t.slot),
                Value::NodeRef(node),
            ) {
                next.current = Some(node);
                next.nodes.push(node);
                next.element_ends.push(0);
                next.element += 1;
                self.stats.hop_lengths[0] += 1;
                if self.breadth {
                    self.stack.push_front(next);
                } else {
                    self.stack.push_back(next);
                }
            } else {
                self.release(self.state_bytes);
            }
        }
        if !self.breadth {
            self.stack.make_contiguous()[start..].reverse();
        }
        self.release(candidate_bytes);
        Ok(())
    }

    fn edge(&mut self, mut state: SearchState, test: &EdgeTest) -> Result<(), ExecutorError> {
        let (min, max) = match test.quantifier {
            EdgeQuantifierKind::Single => (1, 1),
            EdgeQuantifierKind::Questioned => (0, 1),
            EdgeQuantifierKind::Bounded { min, max } => (min, max),
            EdgeQuantifierKind::Unbounded { min } => (min, u32::MAX),
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
                if state.edges.len() >= self.path_hops[state.pattern] as usize {
                    if self.breadth {
                        self.cutoff = true;
                        continue;
                    }
                    return Err(self.limit("max_path_hops"));
                }
                self.reserve(self.state_bytes)?;
                self.stats.history_clones += 1;
                let mut next = state.clone();
                next.current = Some(adjacent.neighbor);
                next.edges.push(adjacent.edge_id);
                next.directions.push(materialize::direction(
                    graph,
                    adjacent.edge_id,
                    current,
                    test.orientation.declared,
                ));
                next.nodes.push(adjacent.neighbor);
                next.depth += 1;
                next.choice = Some((current, adjacent.edge_id, choice));
                self.stack.push_back(next);
            }
            if !self.breadth {
                self.stack.make_contiguous()[start..].reverse();
            }
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
                state.element_ends.push(state.edges.len());
                state.depth = 0;
                state.element += 1;
                self.push(&state)?;
            }
        }
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
