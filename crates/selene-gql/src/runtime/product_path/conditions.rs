//! Source expressions are resolved against the frozen semantic predicate IDs.
//! Property tests apply to each element; a group WHERE qualifies the complete
//! group at transition exit, never an already selected topological shortest path.

use super::{BoundedPathProgram, compile::invalid, state::SearchState};
pub(super) use crate::PathConditions as Conditions;
use crate::{
    AnalyzedStatement, GraphPattern, PathAutomaton,
    runtime::{Binding, EvalCtx, ExecutorError, evaluator},
};
use selene_core::Value;

#[derive(Clone, Copy)]
pub(super) enum Phase {
    Node,
    EdgeHop,
    EdgeExit,
}

pub(super) type Qualifier<'a> =
    dyn Fn(&SearchState, &Value, Phase) -> Result<bool, ExecutorError> + 'a;

pub(super) fn compile(
    source: &GraphPattern,
    automaton: &PathAutomaton,
    analyzed: &AnalyzedStatement,
) -> Result<Vec<Conditions>, ExecutorError> {
    crate::plan::lowering::path_program::conditions(source, automaton, analyzed)
        .map_err(|_| invalid("product path predicate identity mismatch"))
}

pub(super) fn evaluate(
    program: &BoundedPathProgram<'_>,
    state: &SearchState,
    entity: &Value,
    phase: Phase,
    eval: &EvalCtx<'_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    let conditions = &program.paths[state.pattern].conditions[state.element];
    let properties = !matches!(phase, Phase::EdgeExit);
    let inline = !matches!(phase, Phase::EdgeHop);
    if (!properties || conditions.properties.is_empty()) && (!inline || conditions.inline.is_none())
    {
        return Ok(true);
    }
    eval.tx.check_cancellation()?;
    let row = Binding::new(
        state
            .locals
            .iter()
            .map(|v| v.clone().unwrap_or(Value::Null)),
    );
    if properties {
        for (key, expr) in &conditions.properties {
            let props = match entity {
                Value::NodeRef(id) => eval.tx.snapshot().node_properties(*id),
                Value::EdgeRef(id) => eval.tx.snapshot().edge_properties(*id),
                _ => None,
            };
            let actual = props.and_then(|p| p.get(key)).unwrap_or(&Value::Null);
            let expected = evaluator::evaluate(expr, &row, &program.schema, eval)?;
            if matches!(actual, Value::Null)
                || matches!(expected, Value::Null)
                || !crate::runtime::value_compare::equal_non_null(actual, &expected)
            {
                return Ok(false);
            }
        }
    }
    if inline && let Some(expr) = &conditions.inline {
        return Ok(matches!(
            evaluator::evaluate(expr, &row, &program.schema, eval)?,
            Value::Bool(true)
        ));
    }
    Ok(true)
}
