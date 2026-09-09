//! Shared path visited-set helpers.

use rustc_hash::FxHashSet;
use selene_core::{EdgeId, NodeId, Value};

use crate::{
    BindingId, EdgeDirection, HiddenBindingId, HopContributor, PathContributor, PathMode,
    TailBinding,
    runtime::{Binding, ExecutorError},
};

use super::pattern;

pub(crate) struct RepeatStep {
    pub(crate) terminal: bool,
}

pub(crate) struct RepeatStepInput<'a> {
    pub(crate) path_mode: PathMode,
    pub(crate) source: NodeId,
    pub(crate) target: NodeId,
    pub(crate) edge: EdgeId,
    pub(crate) direction: EdgeDirection,
    pub(crate) path_edges: &'a [EdgeId],
    pub(crate) next_depth: u32,
    pub(crate) min: u32,
}

pub(crate) fn repeat_step(
    input: RepeatStepInput<'_>,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Option<RepeatStep>, ExecutorError> {
    match input.path_mode {
        PathMode::Walk => Ok(Some(RepeatStep { terminal: false })),
        PathMode::Trail => {
            if input.path_edges.contains(&input.edge) {
                Ok(None)
            } else {
                Ok(Some(RepeatStep { terminal: false }))
            }
        }
        PathMode::Acyclic => {
            let nodes = repeat_nodes(input.source, input.direction, input.path_edges, env)?;
            if nodes.contains(&input.target) {
                Ok(None)
            } else {
                Ok(Some(RepeatStep { terminal: false }))
            }
        }
        PathMode::Simple => repeat_simple_step(input, env),
    }
}

pub(crate) fn trail_allows_hops(
    row: &Binding,
    contributors: &[HopContributor],
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    let mut seen = FxHashSet::default();
    for contributor in contributors {
        match contributor {
            HopContributor::Fixed(0) => {}
            HopContributor::Fixed(_) => {
                return Err(ExecutorError::ImplementationDefined {
                    detail: "path-search trail contributor lacks edge identity",
                });
            }
            HopContributor::EdgeNamed(binding) => {
                if !insert_edge_value(row_binding_value(row, *binding, env)?, &mut seen)? {
                    return Ok(false);
                }
            }
            HopContributor::EdgeHidden(hidden) => {
                if !insert_edge_value(row_hidden_value(row, *hidden, env)?, &mut seen)? {
                    return Ok(false);
                }
            }
            HopContributor::QuestionedNamed(binding) => {
                if !insert_optional_edge_value(row_binding_value(row, *binding, env)?, &mut seen)? {
                    return Ok(false);
                }
            }
            HopContributor::QuestionedHidden(hidden) => {
                if !insert_optional_edge_value(row_hidden_value(row, *hidden, env)?, &mut seen)? {
                    return Ok(false);
                }
            }
            HopContributor::GroupNamed(binding) => {
                if !insert_edge_list(row_binding_value(row, *binding, env)?, &mut seen)? {
                    return Ok(false);
                }
            }
            HopContributor::GroupHidden(hidden) => {
                if !insert_edge_list(row_hidden_value(row, *hidden, env)?, &mut seen)? {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

pub(crate) fn trail_allows_path(
    row: &Binding,
    contributors: &[PathContributor],
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    let mut seen = FxHashSet::default();
    for edge in collect_path_edges(row, contributors, env)? {
        if !seen.insert(edge) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn collect_path_nodes(
    row: &Binding,
    contributors: &[PathContributor],
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<NodeId>, ExecutorError> {
    let mut nodes = Vec::new();
    for contributor in contributors {
        match *contributor {
            PathContributor::Node(binding) => {
                if let Some(node) = node_value(row_tail_value(row, binding, env)?)? {
                    nodes.push(node);
                }
            }
            PathContributor::EdgeNamed(_) | PathContributor::EdgeHidden(_) => {}
            PathContributor::QuestionedEdgeNamed {
                binding,
                final_binding,
            } => {
                if optional_edge_value(row_binding_value(row, binding, env)?)?.is_some()
                    && let Some(node) = node_value(row_tail_value(row, final_binding, env)?)?
                {
                    nodes.push(node);
                }
            }
            PathContributor::QuestionedEdgeHidden {
                hidden,
                final_binding,
            } => {
                if optional_edge_value(row_hidden_value(row, hidden, env)?)?.is_some()
                    && let Some(node) = node_value(row_tail_value(row, final_binding, env)?)?
                {
                    nodes.push(node);
                }
            }
            PathContributor::EdgeGroupNamed {
                binding,
                source,
                direction,
            } => {
                let edges = edge_list(row_binding_value(row, binding, env)?)?;
                append_group_nodes(row, source, direction, &edges, env, &mut nodes)?;
            }
            PathContributor::EdgeGroupHidden {
                hidden,
                source,
                direction,
            } => {
                let edges = edge_list(row_hidden_value(row, hidden, env)?)?;
                append_group_nodes(row, source, direction, &edges, env, &mut nodes)?;
            }
        }
    }
    Ok(nodes)
}

fn collect_path_edges(
    row: &Binding,
    contributors: &[PathContributor],
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<EdgeId>, ExecutorError> {
    let mut edges = Vec::new();
    for contributor in contributors {
        match *contributor {
            PathContributor::Node(_) => {}
            PathContributor::EdgeNamed(binding) => {
                edges.push(edge_value(row_binding_value(row, binding, env)?)?);
            }
            PathContributor::EdgeHidden(hidden) => {
                edges.push(edge_value(row_hidden_value(row, hidden, env)?)?);
            }
            PathContributor::QuestionedEdgeNamed { binding, .. } => {
                if let Some(edge) = optional_edge_value(row_binding_value(row, binding, env)?)? {
                    edges.push(edge);
                }
            }
            PathContributor::QuestionedEdgeHidden { hidden, .. } => {
                if let Some(edge) = optional_edge_value(row_hidden_value(row, hidden, env)?)? {
                    edges.push(edge);
                }
            }
            PathContributor::EdgeGroupNamed { binding, .. } => {
                edges.extend(edge_list(row_binding_value(row, binding, env)?)?);
            }
            PathContributor::EdgeGroupHidden { hidden, .. } => {
                edges.extend(edge_list(row_hidden_value(row, hidden, env)?)?);
            }
        }
    }
    Ok(edges)
}

fn append_group_nodes(
    row: &Binding,
    source: TailBinding,
    direction: EdgeDirection,
    edges: &[EdgeId],
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
    nodes: &mut Vec<NodeId>,
) -> Result<(), ExecutorError> {
    if edges.is_empty() {
        return Ok(());
    }
    let Some(mut current) = node_value(row_tail_value(row, source, env)?)? else {
        return Err(ExecutorError::ImplementationDefined {
            detail: "path-mode quantified edge source is null",
        });
    };
    if nodes.last().copied() != Some(current) {
        nodes.push(current);
    }
    for edge in edges {
        let next = next_node(*edge, current, direction, env)?;
        nodes.push(next);
        current = next;
    }
    Ok(())
}

fn repeat_simple_step(
    input: RepeatStepInput<'_>,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Option<RepeatStep>, ExecutorError> {
    let nodes = repeat_nodes(input.source, input.direction, input.path_edges, env)?;
    if !nodes.contains(&input.target) {
        return Ok(Some(RepeatStep { terminal: false }));
    }
    let closes_at_source = input.target == input.source
        && nodes.iter().filter(|node| **node == input.source).count() == 1;
    if closes_at_source && input.next_depth >= input.min {
        return Ok(Some(RepeatStep { terminal: true }));
    }
    Ok(None)
}

fn repeat_nodes(
    source: NodeId,
    direction: EdgeDirection,
    path_edges: &[EdgeId],
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<NodeId>, ExecutorError> {
    let mut nodes = Vec::with_capacity(path_edges.len().saturating_add(1));
    let mut current = source;
    nodes.push(current);
    for edge in path_edges {
        current = next_node(*edge, current, direction, env)?;
        nodes.push(current);
    }
    Ok(nodes)
}

fn next_node(
    edge: EdgeId,
    current: NodeId,
    direction: EdgeDirection,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<NodeId, ExecutorError> {
    if env.ctx.tx.snapshot().edge_endpoints(edge).is_none() {
        return Err(ExecutorError::ImplementationDefined {
            detail: "path-mode contributor references missing edge",
        });
    }
    super::edge_access::next_node(env.ctx.tx.snapshot(), edge, current, direction).ok_or(
        ExecutorError::ImplementationDefined {
            detail: "path-mode edge endpoints are inconsistent with path direction",
        },
    )
}

fn insert_edge_list(value: Value, seen: &mut FxHashSet<EdgeId>) -> Result<bool, ExecutorError> {
    for edge in edge_list(value)? {
        if !seen.insert(edge) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn insert_edge_value(value: Value, seen: &mut FxHashSet<EdgeId>) -> Result<bool, ExecutorError> {
    Ok(seen.insert(edge_value(value)?))
}

fn insert_optional_edge_value(
    value: Value,
    seen: &mut FxHashSet<EdgeId>,
) -> Result<bool, ExecutorError> {
    let Some(edge) = optional_edge_value(value)? else {
        return Ok(true);
    };
    Ok(seen.insert(edge))
}

fn edge_list(value: Value) -> Result<Vec<EdgeId>, ExecutorError> {
    let Value::List(values) = value else {
        return Err(ExecutorError::ImplementationDefined {
            detail: "path-mode edge group contributor is not a list",
        });
    };
    values.into_iter().map(edge_value).collect()
}

fn edge_value(value: Value) -> Result<EdgeId, ExecutorError> {
    let Value::EdgeRef(edge) = value else {
        return Err(ExecutorError::ImplementationDefined {
            detail: "path-mode edge contributor is not an edge",
        });
    };
    Ok(edge)
}

fn optional_edge_value(value: Value) -> Result<Option<EdgeId>, ExecutorError> {
    match value {
        Value::Null => Ok(None),
        Value::EdgeRef(edge) => Ok(Some(edge)),
        _ => Err(ExecutorError::ImplementationDefined {
            detail: "path-mode questioned edge contributor is not an edge or null",
        }),
    }
}

fn node_value(value: Value) -> Result<Option<NodeId>, ExecutorError> {
    match value {
        Value::NodeRef(id) => Ok(Some(id)),
        Value::Null => Ok(None),
        _ => Err(ExecutorError::ImplementationDefined {
            detail: "path-mode node contributor is not a node",
        }),
    }
}

fn row_tail_value(
    row: &Binding,
    binding: TailBinding,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    match binding {
        TailBinding::Named(binding) => row_binding_value(row, binding, env),
        TailBinding::Hidden(hidden) => row_hidden_value(row, hidden, env),
    }
}

fn row_binding_value(
    row: &Binding,
    binding: BindingId,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let Some(index) = pattern::binding_index(env.pattern, env.schema, binding) else {
        return Err(ExecutorError::ImplementationDefined {
            detail: "path-mode binding column missing",
        });
    };
    Ok(row.get(index).cloned().unwrap_or(Value::Null))
}

fn row_hidden_value(
    row: &Binding,
    hidden: HiddenBindingId,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let Some(index) = pattern::hidden_index(env.schema, hidden) else {
        return Err(ExecutorError::ImplementationDefined {
            detail: "path-mode hidden binding column missing",
        });
    };
    Ok(row.get(index).cloned().unwrap_or(Value::Null))
}
