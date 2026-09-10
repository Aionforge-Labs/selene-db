//! Mutation-pipeline lowering.

use std::collections::HashMap;

use crate::{
    InsertStatement, MatchClause, MutationPipeline, MutationStatement, MutationTerminator,
    NodePattern, PatternElement, RemoveItem, SetItem, SourceSpan, ValueExpr,
    analyze::{
        AnalyzedStatement, BindingDeclKind, BindingId, BindingUseKind, ElementKind,
        MutationWriteSet, WriteKind, WriteSetEntry,
    },
    plan::{
        BindingTableColumn, BindingTableSchema, DeleteTargetPlan, ExecutionPlan, ImplDefinedCaps,
        InsertEndpointRef, InsertSiteId, MutationOp, PipelineOp, PlannerError, PropertyInit,
    },
};

use super::{expr, match_clause, sequential_match, visible_after_pattern};

/// Lower a mutation pipeline into an execution plan.
pub(crate) fn lower_mutation(
    pipeline: &MutationPipeline,
    analyzed: &AnalyzedStatement,
    max_quantifier: u32,
) -> Result<ExecutionPlan, PlannerError> {
    let write_set = analyzed
        .write_set
        .as_ref()
        .ok_or(PlannerError::WriteSetMissing {
            span: pipeline.span,
        })?;

    let (mut pattern_plan, prefix_filters, mutation_start) =
        lower_read_prefix(&pipeline.statements, analyzed, max_quantifier)?;
    let mut visible = visible_after_pattern(pattern_plan.as_ref());
    let mut ops = Vec::new();
    if let Some(pattern) = pattern_plan.as_mut() {
        pattern.filters.extend(prefix_filters);
    } else {
        ops.extend(prefix_filters.into_iter().map(PipelineOp::Filter));
    }

    let mut cursor = 0usize;
    let mut ids = InsertSiteIdAlloc::default();
    for statement in &pipeline.statements[mutation_start..] {
        match statement {
            MutationStatement::Match(clause) => {
                sequential_match::lower(clause, analyzed, &mut ops, &mut visible, max_quantifier)?;
            }
            MutationStatement::Filter(value) => {
                ops.push(PipelineOp::Filter(expr::filter_predicate(value, analyzed)?));
            }
            MutationStatement::Insert(insert) => lower_insert(
                insert,
                write_set,
                &mut cursor,
                &mut ids,
                analyzed,
                &mut visible,
                &mut ops,
            )?,
            MutationStatement::Set(items) => {
                lower_set_items(items, write_set, &mut cursor, analyzed, &visible, &mut ops)?;
            }
            MutationStatement::Remove(items) => {
                lower_remove_items(items, write_set, &mut cursor, analyzed, &visible, &mut ops)?;
            }
            MutationStatement::Delete(statement) => {
                let mut targets = Vec::with_capacity(statement.items.len());
                for _ in &statement.items {
                    let entry = consume_entry(write_set, &mut cursor, statement.span)?;
                    let WriteKind::DeleteTarget {
                        target,
                        element,
                        mode: _,
                    } = entry.kind
                    else {
                        return Err(mismatch(entry.span));
                    };
                    let target_column_index =
                        mutation_target_column_index(target, element, analyzed, &visible)?;
                    targets.push(DeleteTargetPlan {
                        target,
                        element,
                        target_column_index,
                    });
                }
                ops.push(PipelineOp::Mutation(MutationOp::DeleteTargets {
                    targets,
                    mode: statement.mode,
                    span: statement.span,
                }));
            }
        }
    }
    if let Some(entry) = write_set.entries.get(cursor) {
        return Err(mismatch(entry.span));
    }

    if let Some(terminator) = &pipeline.terminator {
        match terminator {
            MutationTerminator::Return(clause) => {
                // A data-modifying statement's RETURN has no ordering-and-page
                // statement after it, so there is never a sort carrier here.
                // No sort terms here, so no carrier is ever allocated; the
                // counter is a formality.
                let mut next_expr_id = super::next_expr_id(analyzed);
                super::lower_return(
                    clause,
                    analyzed,
                    &mut ops,
                    &mut visible,
                    None,
                    &mut next_expr_id,
                )?;
            }
            MutationTerminator::Finish(_) => visible.clear(),
        }
    } else {
        visible.clear();
    }

    let next_pipeline_op_id = crate::PipelineOpId::new(ops.len() as u32);
    Ok(ExecutionPlan {
        category: analyzed.category,
        pattern_plan,
        pipeline: ops,
        output_schema: BindingTableSchema { columns: visible },
        impl_defined_caps: ImplDefinedCaps::default(),
        expr_ids: analyzed.expr_ids.clone(),
        subqueries: Default::default(),
        next_expr_id: super::next_expr_id(analyzed),
        next_pipeline_op_id,
    })
}

mod helpers;
use helpers::*;

fn lower_read_prefix(
    statements: &[MutationStatement],
    analyzed: &AnalyzedStatement,
    max_quantifier: u32,
) -> Result<
    (
        Option<crate::plan::PatternPlan>,
        Vec<crate::plan::FilterPredicate>,
        usize,
    ),
    PlannerError,
> {
    let mut matches: Vec<&MatchClause> = Vec::new();
    let mut filters = Vec::new();
    for (index, statement) in statements.iter().enumerate() {
        match statement {
            MutationStatement::Match(clause) => matches.push(clause),
            MutationStatement::Filter(value) => {
                filters.push(expr::filter_predicate(value, analyzed)?);
            }
            _ => {
                let pattern_plan =
                    match_clause::lower_match_prefix(&matches, analyzed, max_quantifier)?;
                return Ok((pattern_plan, filters, index));
            }
        }
    }
    let pattern_plan = match_clause::lower_match_prefix(&matches, analyzed, max_quantifier)?;
    Ok((pattern_plan, filters, statements.len()))
}

fn lower_insert(
    insert: &InsertStatement,
    write_set: &MutationWriteSet,
    cursor: &mut usize,
    ids: &mut InsertSiteIdAlloc,
    analyzed: &AnalyzedStatement,
    visible: &mut Vec<BindingTableColumn>,
    ops: &mut Vec<PipelineOp>,
) -> Result<(), PlannerError> {
    for pattern in &insert.patterns {
        let mut sites = HashMap::new();
        for (index, element) in pattern.elements.iter().enumerate() {
            if classify_insert_element(element, analyzed)? == InsertSiteEmission::Emitted {
                sites.insert(index, ids.alloc());
            }
        }

        let mut pending = Vec::new();
        for (index, element) in pattern.elements.iter().enumerate() {
            let Some(site_id) = sites.get(&index).copied() else {
                continue;
            };
            match element {
                PatternElement::Node(node) => {
                    let entry = consume_entry(write_set, cursor, node.span)?;
                    let WriteKind::InsertNode {
                        binding,
                        label_expr,
                        property_keys: _,
                    } = entry.kind
                    else {
                        return Err(mismatch(entry.span));
                    };
                    pending.push(PendingInsert::Node {
                        site_id,
                        binding,
                        label_expr,
                        property_inits: property_inits(&node.properties, analyzed)?,
                        span: node.span,
                    });
                }
                PatternElement::Edge(edge) => {
                    let entry = consume_entry(write_set, cursor, edge.span)?;
                    let WriteKind::InsertEdge {
                        binding,
                        label_expr,
                        property_keys: _,
                    } = entry.kind
                    else {
                        return Err(mismatch(entry.span));
                    };
                    pending.push(PendingInsert::Edge {
                        index,
                        site_id,
                        binding,
                        label_expr,
                        direction: edge.direction,
                        property_inits: property_inits(&edge.properties, analyzed)?,
                        span: edge.span,
                    });
                }
            }
        }

        for item in &pending {
            if let PendingInsert::Node {
                site_id,
                binding,
                label_expr,
                property_inits,
                span,
            } = item
            {
                let (output_column_index, output_column) =
                    push_insert_output(*binding, analyzed, visible)?;
                ops.push(PipelineOp::Mutation(MutationOp::InsertNode {
                    site_id: *site_id,
                    binding: *binding,
                    label_expr: label_expr.clone(),
                    property_inits: property_inits.clone(),
                    output_column_index,
                    output_column: output_column.clone(),
                    span: *span,
                }));
            }
        }
        for item in pending {
            if let PendingInsert::Edge {
                index,
                site_id,
                binding,
                label_expr,
                direction,
                property_inits,
                span,
            } = item
            {
                let left = endpoint_ref(
                    pattern
                        .elements
                        .get(index.wrapping_sub(1))
                        .ok_or_else(|| mismatch(span))?,
                    index.wrapping_sub(1),
                    &sites,
                    analyzed,
                    visible,
                    span,
                )?;
                let right = endpoint_ref(
                    pattern
                        .elements
                        .get(index + 1)
                        .ok_or_else(|| mismatch(span))?,
                    index + 1,
                    &sites,
                    analyzed,
                    visible,
                    span,
                )?;
                let (output_column_index, output_column) =
                    push_insert_output(binding, analyzed, visible)?;
                ops.push(PipelineOp::Mutation(MutationOp::InsertEdge {
                    site_id,
                    binding,
                    label_expr,
                    left,
                    right,
                    direction,
                    property_inits,
                    output_column_index,
                    output_column,
                    span,
                }));
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
enum PendingInsert {
    Node {
        site_id: InsertSiteId,
        binding: Option<BindingId>,
        label_expr: Option<crate::LabelExpr>,
        property_inits: Vec<PropertyInit>,
        span: SourceSpan,
    },
    Edge {
        index: usize,
        site_id: InsertSiteId,
        binding: Option<BindingId>,
        label_expr: Option<crate::LabelExpr>,
        direction: crate::EdgeDirection,
        property_inits: Vec<PropertyInit>,
        span: SourceSpan,
    },
}

fn lower_set_items(
    items: &[SetItem],
    write_set: &MutationWriteSet,
    cursor: &mut usize,
    analyzed: &AnalyzedStatement,
    visible: &[BindingTableColumn],
    ops: &mut Vec<PipelineOp>,
) -> Result<(), PlannerError> {
    for item in items {
        match item {
            SetItem::Property {
                key, value, span, ..
            } => {
                let entry = consume_entry(write_set, cursor, *span)?;
                let WriteKind::SetProperty {
                    target,
                    element,
                    key: entry_key,
                    ..
                } = entry.kind
                else {
                    return Err(mismatch(entry.span));
                };
                if entry_key != *key {
                    return Err(mismatch(entry.span));
                }
                let target_column_index =
                    mutation_target_column_index(target, element, analyzed, visible)?;
                ops.push(PipelineOp::Mutation(MutationOp::SetProperty {
                    target,
                    element,
                    target_column_index,
                    key: key.clone(),
                    value: expr::project_expr(value, None, analyzed)?,
                    span: entry.span,
                }));
            }
            SetItem::PropertyMerge {
                properties, span, ..
            } => {
                for (key, value) in properties {
                    let entry = consume_entry(write_set, cursor, *span)?;
                    let WriteKind::SetProperty {
                        target,
                        element,
                        key: entry_key,
                        ..
                    } = entry.kind
                    else {
                        return Err(mismatch(entry.span));
                    };
                    if entry_key != *key {
                        return Err(mismatch(entry.span));
                    }
                    let target_column_index =
                        mutation_target_column_index(target, element, analyzed, visible)?;
                    ops.push(PipelineOp::Mutation(MutationOp::SetProperty {
                        target,
                        element,
                        target_column_index,
                        key: key.clone(),
                        value: expr::project_expr(value, None, analyzed)?,
                        span: entry.span,
                    }));
                }
            }
            SetItem::Label { label, span, .. } => {
                let entry = consume_entry(write_set, cursor, *span)?;
                let WriteKind::SetLabel {
                    target,
                    element,
                    label: entry_label,
                } = entry.kind
                else {
                    return Err(mismatch(entry.span));
                };
                if entry_label != *label {
                    return Err(mismatch(entry.span));
                }
                let target_column_index =
                    mutation_target_column_index(target, element, analyzed, visible)?;
                ops.push(PipelineOp::Mutation(MutationOp::SetLabel {
                    target,
                    element,
                    target_column_index,
                    label: label.clone(),
                    span: entry.span,
                }));
            }
        }
    }
    Ok(())
}

fn lower_remove_items(
    items: &[RemoveItem],
    write_set: &MutationWriteSet,
    cursor: &mut usize,
    analyzed: &AnalyzedStatement,
    visible: &[BindingTableColumn],
    ops: &mut Vec<PipelineOp>,
) -> Result<(), PlannerError> {
    for item in items {
        match item {
            RemoveItem::Property { key, span, .. } => {
                let entry = consume_entry(write_set, cursor, *span)?;
                let WriteKind::RemoveProperty {
                    target,
                    element,
                    key: entry_key,
                } = entry.kind
                else {
                    return Err(mismatch(entry.span));
                };
                if entry_key != *key {
                    return Err(mismatch(entry.span));
                }
                let target_column_index =
                    mutation_target_column_index(target, element, analyzed, visible)?;
                ops.push(PipelineOp::Mutation(MutationOp::RemoveProperty {
                    target,
                    element,
                    target_column_index,
                    key: key.clone(),
                    span: entry.span,
                }));
            }
            RemoveItem::Label { label, span, .. } => {
                let entry = consume_entry(write_set, cursor, *span)?;
                let WriteKind::RemoveLabel {
                    target,
                    element,
                    label: entry_label,
                } = entry.kind
                else {
                    return Err(mismatch(entry.span));
                };
                if entry_label != *label {
                    return Err(mismatch(entry.span));
                }
                let target_column_index =
                    mutation_target_column_index(target, element, analyzed, visible)?;
                ops.push(PipelineOp::Mutation(MutationOp::RemoveLabel {
                    target,
                    element,
                    target_column_index,
                    label: label.clone(),
                    span: entry.span,
                }));
            }
        }
    }
    Ok(())
}

fn property_inits(
    properties: &[(selene_core::DbString, ValueExpr)],
    analyzed: &AnalyzedStatement,
) -> Result<Vec<PropertyInit>, PlannerError> {
    properties
        .iter()
        .map(|(key, value)| {
            Ok(PropertyInit {
                key: key.clone(),
                value: expr::project_expr(value, None, analyzed)?,
                span: value.span(),
            })
        })
        .collect()
}

#[cfg(test)]
mod defensive_tests {
    use super::*;
    use crate::{
        EmptyProcedureRegistry, SourceSpan, analyze::StatementCategory, parse, plan::plan,
    };

    #[test]
    fn missing_write_set_reports_planner_error() {
        let analyzed = crate::analyze(
            parse("INSERT (n)").expect("parses"),
            &EmptyProcedureRegistry,
            None,
        )
        .expect("analyzes");
        let mut broken = analyzed;
        broken.corrupt_for_test(|_, semantic| {
            semantic.write_set = None;
            semantic.category = StatementCategory::DataModifying;
        });
        let err = plan(&broken, &EmptyProcedureRegistry).expect_err("missing write set");
        assert!(matches!(err, PlannerError::WriteSetMissing { .. }));
    }

    #[test]
    fn write_set_shape_mismatch_reports_planner_error() {
        let analyzed = crate::analyze(
            parse("INSERT (n)").expect("parses"),
            &EmptyProcedureRegistry,
            None,
        )
        .expect("analyzes");
        let mut broken = analyzed;
        broken.corrupt_for_test(|_, semantic| {
            semantic.write_set = Some(MutationWriteSet {
                entries: Vec::new(),
            });
            semantic.span = SourceSpan::new(0, 10);
        });
        let err = plan(&broken, &EmptyProcedureRegistry).expect_err("mismatch");
        assert!(matches!(err, PlannerError::WriteSetPatternMismatch { .. }));
    }
}
