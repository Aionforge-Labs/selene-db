//! Source expressions are resolved against the frozen semantic predicate IDs.
//! Property tests apply to each element; a group WHERE qualifies the complete
//! group at transition exit, never an already selected topological shortest path.

use super::{BoundedPathProgram, compile::invalid, state::SearchState};
use crate::{
    AnalyzedStatement, GraphPattern, PathAutomaton, PathSemanticElement, PatternElement, ValueExpr,
    runtime::{Binding, EvalCtx, ExecutorError, evaluator},
};
use selene_core::{DbString, Value};

pub(super) struct Conditions {
    properties: Vec<(DbString, ValueExpr)>,
    inline: Option<ValueExpr>,
}

impl Conditions {
    pub(super) fn is_empty(&self) -> bool {
        self.properties.is_empty() && self.inline.is_none()
    }
}

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
    if source.span != automaton.origin || source.elements.len() != automaton.semantic.elements.len()
    {
        return Err(invalid("product path source/semantic shape mismatch"));
    }
    source
        .elements
        .iter()
        .zip(&automaton.semantic.elements)
        .map(|(source, semantic)| {
            let (properties, inline, ids, inline_id) = match (source, semantic) {
                (PatternElement::Node(n), PathSemanticElement::Node(t)) => (
                    n.properties.as_slice(),
                    n.inline_where.as_ref(),
                    &t.property_predicates,
                    t.inline_where,
                ),
                (PatternElement::Edge(e), PathSemanticElement::Edge(t)) => (
                    e.properties.as_slice(),
                    e.inline_where.as_ref(),
                    &t.property_predicates,
                    t.inline_where,
                ),
                _ => return Err(invalid("product path source/semantic element mismatch")),
            };
            if properties.len() != ids.len()
                || properties
                    .iter()
                    .zip(ids)
                    .any(|((_, e), id)| analyzed.expr_ids.get(e) != Some(*id))
                || inline.and_then(|e| analyzed.expr_ids.get(e)) != inline_id
            {
                return Err(invalid("product path predicate identity mismatch"));
            }
            for expr in properties.iter().map(|(_, e)| e).chain(inline) {
                reject_subqueries(expr)?;
            }
            Ok(Conditions {
                properties: properties.to_vec(),
                inline: inline.cloned(),
            })
        })
        .collect()
}

fn reject_subqueries(expr: &ValueExpr) -> Result<(), ExecutorError> {
    if matches!(
        expr,
        ValueExpr::Exists { .. } | ValueExpr::ValueSubquery { .. }
    ) {
        return Err(ExecutorError::FeatureNotSupportedYet {
            feature: "product path expression subqueries",
            span: expr.span(),
        });
    }
    let mut result = Ok(());
    expr.for_each_child(&mut |child| {
        if result.is_ok() {
            result = reject_subqueries(child);
        }
    });
    result
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
