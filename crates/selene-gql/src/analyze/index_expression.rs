//! One semantic admission service for declarations and optimizer equivalence.

use selene_core::{
    TypeKind,
    scalar_index_expression::{
        ScalarIndexExpression, ScalarIndexOperation as Op, ScalarIndexSelector as Selector,
    },
};

use super::{AnalyzedStatement, BindingId, ExprId, SemanticTree, semantic::ExpressionKind as E};
use crate::{Literal, PipelineStatement, Statement, UnaryOp};

/// Analyze an index expression using the ordinary parser and semantic service.
/// `n` is the sole element binding. This is a Rust extension facility, not DDL
/// grammar. The source must contain exactly one expression, not query clauses.
pub fn analyze_source(source: &str) -> Result<ScalarIndexExpression, &'static str> {
    let statement = crate::parse(&format!("MATCH (n) RETURN {source}"))
        .map_err(|_| "invalid_expression_source")?;
    let analyzed = super::analyze(statement, &crate::EmptyProcedureRegistry, None)
        .map_err(|_| "invalid_expression_bindings")?;
    if !analyzed.parameters.is_empty() {
        return Err("parameterized_expression_target");
    }
    if !analyzed.calls.is_empty() || analyzed.write_set.is_some() {
        return Err("effectful_expression_target");
    }
    let Statement::Query(query) = analyzed.source() else {
        return Err("non_scalar_expression_target");
    };
    let [PipelineStatement::Match(_), PipelineStatement::Return(ret)] = query.statements.as_slice()
    else {
        return Err("non_scalar_expression_target");
    };
    if ret.star
        || ret.distinct
        || ret.group_by.is_some()
        || ret.having.is_some()
        || ret.items.len() != 1
    {
        return Err("non_scalar_expression_target");
    }
    let (_, expression) =
        from_source(&analyzed, &ret.items[0].expr).ok_or("unsupported_expression_target")?;
    if expression.operations.is_empty() {
        return Err("use_property_index_for_bare_property");
    }
    Ok(expression)
}

/// Canonicalize only an already-resolved expression occurrence. Source spellings,
/// casts, selectors, and functions outside this bounded proof remain scan-only.
pub(crate) fn from_source(
    analyzed: &AnalyzedStatement,
    source: &crate::ValueExpr,
) -> Option<(BindingId, ScalarIndexExpression)> {
    let root = analyzed.expression(source)?;
    let result = compile(analyzed.semantics(), root.id, 0)?;
    result.1.is_valid().then_some(result)
}

fn compile(
    tree: &SemanticTree,
    id: ExprId,
    depth: usize,
) -> Option<(BindingId, ScalarIndexExpression)> {
    if depth > 16 {
        return None;
    }
    let node = tree.expressions.get(id.get() as usize)?;
    // Dynamic is not assumed scalar: the closed operation vocabulary below is
    // the proof, with runtime coverage guarding data-dependent type failures.
    if !matches!(
        tree.expr_types.structural_type(id).kind(),
        TypeKind::Dynamic | TypeKind::Scalar(_)
    ) {
        return None;
    }
    match &node.kind {
        E::Property(property) => {
            let [child] = node.children.as_slice() else {
                return None;
            };
            let E::Binding(binding) = tree.expressions.get(child.get() as usize)?.kind else {
                return None;
            };
            Some((
                binding,
                ScalarIndexExpression {
                    semantics: 1,
                    property: property.to_string(),
                    operations: Vec::new(),
                },
            ))
        }
        E::Function {
            name,
            star: false,
            distinct: false,
        } if name.len() == 1 => {
            let name = name.first().as_str().to_ascii_lowercase();
            let (first, rest) = node.children.split_first()?;
            let (binding, mut expression) = compile(tree, *first, depth + 1)?;
            let operation = match name.as_str() {
                "lower" if rest.is_empty() => Op::Lower,
                "upper" if rest.is_empty() => Op::Upper,
                "json_get_path_scalar" => Op::JsonScalarPath(selectors(tree, rest)?),
                "json_get_path_text" => Op::JsonTextPath(selectors(tree, rest)?),
                _ => return None,
            };
            expression.operations.push(operation);
            Some((binding, expression))
        }
        _ => None,
    }
}

fn selectors(tree: &SemanticTree, ids: &[ExprId]) -> Option<Vec<Selector>> {
    if !(1..=64).contains(&ids.len()) {
        return None;
    }
    if let [id] = ids {
        let node = tree.expressions.get(id.get() as usize)?;
        let document = matches!(&node.kind, E::Cast(crate::GqlType::Json))
            || matches!(&node.kind, E::Function { name, star: false, distinct: false }
                if name.len() == 1 && matches!(name.first().as_str().to_ascii_lowercase().as_str(), "json" | "json_parse"));
        if document
            && let [child] = node.children.as_slice()
            && let E::Literal(Literal::String(text, _, _)) =
                &tree.expressions.get(child.get() as usize)?.kind
        {
            let json = selene_core::JsonValue::parse_str(text.as_str()).ok()?;
            let values = json.as_serde().as_array()?;
            if !(1..=64).contains(&values.len()) {
                return None;
            }
            return values
                .iter()
                .map(|value| match value {
                    serde_json::Value::String(key) => Some(Selector::Key(key.clone())),
                    serde_json::Value::Number(number) => number.as_i64().map(Selector::Index),
                    _ => None,
                })
                .collect();
        }
    }
    ids.iter()
        .map(|id| {
            let node = tree.expressions.get(id.get() as usize)?;
            match &node.kind {
                E::Literal(Literal::String(key, _, _)) => Some(Selector::Key(key.to_string())),
                E::Literal(Literal::Integer(index, _)) => Some(Selector::Index(*index)),
                E::Unary(UnaryOp::Negate) => {
                    let [child] = node.children.as_slice() else {
                        return None;
                    };
                    let E::Literal(Literal::Integer(index, _)) =
                        tree.expressions.get(child.get() as usize)?.kind
                    else {
                        return None;
                    };
                    index.checked_neg().map(Selector::Index)
                }
                _ => None,
            }
        })
        .collect()
}
