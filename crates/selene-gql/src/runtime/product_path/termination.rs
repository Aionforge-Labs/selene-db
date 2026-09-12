//! Conservative completion certificates for open WALK selectors.
//!
//! This is NOT a distance cache or a path acceptance oracle. Reachability through
//! the union of the automaton's edge tests only over-approximates possible endpoint
//! pairs. History and predicates remain in the enumerator. A superset member with
//! no qualifying path prevents early completion (and may cause a resource error).
//! We stop only after EVERY possible partition has its requested quota and the
//! whole last qualifying length layer is drained. No required tie is discarded.

use super::{BoundedPathProgram, state::SearchState};
use crate::{
    PathSelector, PathSemanticElement,
    runtime::{ExecutorError, batch::operator::BatchExecutionContext, edge_access, scan},
};
use selene_core::{NodeId, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(super) type Pairs = BTreeSet<(NodeId, NodeId)>;

pub(super) fn quota(selector: Option<PathSelector>) -> Option<(usize, bool)> {
    match selector? {
        PathSelector::All => None,
        PathSelector::Any { paths } | PathSelector::CountedShortest { paths } => {
            Some((paths as usize, false))
        }
        PathSelector::AnyShortest => Some((1, false)),
        PathSelector::AllShortest => Some((1, true)),
        PathSelector::CountedShortestGroup { groups } => Some((groups as usize, true)),
    }
}

pub(super) fn possible_pairs(
    program: &BoundedPathProgram<'_>,
    seed: &SearchState,
    ctx: &BatchExecutionContext<'_>,
    work: &mut u64,
    max_work: u64,
) -> Result<Pairs, ExecutorError> {
    let path = &program.paths[seed.pattern];
    let elements = &path.automaton.semantic.elements;
    let PathSemanticElement::Node(first) = &elements[0] else {
        unreachable!()
    };
    let PathSemanticElement::Node(last) = elements.last().unwrap() else {
        unreachable!()
    };
    let graph = ctx.snapshot()?;
    let nodes = graph
        .live_node_candidates()
        .map_err(|_| super::compile::invalid("path completion candidates unavailable"))?;
    let bound = |id: Option<crate::BindingId>| {
        id.and_then(|id| program.bindings.iter().position(|b| *b == id))
            .and_then(|slot| seed.locals[slot].as_ref())
    };
    let mut pairs = Pairs::new();
    for start in nodes.iter() {
        if bound(first.binding).is_some_and(|v| v != &Value::NodeRef(start))
            || !graph.node_labels(start).is_some_and(|labels| {
                first
                    .label
                    .as_ref()
                    .is_none_or(|l| scan::label_matches_node(l, labels))
            })
        {
            continue;
        }
        let mut reached = BTreeSet::from([start]);
        let mut pending = vec![start];
        while let Some(node) = pending.pop() {
            tick(ctx, path.automaton.origin, work, max_work)?;
            if bound(last.binding).is_none_or(|v| v == &Value::NodeRef(node))
                && (first.binding.is_none() || first.binding != last.binding || node == start)
                && graph.node_labels(node).is_some_and(|labels| {
                    last.label
                        .as_ref()
                        .is_none_or(|l| scan::label_matches_node(l, labels))
                })
            {
                pairs.insert((start, node));
            }
            for element in elements {
                let PathSemanticElement::Edge(test) = element else {
                    continue;
                };
                for adjacent in edge_access::adjacent_edges(graph, node, test.orientation.declared)
                {
                    tick(ctx, path.automaton.origin, work, max_work)?;
                    if graph.edge_label(adjacent.edge_id).is_some_and(|label| {
                        test.label
                            .as_ref()
                            .is_none_or(|l| scan::label_matches_edge(l, label))
                    }) && reached.insert(adjacent.neighbor)
                    {
                        pending.push(adjacent.neighbor);
                    }
                }
            }
        }
    }
    Ok(pairs)
}

fn tick(
    ctx: &BatchExecutionContext<'_>,
    span: crate::SourceSpan,
    work: &mut u64,
    max: u64,
) -> Result<(), ExecutorError> {
    ctx.check_cancel(span)?;
    if *work >= max {
        return Err(ExecutorError::ProgramLimitExceeded {
            detail: "max_path_work",
            span,
        });
    }
    *work += 1;
    Ok(())
}

pub(super) fn complete(pairs: &Pairs, candidates: &[SearchState], quota: (usize, bool)) -> bool {
    if quota.0 == 0 || pairs.is_empty() {
        return true;
    }
    let mut counts = BTreeMap::<_, (usize, BTreeSet<usize>)>::new();
    for state in candidates {
        let entry = counts
            .entry((state.nodes[0], *state.nodes.last().unwrap()))
            .or_default();
        entry.0 += 1;
        entry.1.insert(state.edges.len());
    }
    pairs.iter().all(|pair| {
        counts.get(pair).is_some_and(|(count, lengths)| {
            (if quota.1 { lengths.len() } else { *count }) >= quota.0
        })
    })
}
