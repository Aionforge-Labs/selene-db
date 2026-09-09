//! Bounded variable-length edge repeat operator.

use selene_core::{EdgeId, NodeId, Value};
use selene_graph::adjacency::AdjacencyEdge;

use crate::{
    EdgeDirection, JoinTree, PathMode, PatternPlan,
    plan::RepeatEdgeMatch,
    runtime::{Binding, BindingTableSchema, EvalCtx, ExecutorError},
};

use super::{pattern, scan, visited_set};

pub(crate) fn execute(
    child: &JoinTree,
    edge: &RepeatEdgeMatch,
    direction: EdgeDirection,
    min: u32,
    max: Option<u32>,
    path_mode: PathMode,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    if path_mode != PathMode::Walk && max.is_some() {
        return Err(ExecutorError::FeatureNotSupportedYet {
            feature: "non-WALK variable-length edge execution",
            span: edge.span,
        });
    }

    let source_index = pattern::source_index(
        env.pattern,
        env.schema,
        edge.left_binding,
        edge.left_hidden_binding,
        "repeat source binding column missing",
    )?;
    let child_rows = pattern::walk_join_tree(child, env)?;
    let mut state = RepeatState {
        edge,
        direction,
        min,
        max,
        path_mode,
        cap: env.ctx.impl_defined_caps().max_quantifier,
        pattern_plan: env.pattern,
        schema: env.schema,
        source_index,
        final_slot: pattern::ColumnSlot::binding(
            env.pattern,
            env.schema,
            edge.final_binding,
            "repeat final binding column missing",
        )?,
        final_hidden_slot: pattern::ColumnSlot::hidden(
            env.schema,
            edge.final_hidden_binding,
            "repeat final hidden binding column missing",
        )?,
        group_slot: pattern::ColumnSlot::binding(
            env.pattern,
            env.schema,
            edge.group_binding,
            "repeat group binding column missing",
        )?,
        group_hidden_slot: pattern::ColumnSlot::hidden(
            env.schema,
            edge.group_hidden_binding,
            "repeat group hidden binding column missing",
        )?,
        ctx: env.ctx,
        output: Vec::new(),
        steps_since_check: 0,
    };
    for row in child_rows {
        state
            .ctx
            .tx
            .check_cancellation_stride(&mut state.steps_since_check, 1)?;
        let Some(source) = pattern::node_at_index(
            &row,
            state.source_index,
            "repeat source binding is not a node",
        )?
        else {
            continue;
        };
        let mut path_edges = Vec::new();
        traverse(source, source, &row, &mut path_edges, 0, &mut state)?;
    }
    Ok(state.output)
}

struct RepeatState<'a, 'eval, 'ctx, 'g, 'plan> {
    edge: &'a RepeatEdgeMatch,
    direction: EdgeDirection,
    min: u32,
    max: Option<u32>,
    path_mode: PathMode,
    cap: u32,
    pattern_plan: &'a PatternPlan,
    schema: &'a BindingTableSchema,
    source_index: usize,
    final_slot: pattern::ColumnSlot,
    final_hidden_slot: pattern::ColumnSlot,
    group_slot: pattern::ColumnSlot,
    group_hidden_slot: pattern::ColumnSlot,
    ctx: &'a EvalCtx<'eval, 'ctx, 'g, 'plan>,
    output: Vec<Binding>,
    steps_since_check: usize,
}

fn traverse(
    source: NodeId,
    current: NodeId,
    row: &Binding,
    path_edges: &mut Vec<EdgeId>,
    depth: u32,
    state: &mut RepeatState<'_, '_, '_, '_, '_>,
) -> Result<(), ExecutorError> {
    if depth >= state.min {
        maybe_emit_path(current, row, path_edges, state)?;
    }
    if state.max == Some(depth) {
        return Ok(());
    }

    for adjacent in adjacent_edges(current, state.direction, state.ctx) {
        state
            .ctx
            .tx
            .check_cancellation_stride(&mut state.steps_since_check, 1)?;
        if !edge_step_matches(adjacent.edge_id, row, path_edges, state)? {
            continue;
        }
        let next_depth = depth.saturating_add(1);
        let step = visited_set::repeat_step(
            visited_set::RepeatStepInput {
                path_mode: state.path_mode,
                source,
                target: adjacent.neighbor,
                edge: adjacent.edge_id,
                direction: state.direction,
                path_edges,
                next_depth,
                min: state.min,
            },
            pattern::WalkContext {
                pattern: state.pattern_plan,
                schema: state.schema,
                seed: None,
                ctx: state.ctx,
            },
        )?;
        let Some(step) = step else {
            continue;
        };
        if state.max.is_none() && next_depth > state.cap {
            return Err(ExecutorError::ProgramLimitExceeded {
                detail: "max_quantifier",
                span: state.edge.span,
            });
        }
        path_edges.push(adjacent.edge_id);
        if step.terminal {
            maybe_emit_path(adjacent.neighbor, row, path_edges, state)?;
        } else {
            traverse(
                source,
                adjacent.neighbor,
                row,
                path_edges,
                next_depth,
                state,
            )?;
        }
        path_edges.pop();
    }
    Ok(())
}

fn maybe_emit_path(
    final_node: NodeId,
    row: &Binding,
    path_edges: &[EdgeId],
    state: &mut RepeatState<'_, '_, '_, '_, '_>,
) -> Result<(), ExecutorError> {
    if !final_node_label_matches(state.edge, final_node, state.ctx) {
        return Ok(());
    }

    let mut values = row.cloned_values();
    values.resize(state.schema.columns.len(), Value::Null);
    if !state
        .final_slot
        .set(&mut values, Value::NodeRef(final_node))
    {
        return Ok(());
    }
    if !state
        .final_hidden_slot
        .set(&mut values, Value::NodeRef(final_node))
    {
        return Ok(());
    }
    let group_value = edge_list_value(path_edges);
    if !state.group_slot.set(&mut values, group_value.clone()) {
        return Ok(());
    }
    if !state.group_hidden_slot.set(&mut values, group_value) {
        return Ok(());
    }

    let candidate = Binding::from_parts(values, row.cloned_insert_sites());
    if !predicates_pass(
        &state.edge.final_property_predicates,
        &candidate,
        &Value::NodeRef(final_node),
        state,
    )? {
        return Ok(());
    }
    state.output.push(candidate);
    Ok(())
}

fn edge_step_matches(
    edge_id: EdgeId,
    row: &Binding,
    path_edges: &[EdgeId],
    state: &RepeatState<'_, '_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    if !edge_label_matches(state.edge, edge_id, state.ctx) {
        return Ok(false);
    }
    predicates_pass(
        &state.edge.property_predicates,
        row,
        &Value::EdgeRef(edge_id),
        state,
    )
    .and_then(|passes| {
        if !passes || state.edge.inline_predicates.is_empty() {
            return Ok(passes);
        }
        let candidate;
        let predicate_row = if state.edge.group_binding.is_some() {
            candidate = current_step_row(row, path_edges, edge_id, state)?;
            let Some(candidate) = candidate.as_ref() else {
                return Ok(false);
            };
            candidate
        } else {
            row
        };
        pattern::filter_predicates_pass(
            &state.edge.inline_predicates,
            state.pattern_plan,
            predicate_row,
            state.schema,
            state.ctx,
        )
    })
}

fn current_step_row(
    row: &Binding,
    path_edges: &[EdgeId],
    edge_id: EdgeId,
    state: &RepeatState<'_, '_, '_, '_, '_>,
) -> Result<Option<Binding>, ExecutorError> {
    let mut values = row.cloned_values();
    values.resize(state.schema.columns.len(), Value::Null);
    let edge_list = path_edges
        .iter()
        .copied()
        .chain(std::iter::once(edge_id))
        .map(Value::EdgeRef)
        .collect();
    if !state.group_slot.set(&mut values, Value::List(edge_list)) {
        return Ok(None);
    }
    Ok(Some(Binding::from_parts(values, row.cloned_insert_sites())))
}

fn edge_list_value(path_edges: &[EdgeId]) -> Value {
    Value::List(path_edges.iter().copied().map(Value::EdgeRef).collect())
}

fn predicates_pass(
    predicates: &[crate::FilterPredicate],
    row: &Binding,
    entity: &Value,
    state: &RepeatState<'_, '_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    for predicate in predicates {
        if !scan::predicate_passes(
            predicate,
            state.pattern_plan,
            row,
            state.schema,
            entity,
            state.ctx,
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn adjacent_edges(
    node: NodeId,
    direction: EdgeDirection,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Vec<AdjacencyEdge> {
    super::edge_access::adjacent_edges(ctx.tx.snapshot(), node, direction)
        .cloned()
        .collect()
}

fn edge_label_matches(
    edge: &RepeatEdgeMatch,
    edge_id: EdgeId,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> bool {
    let Some(label_expr) = &edge.label_predicate else {
        return true;
    };
    ctx.tx
        .snapshot()
        .edge_label(edge_id)
        .is_some_and(|label| scan::label_matches_edge(label_expr, label))
}

fn final_node_label_matches(
    edge: &RepeatEdgeMatch,
    node: NodeId,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> bool {
    let Some(label_expr) = &edge.final_label_predicate else {
        return true;
    };
    ctx.tx
        .snapshot()
        .node_labels(node)
        .is_some_and(|labels| scan::label_matches_node(label_expr, labels))
}
