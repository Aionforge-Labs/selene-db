//! Mutation-pipeline bind handling.

use crate::{
    DeleteStatement, ElementKind, MutationPipeline, MutationStatement, MutationTerminator,
    RemoveItem, SetItem,
    analyze::{
        binding::BindingUseKind,
        error::{AnalysisError, ConditionClause},
        write_set::WriteKind,
    },
};

use super::{BindContext, expr, pattern, query};

pub(crate) fn bind_mutation_pipeline(
    ctx: &mut BindContext,
    pipeline: &MutationPipeline,
) -> Result<(), AnalysisError> {
    ctx.use_working_graph(pipeline.span)?;
    for (statement_index, statement) in pipeline.statements.iter().enumerate() {
        bind_mutation_statement(ctx, statement_index, statement)?;
    }
    if let Some(terminator) = &pipeline.terminator {
        match terminator {
            MutationTerminator::Return(clause) => query::bind_return_clause(ctx, clause)?,
            MutationTerminator::Finish(_) => {}
        }
    }
    Ok(())
}

fn bind_mutation_statement(
    ctx: &mut BindContext,
    statement_index: usize,
    statement: &MutationStatement,
) -> Result<(), AnalysisError> {
    match statement {
        MutationStatement::Match(clause) => pattern::bind_match_clause(ctx, clause),
        MutationStatement::Filter(value) => {
            expr::bind_condition(ctx, value, ConditionClause::Filter)?;
            Ok(())
        }
        MutationStatement::Insert(insert) => {
            for graph_pattern in &insert.patterns {
                pattern::bind_insert_graph_pattern(ctx, statement_index, graph_pattern)?;
            }
            Ok(())
        }
        MutationStatement::Set(items) => bind_set_items(ctx, statement_index, items),
        MutationStatement::Remove(items) => bind_remove_items(ctx, statement_index, items),
        MutationStatement::Delete(statement) => bind_delete(ctx, statement_index, statement),
    }
}

fn bind_set_items(
    ctx: &mut BindContext,
    statement_index: usize,
    items: &[SetItem],
) -> Result<(), AnalysisError> {
    for item in items {
        match item {
            SetItem::Property {
                target,
                key,
                value,
                span,
            } => {
                let target = ctx.resolve(target.clone(), *span, BindingUseKind::SetTarget)?;
                expr::bind_value_expr(ctx, value)?;
                let element = ctx.element_kind(target);
                ctx.record_write(
                    statement_index,
                    *span,
                    WriteKind::SetProperty {
                        target,
                        element,
                        key: key.clone(),
                        value_span: value.span(),
                    },
                );
            }
            SetItem::PropertyMerge {
                target,
                properties,
                span,
            } => {
                let target = ctx.resolve(target.clone(), *span, BindingUseKind::SetTarget)?;
                let element = ctx.element_kind(target);
                for (_, value) in properties {
                    expr::bind_value_expr(ctx, value)?;
                }
                for (key, value) in properties {
                    ctx.record_write(
                        statement_index,
                        *span,
                        WriteKind::SetProperty {
                            target,
                            element,
                            key: key.clone(),
                            value_span: value.span(),
                        },
                    );
                }
            }
            SetItem::Label {
                target,
                label,
                span,
            } => {
                let target = ctx.resolve(target.clone(), *span, BindingUseKind::SetTarget)?;
                let element = ctx.element_kind(target);
                ctx.record_write(
                    statement_index,
                    *span,
                    WriteKind::SetLabel {
                        target,
                        element,
                        label: label.clone(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn bind_remove_items(
    ctx: &mut BindContext,
    statement_index: usize,
    items: &[RemoveItem],
) -> Result<(), AnalysisError> {
    for item in items {
        match item {
            RemoveItem::Property { target, key, span } => {
                let target = ctx.resolve(target.clone(), *span, BindingUseKind::RemoveTarget)?;
                let element = ctx.element_kind(target);
                ctx.record_write(
                    statement_index,
                    *span,
                    WriteKind::RemoveProperty {
                        target,
                        element,
                        key: key.clone(),
                    },
                );
            }
            RemoveItem::Label {
                target,
                label,
                span,
            } => {
                let target = ctx.resolve(target.clone(), *span, BindingUseKind::RemoveTarget)?;
                let element = ctx.element_kind(target);
                if matches!(element, ElementKind::Edge) {
                    return Err(AnalysisError::NotImplemented {
                        message: "REMOVE label on edge is not supported; edge labels are immutable"
                            .to_owned(),
                        span: *span,
                        hint: None,
                    });
                }
                ctx.record_write(
                    statement_index,
                    *span,
                    WriteKind::RemoveLabel {
                        target,
                        element,
                        label: label.clone(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn bind_delete(
    ctx: &mut BindContext,
    statement_index: usize,
    statement: &DeleteStatement,
) -> Result<(), AnalysisError> {
    for item in &statement.items {
        let target = ctx.resolve(item.clone(), statement.span, BindingUseKind::DeleteTarget)?;
        let element = ctx.element_kind(target);
        if matches!(element, ElementKind::Path | ElementKind::Alias) {
            return Err(AnalysisError::InvalidReference {
                message: "DELETE target must be a node or edge binding".to_owned(),
                span: statement.span,
            });
        }
        ctx.record_write(
            statement_index,
            statement.span,
            WriteKind::DeleteTarget {
                target,
                element,
                mode: statement.mode,
            },
        );
    }
    Ok(())
}
