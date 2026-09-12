//! Lowering-cost measurement without service-level objectives.

use std::collections::BTreeMap;

use crate::{
    ProcedureRegistry,
    analyze::{AnalyzedStatement, BindingId, ScopeId, StatementCategory},
    plan::logical::{effect::LogicalEffect, lowering::lower_logical},
};

/// Measure lowering cost without asserting a service-level objective.
///
/// Runs `lower_logical` once and returns the wall-clock microseconds plus the
/// conservative dependency sizes the cache must compare. Callers report the
/// numbers; they never gate correctness on them.
#[must_use]
pub fn measure_lowering_cost(
    analyzed: &AnalyzedStatement,
    registry: &dyn ProcedureRegistry,
) -> (u128, BTreeMap<&'static str, usize>) {
    let start = std::time::Instant::now();
    let plan = lower_logical(analyzed, registry);
    let elapsed = start.elapsed().as_micros();
    let mut sizes = BTreeMap::new();
    sizes.insert("calls", analyzed.calls.len());
    sizes.insert(
        "write_entries",
        analyzed
            .write_set
            .as_ref()
            .map_or(0, |set| set.entries.len()),
    );
    sizes.insert("expressions", analyzed.expressions.len());
    if let Ok(plan) = plan {
        sizes.insert("operators", plan.operators.len());
        sizes.insert("output_columns", plan.output_schema.columns.len());
        sizes.insert("paths", plan.paths.automata.len());
    }
    (elapsed, sizes)
}

#[allow(
    dead_code,
    reason = "category mapping is exercised through effect tests"
)]
pub(crate) const fn category_effect(category: StatementCategory) -> LogicalEffect {
    LogicalEffect::from_category(category)
}

#[allow(
    dead_code,
    reason = "binding lookup helper documents the semantic path"
)]
pub(crate) fn binding_lookup(analyzed: &AnalyzedStatement, binding: BindingId) -> Option<ScopeId> {
    analyzed
        .scopes
        .declarations()
        .iter()
        .find(|decl| decl.id() == binding)
        .map(|_| analyzed.root_scope())
}
