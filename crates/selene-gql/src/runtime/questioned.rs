//! Questioned edge (`?`) join-tree operator.

use std::collections::BTreeSet;

use selene_core::{EdgeId, NodeId, Value};
use selene_graph::{CandidateSet, Edge};

use crate::{
    EdgeDirection, EdgeMatch, JoinTree, PatternPlan,
    runtime::{Binding, BindingTableSchema, EvalCtx, ExecutorError},
};

use super::{edge_access, pattern, scan};

pub(crate) fn execute(
    child: &JoinTree,
    edge: &EdgeMatch,
    direction: EdgeDirection,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let source_index = pattern::source_index(
        env.pattern,
        env.schema,
        edge.left_binding,
        edge.left_hidden_binding,
        "questioned source binding column missing",
    )?;
    let child_rows = pattern::walk_join_tree(child, env)?;
    let mut rows = Vec::new();
    let edge_candidate_filter = edge_access::candidate_edge_filter(edge, env.ctx)?;
    let mut state = QuestionedState {
        edge,
        pattern_plan: env.pattern,
        schema: env.schema,
        source_index,
        edge_slot: pattern::ColumnSlot::binding(
            env.pattern,
            env.schema,
            edge.binding,
            "questioned edge binding column missing",
        )?,
        edge_hidden_slot: pattern::ColumnSlot::hidden(
            env.schema,
            edge.hidden_binding,
            "questioned edge hidden binding column missing",
        )?,
        right_slot: pattern::ColumnSlot::binding(
            env.pattern,
            env.schema,
            edge.right_binding,
            "questioned right binding column missing",
        )?,
        right_hidden_slot: pattern::ColumnSlot::hidden(
            env.schema,
            edge.right_hidden_binding,
            "questioned right hidden binding column missing",
        )?,
        edge_candidate_filter,
        ctx: env.ctx,
        output: &mut rows,
    };
    for row in child_rows {
        let Some(source) = pattern::node_at_index(
            &row,
            state.source_index,
            "questioned source binding is not a node",
        )?
        else {
            continue;
        };
        maybe_emit_skipped(source, &row, &mut state)?;
        expand_from_source(source, &row, direction, &mut state)?;
    }
    Ok(rows)
}

struct QuestionedState<'a, 'eval, 'ctx, 'g, 'plan, 'out> {
    edge: &'a EdgeMatch,
    pattern_plan: &'a PatternPlan,
    schema: &'a BindingTableSchema,
    source_index: usize,
    edge_slot: pattern::ColumnSlot,
    edge_hidden_slot: pattern::ColumnSlot,
    right_slot: pattern::ColumnSlot,
    right_hidden_slot: pattern::ColumnSlot,
    edge_candidate_filter: Option<CandidateSet<Edge>>,
    ctx: &'a EvalCtx<'eval, 'ctx, 'g, 'plan>,
    output: &'out mut Vec<Binding>,
}

fn maybe_emit_skipped(
    source: NodeId,
    row: &Binding,
    state: &mut QuestionedState<'_, '_, '_, '_, '_, '_>,
) -> Result<(), ExecutorError> {
    if !right_node_matches(state.edge, source, state.ctx) {
        return Ok(());
    }
    let mut values = row.cloned_values();
    values.resize(state.schema.columns.len(), Value::Null);
    if !state.edge_slot.set(&mut values, Value::Null) {
        return Ok(());
    }
    if !state.edge_hidden_slot.set(&mut values, Value::Null) {
        return Ok(());
    }
    if !state.right_slot.set(&mut values, Value::NodeRef(source)) {
        return Ok(());
    }
    if !state
        .right_hidden_slot
        .set(&mut values, Value::NodeRef(source))
    {
        return Ok(());
    }
    let candidate = Binding::from_parts(values, row.cloned_insert_sites());
    if !predicates_pass(
        &state.edge.right_property_predicates,
        state.pattern_plan,
        &candidate,
        state.schema,
        &Value::NodeRef(source),
        state.ctx,
    )? {
        return Ok(());
    }
    state.output.push(candidate);
    Ok(())
}

fn expand_from_source(
    source: NodeId,
    row: &Binding,
    direction: EdgeDirection,
    state: &mut QuestionedState<'_, '_, '_, '_, '_, '_>,
) -> Result<(), ExecutorError> {
    let mut seen = BTreeSet::new();
    match direction {
        EdgeDirection::Right => {
            if let Some(entry) = state.ctx.tx.snapshot().outgoing_edges(source) {
                for adjacent in entry.iter() {
                    maybe_emit_taken(adjacent.edge_id, adjacent.neighbor, row, state)?;
                }
            }
        }
        EdgeDirection::Left => {
            if let Some(entry) = state.ctx.tx.snapshot().incoming_edges(source) {
                for adjacent in entry.iter() {
                    maybe_emit_taken(adjacent.edge_id, adjacent.neighbor, row, state)?;
                }
            }
        }
        EdgeDirection::Undirected => {
            if let Some(entry) = state.ctx.tx.snapshot().outgoing_edges(source) {
                for adjacent in entry.iter() {
                    if seen.insert(adjacent.edge_id) {
                        maybe_emit_taken(adjacent.edge_id, adjacent.neighbor, row, state)?;
                    }
                }
            }
            if let Some(entry) = state.ctx.tx.snapshot().incoming_edges(source) {
                for adjacent in entry.iter() {
                    if seen.insert(adjacent.edge_id) {
                        maybe_emit_taken(adjacent.edge_id, adjacent.neighbor, row, state)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn maybe_emit_taken(
    edge_id: EdgeId,
    right_node: NodeId,
    row: &Binding,
    state: &mut QuestionedState<'_, '_, '_, '_, '_, '_>,
) -> Result<(), ExecutorError> {
    if !edge_access::edge_filter_matches(state.edge_candidate_filter.as_ref(), edge_id) {
        return Ok(());
    }
    if !edge_label_matches(state.edge, edge_id, state.ctx)
        || !right_node_matches(state.edge, right_node, state.ctx)
    {
        return Ok(());
    }

    let mut values = row.cloned_values();
    values.resize(state.schema.columns.len(), Value::Null);
    if !state.edge_slot.set(&mut values, Value::EdgeRef(edge_id)) {
        return Ok(());
    }
    if !state
        .edge_hidden_slot
        .set(&mut values, Value::EdgeRef(edge_id))
    {
        return Ok(());
    }
    if !state
        .right_slot
        .set(&mut values, Value::NodeRef(right_node))
    {
        return Ok(());
    }
    if !state
        .right_hidden_slot
        .set(&mut values, Value::NodeRef(right_node))
    {
        return Ok(());
    }
    let candidate = Binding::from_parts(values, row.cloned_insert_sites());
    if !predicates_pass(
        &state.edge.property_predicates,
        state.pattern_plan,
        &candidate,
        state.schema,
        &Value::EdgeRef(edge_id),
        state.ctx,
    )? || !predicates_pass(
        &state.edge.right_property_predicates,
        state.pattern_plan,
        &candidate,
        state.schema,
        &Value::NodeRef(right_node),
        state.ctx,
    )? {
        return Ok(());
    }
    state.output.push(candidate);
    Ok(())
}

fn edge_label_matches(edge: &EdgeMatch, edge_id: EdgeId, ctx: &EvalCtx<'_, '_, '_, '_>) -> bool {
    let Some(label_expr) = &edge.label_predicate else {
        return true;
    };
    ctx.tx
        .snapshot()
        .edge_label(edge_id)
        .is_some_and(|label| scan::label_matches_edge(label_expr, label))
}

fn right_node_matches(edge: &EdgeMatch, node: NodeId, ctx: &EvalCtx<'_, '_, '_, '_>) -> bool {
    let Some(label_expr) = &edge.right_label_predicate else {
        return true;
    };
    ctx.tx
        .snapshot()
        .node_labels(node)
        .is_some_and(|labels| scan::label_matches_node(label_expr, labels))
}

fn predicates_pass(
    predicates: &[crate::FilterPredicate],
    pattern_plan: &PatternPlan,
    row: &Binding,
    schema: &BindingTableSchema,
    entity: &Value,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    for predicate in predicates {
        if !scan::predicate_passes(predicate, pattern_plan, row, schema, entity, ctx)? {
            return Ok(false);
        }
    }
    Ok(true)
}
