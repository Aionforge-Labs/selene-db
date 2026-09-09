//! Mutation pipeline builders.

use pest::iterators::Pair;

use crate::{
    ast::{
        DeleteMode, DeleteStatement, EdgeDirection, EdgePattern, GraphPattern, InsertStatement,
        MutationPipeline, MutationStatement, MutationTerminator, NodePattern, PatternElement,
        RemoveItem, SetItem, util::NonEmpty,
    },
    error::ParserError,
};

use super::{
    Rule, build_filter, build_return_clause, db_string_pair, expr, first_child, not_implemented,
    pattern, span, unexpected_pair,
};

pub(super) fn build_mutation_pipeline(
    pair: Pair<'_, Rule>,
) -> Result<MutationPipeline, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::mutation_pipeline);
    let source_span = span(&pair);
    let mut statements = Vec::new();
    let mut terminator = None;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::match_stmt => {
                statements.push(MutationStatement::Match(pattern::build_match_clause(
                    child,
                )?));
            }
            Rule::filter_stmt => {
                statements.push(MutationStatement::Filter(build_filter(child)?));
            }
            Rule::mutation_op => statements.push(build_mutation_op(child)?),
            Rule::return_stmt => {
                terminator = Some(MutationTerminator::Return(build_return_clause(child)?));
            }
            Rule::finish_stmt => terminator = Some(MutationTerminator::Finish(span(&child))),
            _ => return Err(unexpected_pair(child, "unexpected mutation-pipeline child")),
        }
    }

    Ok(MutationPipeline {
        statements: NonEmpty::try_from_vec(statements)
            .expect("grammar guarantees mutation_op+ >= 1"),
        terminator,
        span: source_span,
    })
}

fn build_mutation_op(pair: Pair<'_, Rule>) -> Result<MutationStatement, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::mutation_op);
    let inner = first_child(pair)?;
    match inner.as_rule() {
        Rule::insert_op => build_insert(inner).map(MutationStatement::Insert),
        Rule::set_stmt => build_set_items(inner).map(MutationStatement::Set),
        Rule::remove_stmt => build_remove_items(inner).map(MutationStatement::Remove),
        Rule::detach_delete_op => {
            build_delete(inner, DeleteMode::Detach).map(MutationStatement::Delete)
        }
        Rule::delete_op => build_delete_op(inner).map(MutationStatement::Delete),
        Rule::merge_op => Err(not_implemented(&inner, "MERGE is deferred")),
        _ => Err(unexpected_pair(inner, "expected mutation operation")),
    }
}

fn build_insert(pair: Pair<'_, Rule>) -> Result<InsertStatement, ParserError> {
    let source_span = span(&pair);
    let graph_pair = pair
        .into_inner()
        .find(|child| child.as_rule() == Rule::insert_graph_pattern)
        .ok_or_else(|| ParserError::syntax("INSERT is missing graph pattern", source_span, None))?;
    Ok(InsertStatement {
        patterns: build_insert_graph_pattern(graph_pair)?,
        span: source_span,
    })
}

fn build_insert_graph_pattern(pair: Pair<'_, Rule>) -> Result<Vec<GraphPattern>, ParserError> {
    pair.into_inner()
        .filter(|child| child.as_rule() == Rule::insert_path_pattern)
        .map(|child| build_insert_path_pattern(child))
        .collect()
}

fn build_insert_path_pattern(pair: Pair<'_, Rule>) -> Result<GraphPattern, ParserError> {
    let source_span = span(&pair);
    let mut elements = Vec::new();
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::insert_node_pattern => {
                elements.push(PatternElement::Node(build_insert_node_pattern(child)?));
            }
            Rule::insert_edge_pattern => {
                elements.push(PatternElement::Edge(build_insert_edge_pattern(child)?));
            }
            _ => return Err(unexpected_pair(child, "unexpected INSERT path child")),
        }
    }
    Ok(GraphPattern {
        path_binding: None,
        elements,
        span: source_span,
    })
}

fn build_insert_node_pattern(pair: Pair<'_, Rule>) -> Result<NodePattern, ParserError> {
    let source_span = span(&pair);
    let mut binding = None;
    let mut label_expr = None;
    let mut properties = Vec::new();

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::ident => binding = Some(db_string_pair(child)?),
            Rule::insert_label_set => {
                label_expr = Some(pattern::build_label_expr(first_child(child)?)?);
            }
            Rule::property_map => properties = pattern::build_property_map(child)?,
            _ => return Err(unexpected_pair(child, "unexpected INSERT node child")),
        }
    }

    Ok(NodePattern {
        binding,
        label_expr,
        properties,
        inline_where: None,
        span: source_span,
    })
}

fn build_insert_edge_pattern(pair: Pair<'_, Rule>) -> Result<EdgePattern, ParserError> {
    let source_span = span(&pair);
    let inner = first_child(pair)?;
    let direction = match inner.as_rule() {
        Rule::insert_edge_right => EdgeDirection::Right,
        Rule::insert_edge_left => EdgeDirection::Left,
        Rule::insert_edge_undirected => EdgeDirection::Undirected,
        _ => return Err(unexpected_pair(inner, "expected INSERT edge pattern")),
    };
    let mut pattern = EdgePattern {
        binding: None,
        direction,
        abbreviated: false,
        label_expr: None,
        properties: Vec::new(),
        quantifier: None,
        inline_where: None,
        span: source_span,
    };

    for child in inner.into_inner() {
        match child.as_rule() {
            Rule::edge_var => pattern.binding = Some(db_string_pair(first_child(child)?)?),
            Rule::label_expr => pattern.label_expr = Some(pattern::build_label_expr(child)?),
            Rule::property_map => pattern.properties = pattern::build_property_map(child)?,
            _ => return Err(unexpected_pair(child, "unexpected INSERT edge child")),
        }
    }

    Ok(pattern)
}

fn build_set_items(pair: Pair<'_, Rule>) -> Result<Vec<SetItem>, ParserError> {
    pair.into_inner()
        .filter(|child| child.as_rule() == Rule::set_item)
        .map(|child| build_set_item(child))
        .collect()
}

fn build_set_item(pair: Pair<'_, Rule>) -> Result<SetItem, ParserError> {
    let inner = first_child(pair)?;
    let source_span = span(&inner);
    let rule = inner.as_rule();
    let mut children = inner.into_inner();
    match rule {
        Rule::set_property_item => {
            let target =
                next_db_string(&mut children, source_span, "SET property is missing target")?;
            let key = next_db_string(&mut children, source_span, "SET property is missing key")?;
            let value_pair = children.next().ok_or_else(|| {
                ParserError::syntax("SET property is missing value", source_span, None)
            })?;
            Ok(SetItem::Property {
                target,
                key,
                value: expr::build_value_expr(value_pair)?,
                span: source_span,
            })
        }
        Rule::set_all_properties_item => {
            let target = next_db_string(&mut children, source_span, "SET map is missing target")?;
            let map_pair = children.next().ok_or_else(|| {
                ParserError::syntax("SET map is missing property map", source_span, None)
            })?;
            Ok(SetItem::PropertyMerge {
                target,
                properties: pattern::build_property_map(map_pair)?,
                span: source_span,
            })
        }
        Rule::set_label_item => {
            let target = next_db_string(&mut children, source_span, "SET label is missing target")?;
            let label = next_db_string(&mut children, source_span, "SET label is missing label")?;
            Ok(SetItem::Label {
                target,
                label,
                span: source_span,
            })
        }
        _ => Err(ParserError::syntax("expected SET item", source_span, None)),
    }
}

fn build_remove_items(pair: Pair<'_, Rule>) -> Result<Vec<RemoveItem>, ParserError> {
    pair.into_inner()
        .filter(|child| child.as_rule() == Rule::remove_item)
        .map(|child| build_remove_item(child))
        .collect()
}

fn build_remove_item(pair: Pair<'_, Rule>) -> Result<RemoveItem, ParserError> {
    let inner = first_child(pair)?;
    let source_span = span(&inner);
    let rule = inner.as_rule();
    let mut children = inner.into_inner();
    match rule {
        Rule::remove_property_item => {
            let target = next_db_string(
                &mut children,
                source_span,
                "REMOVE property is missing target",
            )?;
            let key = next_db_string(&mut children, source_span, "REMOVE property is missing key")?;
            Ok(RemoveItem::Property {
                target,
                key,
                span: source_span,
            })
        }
        Rule::remove_label_item => {
            let target =
                next_db_string(&mut children, source_span, "REMOVE label is missing target")?;
            let label =
                next_db_string(&mut children, source_span, "REMOVE label is missing label")?;
            Ok(RemoveItem::Label {
                target,
                label,
                span: source_span,
            })
        }
        _ => Err(ParserError::syntax(
            "expected REMOVE item",
            source_span,
            None,
        )),
    }
}

fn build_delete_op(pair: Pair<'_, Rule>) -> Result<DeleteStatement, ParserError> {
    let source_span = span(&pair);
    // Single pass over the children: detect the `NODETACH` keyword and collect
    // delete-target identifiers without cloning the pest pair.
    let mut mode = DeleteMode::Bare;
    let mut items = Vec::new();
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::nodetach_kw => mode = DeleteMode::NoDetach,
            Rule::ident => items.push(db_string_pair(child)?),
            _ => {}
        }
    }
    finish_delete(mode, items, source_span)
}

fn build_delete(pair: Pair<'_, Rule>, mode: DeleteMode) -> Result<DeleteStatement, ParserError> {
    let source_span = span(&pair);
    let items = pair
        .into_inner()
        .filter(|child| child.as_rule() == Rule::ident)
        .map(|child| db_string_pair(child))
        .collect::<Result<Vec<_>, _>>()?;
    finish_delete(mode, items, source_span)
}

fn finish_delete(
    mode: DeleteMode,
    items: Vec<selene_core::DbString>,
    source_span: crate::ast::SourceSpan,
) -> Result<DeleteStatement, ParserError> {
    if items.is_empty() {
        return Err(ParserError::syntax(
            "DELETE is missing target",
            source_span,
            None,
        ));
    }
    Ok(DeleteStatement {
        mode,
        items,
        span: source_span,
    })
}

fn next_db_string(
    children: &mut pest::iterators::Pairs<'_, Rule>,
    source_span: crate::ast::SourceSpan,
    missing: &'static str,
) -> Result<selene_core::DbString, ParserError> {
    children
        .next()
        .ok_or_else(|| ParserError::syntax(missing, source_span, None))
        .and_then(|pair| db_string_pair(pair))
}
