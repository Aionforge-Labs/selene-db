//! Non-leading MATCH lowering helpers.

use std::collections::BTreeSet;

use crate::{
    MatchClause,
    analyze::AnalyzedStatement,
    plan::{BindingTableColumn, PipelineOp, PlannerError},
};

use super::{match_clause, visible_after_pattern};

pub(super) fn lower(
    clause: &MatchClause,
    analyzed: &AnalyzedStatement,
    ops: &mut Vec<PipelineOp>,
    visible: &mut Vec<BindingTableColumn>,
    max_quantifier: super::PathLowering<'_>,
) -> Result<(), PlannerError> {
    let left_names = visible
        .iter()
        .filter_map(|column| column.name.clone())
        .collect::<BTreeSet<_>>();
    let (pattern, global_filters) =
        match_clause::lower_pipeline_match(clause, analyzed, &left_names, max_quantifier)?;
    for column in visible_after_pattern(Some(&pattern)) {
        if !visible.iter().any(|existing| existing.name == column.name) {
            visible.push(column);
        }
    }
    if clause.optional {
        ops.push(PipelineOp::OptionalMatch(pattern));
    } else {
        ops.push(PipelineOp::Match(pattern));
    }
    ops.extend(global_filters.into_iter().map(PipelineOp::Filter));
    Ok(())
}
