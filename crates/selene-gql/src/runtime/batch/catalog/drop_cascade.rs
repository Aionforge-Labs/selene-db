//! `DROP NODE TYPE` / `DROP EDGE TYPE` execution (RESTRICT default + CASCADE).
//!
//! Extracted from the catalog pipeline operator to keep `mod.rs` under the
//! 700-LOC file cap. The branching policy is the Seam-B fix from the
//! deletion-reclamation audit (Item 3): the mutator (the single write funnel,
//! hard rule 11) owns the surviving-instance check and the truncate-then-drop
//! ordering, parameterized by [`GraphDropBehavior`], so no I/O surface can
//! bypass it. The executor only threads the planned behavior through.
//!
//! `RESTRICT` (the default when no behavior keyword is written) rejects a drop
//! whose type still has instances (or, for node types, an inbound edge-type
//! reference) with `G2000` `GraphTypeViolation` and drops nothing.
//! `CASCADE` (`IM_DROP_CASCADE`) truncates the instances first, then drops the
//! type, atomically in one transaction.

use selene_core::DbString;
use selene_graph::DropBehavior as GraphDropBehavior;

use crate::{
    DropBehavior, SourceSpan,
    runtime::{BindingTable, ExecutorError, TxContext},
};

use super::{catalog_graph_error, closed_graph_type, edge_type_exists, node_type_exists};

/// Map the planner-level [`DropBehavior`] onto the graph mutator's behavior.
const fn graph_drop_behavior(behavior: DropBehavior) -> GraphDropBehavior {
    match behavior {
        DropBehavior::Restrict => GraphDropBehavior::Restrict,
        DropBehavior::Cascade => GraphDropBehavior::Cascade,
    }
}

/// Execute `DROP NODE TYPE :label [RESTRICT | CASCADE]`.
///
/// An absent type under `IF EXISTS` is a clean no-op (handled before any
/// mutation). Otherwise the mutator enforces the behavior policy and either
/// rejects (RESTRICT with surviving instances / dependency) or truncates then
/// drops (CASCADE), all inside the caller's single write transaction.
pub(super) fn execute_drop_node_type(
    label: DbString,
    if_exists: bool,
    behavior: DropBehavior,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    ctx.ensure_write_txn("catalog op invoked without write transaction", span)?;
    let graph_type = closed_graph_type(ctx.snapshot(), span)?;
    if !node_type_exists(Some(&graph_type), label.clone()) && if_exists {
        return Ok(table);
    }
    ctx.mutator_with_span("catalog op invoked without write transaction", span)?
        .drop_node_type(label, graph_drop_behavior(behavior))
        .map_err(|source| catalog_graph_error(source, span))?;
    Ok(table)
}

/// Execute `DROP EDGE TYPE :label [RESTRICT | CASCADE]`.
///
/// Symmetric with [`execute_drop_node_type`]; edge types have no inbound type
/// dependency, so RESTRICT enforces only the surviving-instance check.
pub(super) fn execute_drop_edge_type(
    label: DbString,
    if_exists: bool,
    behavior: DropBehavior,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    ctx.ensure_write_txn("catalog op invoked without write transaction", span)?;
    let graph_type = closed_graph_type(ctx.snapshot(), span)?;
    if !edge_type_exists(Some(&graph_type), label.clone()) && if_exists {
        return Ok(table);
    }
    ctx.mutator_with_span("catalog op invoked without write transaction", span)?
        .drop_edge_type(label, graph_drop_behavior(behavior))
        .map_err(|source| catalog_graph_error(source, span))?;
    Ok(table)
}
