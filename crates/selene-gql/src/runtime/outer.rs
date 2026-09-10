//! Left-outer join-tree operator.

use selene_core::DbString;

use crate::{
    FilterPredicate, JoinTree,
    runtime::{Binding, ExecutorError},
};

use super::pattern;

pub(crate) fn execute(
    left: &JoinTree,
    right: &JoinTree,
    key: &[DbString],
    right_filters: &[FilterPredicate],
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let key_indexes = pattern::resolve_key(env.schema, key)?;
    let left_rows = pattern::walk_join_tree(left, env)?;
    let mut output = Vec::new();
    for left_row in left_rows {
        let right_env = pattern::WalkContext {
            pattern: env.pattern,
            schema: env.schema,
            seed: Some(&left_row),
            ctx: env.ctx,
        };
        let right_rows = pattern::walk_join_tree(right, right_env)?;
        let mut matched = false;
        for right_row in right_rows {
            if !pattern::filter_predicates_pass(
                right_filters,
                env.pattern,
                &right_row,
                env.schema,
                env.ctx,
            )? {
                continue;
            }
            if !pattern::rows_match_on_resolved_key(&left_row, &right_row, &key_indexes)? {
                continue;
            }
            output.push(pattern::merge_rows(&left_row, &right_row, env.schema));
            matched = true;
        }
        if !matched {
            output.push(left_row);
        }
    }
    Ok(output)
}
