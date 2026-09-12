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
    // Do not use a later false constant to suppress an earlier expression
    // error/effect. Tightening is safe only when every local condition is a
    // literal property comparison; opaque predicates retain the coarse superset.
    let literal_only = path.conditions.iter().all(|conditions| {
        conditions.inline.is_none()
            && conditions
                .properties
                .iter()
                .all(|(_, expr)| matches!(expr, crate::ValueExpr::Literal(_)))
    });
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
            || (literal_only && !literal_properties_possible(&path.conditions[0], start, graph))
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
                && (!literal_only
                    || literal_properties_possible(path.conditions.last().unwrap(), node, graph))
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

// Constants have no binding dependencies, errors or effects. A false literal
// property test proves an endpoint impossible without evaluating predicates
// early. All other expressions remain opaque, so qualification/order is intact.
fn literal_properties_possible(
    conditions: &super::conditions::Conditions,
    node: NodeId,
    graph: &selene_graph::SeleneGraph,
) -> bool {
    conditions.properties.iter().all(|(key, expr)| {
        let crate::ValueExpr::Literal(literal) = expr else {
            return true;
        };
        let expected = crate::runtime::evaluator::literal_value(literal);
        graph
            .node_properties(node)
            .and_then(|properties| properties.get(key))
            .is_some_and(|actual| {
                !matches!(actual, Value::Null)
                    && !matches!(expected, Value::Null)
                    && crate::runtime::value_compare::equal_non_null(actual, &expected)
            })
    })
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
