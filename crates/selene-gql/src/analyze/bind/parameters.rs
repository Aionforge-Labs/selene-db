//! Typed parameter declaration checks.

use std::collections::BTreeMap;

use selene_core::DbString;

use crate::{
    DdlStatement, ExistsBody, ForStatement, GqlType, IsCheckKind, LimitValue, MatchClause,
    MutationPipeline, MutationStatement, MutationTerminator, PatternElement, PipelineStatement,
    ProcedureCall, QueryPipeline, ReturnClause, ReturnItem, SetItem, SourceSpan, Statement,
    TypePropertyConstraint, ValueExpr,
    analyze::{ParameterUse, error::AnalysisError},
};

pub(super) type DeclarationMap = BTreeMap<DbString, (selene_core::StructuralType, SourceSpan)>;

#[derive(Default)]
struct ParameterCollector {
    declarations: DeclarationMap,
    uses: Vec<ParameterUse>,
}

pub(crate) fn collect_statement_parameters(
    statement: &Statement,
) -> Result<Vec<ParameterUse>, AnalysisError> {
    let mut collector = ParameterCollector::default();
    collect_statement_parameter_declarations(statement, &mut collector)?;
    let ParameterCollector {
        declarations,
        mut uses,
    } = collector;
    for parameter in &mut uses {
        parameter.declared_type = declarations
            .get(&parameter.name)
            .map(|(declared_type, _)| crate::lower_value_type(declared_type));
    }
    uses.sort_by_key(|parameter| (parameter.span.byte_offset, parameter.span.byte_len));
    Ok(uses)
}

pub(crate) fn validate_parameter_declarations(
    pipeline: &QueryPipeline,
) -> Result<(), AnalysisError> {
    let mut collector = ParameterCollector::default();
    collect_pipeline_parameter_declarations(pipeline, &mut collector)
}

fn collect_statement_parameter_declarations(
    statement: &Statement,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    match statement {
        Statement::Query(pipeline) => {
            collect_pipeline_parameter_declarations(pipeline, declarations)
        }
        Statement::Composite { first, rest, .. } => {
            collect_pipeline_parameter_declarations(first, declarations)?;
            for (_, pipeline) in rest {
                collect_pipeline_parameter_declarations(pipeline, declarations)?;
            }
            Ok(())
        }
        Statement::Chained { blocks, .. } => {
            for pipeline in blocks {
                collect_pipeline_parameter_declarations(pipeline, declarations)?;
            }
            Ok(())
        }
        Statement::Mutate(pipeline) => {
            collect_mutation_parameter_declarations(pipeline, declarations)
        }
        Statement::Ddl(statement) => collect_ddl_parameter_declarations(statement, declarations),
        Statement::Call(call) => collect_call_parameter_declarations(call, declarations),
        Statement::Explain { inner, .. } => {
            collect_statement_parameter_declarations(inner, declarations)
        }
        Statement::SessionSetValue { value, .. } => {
            collect_value_parameter_declarations(value, declarations)
        }
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::SessionSetTimeZone { .. }
        | Statement::SessionSetGraph { .. }
        | Statement::SessionReset { .. }
        | Statement::SessionClose { .. } => Ok(()),
    }
}

fn collect_pipeline_parameter_declarations(
    pipeline: &QueryPipeline,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    for statement in &pipeline.statements {
        match statement {
            PipelineStatement::Match(clause) => {
                collect_match_clause_parameter_declarations(clause, declarations)?;
            }
            PipelineStatement::Filter(value)
            | PipelineStatement::For(ForStatement { source: value, .. }) => {
                collect_value_parameter_declarations(value, declarations)?;
            }
            PipelineStatement::Let(bindings) => {
                for binding in bindings {
                    collect_value_parameter_declarations(&binding.value, declarations)?;
                }
            }
            PipelineStatement::Sorting(terms) => {
                for term in terms {
                    collect_value_parameter_declarations(&term.expr, declarations)?;
                }
            }
            PipelineStatement::Limit(value) | PipelineStatement::Offset(value) => {
                collect_limit_parameter_declarations(value, declarations)?;
            }
            PipelineStatement::Return(clause) => {
                collect_return_parameter_declarations(clause, declarations)?;
            }
            PipelineStatement::With(clause) => {
                collect_projection_parameter_declarations(
                    &clause.items,
                    clause.group_by.as_deref(),
                    clause.having.as_ref(),
                    declarations,
                )?;
                if let Some(where_clause) = &clause.where_clause {
                    collect_value_parameter_declarations(where_clause, declarations)?;
                }
            }
            PipelineStatement::Call(call) => {
                collect_call_parameter_declarations(call, declarations)?;
            }
            PipelineStatement::CallSubquery(call) => {
                collect_pipeline_parameter_declarations(&call.body, declarations)?;
            }
        }
    }
    Ok(())
}

fn collect_mutation_parameter_declarations(
    pipeline: &MutationPipeline,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    for statement in &pipeline.statements {
        match statement {
            MutationStatement::Match(clause) => {
                collect_match_clause_parameter_declarations(clause, declarations)?;
            }
            MutationStatement::Filter(value) => {
                collect_value_parameter_declarations(value, declarations)?;
            }
            MutationStatement::Insert(insert) => {
                for pattern in &insert.patterns {
                    collect_graph_pattern_parameter_declarations(pattern, declarations)?;
                }
            }
            MutationStatement::Set(items) => {
                for item in items {
                    match item {
                        SetItem::Property { value, .. } => {
                            collect_value_parameter_declarations(value, declarations)?;
                        }
                        SetItem::PropertyMerge { properties, .. } => {
                            for (_, value) in properties {
                                collect_value_parameter_declarations(value, declarations)?;
                            }
                        }
                        SetItem::Label { .. } => {}
                    }
                }
            }
            MutationStatement::Remove(_) | MutationStatement::Delete(_) => {}
        }
    }
    if let Some(MutationTerminator::Return(clause)) = &pipeline.terminator {
        collect_return_parameter_declarations(clause, declarations)?;
    }
    Ok(())
}

fn collect_ddl_parameter_declarations(
    statement: &DdlStatement,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    match statement {
        DdlStatement::CreateNodeType { properties, .. }
        | DdlStatement::CreateEdgeType { properties, .. }
        | DdlStatement::AlterNodeType { properties, .. }
        | DdlStatement::AlterEdgeType { properties, .. } => {
            for property in properties {
                for constraint in &property.constraints {
                    if let TypePropertyConstraint::Default(value, _) = constraint {
                        collect_value_parameter_declarations(value, declarations)?;
                    }
                }
            }
        }
        DdlStatement::CreateSchema { .. }
        | DdlStatement::DropSchema { .. }
        | DdlStatement::CreateGraph { .. }
        | DdlStatement::DropGraph { .. }
        | DdlStatement::CreateGraphType { .. }
        | DdlStatement::DropGraphType { .. }
        | DdlStatement::DropNodeType { .. }
        | DdlStatement::DropEdgeType { .. }
        | DdlStatement::TruncateNodeType { .. }
        | DdlStatement::TruncateEdgeType { .. }
        | DdlStatement::CreateIndex { .. }
        | DdlStatement::DropIndex { .. }
        | DdlStatement::ShowNodeTypes(_)
        | DdlStatement::ShowEdgeTypes(_)
        | DdlStatement::ShowIndexes(_)
        | DdlStatement::ShowProcedures(_) => {}
    }
    Ok(())
}

fn collect_return_parameter_declarations(
    clause: &ReturnClause,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    collect_projection_parameter_declarations(
        &clause.items,
        clause.group_by.as_deref(),
        clause.having.as_ref(),
        declarations,
    )
}

fn collect_projection_parameter_declarations(
    items: &[ReturnItem],
    group_by: Option<&[ValueExpr]>,
    having: Option<&ValueExpr>,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    for item in items {
        collect_value_parameter_declarations(&item.expr, declarations)?;
    }
    if let Some(values) = group_by {
        for value in values {
            collect_value_parameter_declarations(value, declarations)?;
        }
    }
    if let Some(value) = having {
        collect_value_parameter_declarations(value, declarations)?;
    }
    Ok(())
}

fn collect_call_parameter_declarations(
    call: &ProcedureCall,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    for arg in &call.args {
        collect_value_parameter_declarations(arg, declarations)?;
    }
    Ok(())
}

fn collect_match_clause_parameter_declarations(
    clause: &MatchClause,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    for pattern in &clause.patterns {
        collect_graph_pattern_parameter_declarations(pattern, declarations)?;
    }
    if let Some(value) = &clause.where_clause {
        collect_value_parameter_declarations(value, declarations)?;
    }
    Ok(())
}

fn collect_graph_pattern_parameter_declarations(
    pattern: &crate::GraphPattern,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => {
                for (_, value) in &node.properties {
                    collect_value_parameter_declarations(value, declarations)?;
                }
                if let Some(value) = &node.inline_where {
                    collect_value_parameter_declarations(value, declarations)?;
                }
            }
            PatternElement::Edge(edge) => {
                for (_, value) in &edge.properties {
                    collect_value_parameter_declarations(value, declarations)?;
                }
                if let Some(value) = &edge.inline_where {
                    collect_value_parameter_declarations(value, declarations)?;
                }
            }
        }
    }
    Ok(())
}

fn collect_limit_parameter_declarations(
    value: &LimitValue,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    if let LimitValue::Parameter {
        name,
        declared_type,
        span,
    } = value
    {
        if let Some(declared_type) = declared_type {
            record_parameter_declaration(
                &mut declarations.declarations,
                name.clone(),
                declared_type,
                *span,
            )?;
        }
        declarations.uses.push(ParameterUse {
            name: name.clone(),
            span: *span,
            declared_type: declared_type.clone(),
        });
    }
    Ok(())
}

fn collect_value_parameter_declarations(
    value: &ValueExpr,
    declarations: &mut ParameterCollector,
) -> Result<(), AnalysisError> {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            ValueExpr::Parameter {
                name,
                declared_type,
                span,
            } => {
                if let Some(declared_type) = declared_type {
                    record_parameter_declaration(
                        &mut declarations.declarations,
                        name.clone(),
                        declared_type,
                        *span,
                    )?;
                }
                declarations.uses.push(ParameterUse {
                    name: name.clone(),
                    span: *span,
                    declared_type: declared_type.clone(),
                });
            }
            ValueExpr::PropertyAccess { target, .. }
            | ValueExpr::UnaryOp {
                operand: target, ..
            }
            | ValueExpr::PropertyExists { target, .. }
            | ValueExpr::Normalize { source: target, .. }
            | ValueExpr::Cast { value: target, .. } => stack.push(target),
            ValueExpr::ListLiteral { items, .. }
            | ValueExpr::PathConstructor {
                elements: items, ..
            }
            | ValueExpr::AllDifferent { items, .. }
            | ValueExpr::Same { items, .. }
            | ValueExpr::FunctionCall { args: items, .. } => stack.extend(items.iter()),
            ValueExpr::DurationBetween { start, end, .. } => {
                stack.push(end);
                stack.push(start);
            }
            ValueExpr::RecordLiteral { fields, .. } => {
                stack.extend(fields.iter().map(|(_, value)| value));
            }
            ValueExpr::BinaryOp { lhs, rhs, .. } => {
                stack.push(rhs);
                stack.push(lhs);
            }
            ValueExpr::IsCheck { operand, kind, .. } => {
                stack.push(operand);
                match kind {
                    IsCheckKind::SourceOf(value) | IsCheckKind::DestinationOf(value) => {
                        stack.push(value);
                    }
                    IsCheckKind::Null
                    | IsCheckKind::Directed
                    | IsCheckKind::Labeled(_)
                    | IsCheckKind::TruthValue(_)
                    | IsCheckKind::Typed(_)
                    | IsCheckKind::Normalized(_) => {}
                }
            }
            ValueExpr::InList { operand, list, .. } => {
                stack.extend(list.iter());
                stack.push(operand);
            }
            ValueExpr::InListExpression { operand, list, .. } => {
                stack.push(list);
                stack.push(operand);
            }
            ValueExpr::Case {
                branches,
                else_branch,
                ..
            } => {
                if let Some(value) = else_branch {
                    stack.push(value);
                }
                for (condition, result) in branches {
                    stack.push(result);
                    stack.push(condition);
                }
            }
            ValueExpr::Trim {
                character, source, ..
            } => {
                stack.push(source);
                if let Some(character) = character {
                    stack.push(character);
                }
            }
            ValueExpr::Exists { body, .. } => match body {
                ExistsBody::Match(pattern) => {
                    collect_match_clause_parameter_declarations(pattern, declarations)?;
                }
                ExistsBody::Query(pipeline) => {
                    collect_pipeline_parameter_declarations(pipeline, declarations)?;
                }
            },
            ValueExpr::ValueSubquery { body, .. } => {
                collect_pipeline_parameter_declarations(body, declarations)?;
            }
            ValueExpr::Literal(_) | ValueExpr::Variable { .. } => {}
        }
    }
    Ok(())
}

fn record_parameter_declaration(
    declarations: &mut DeclarationMap,
    name: DbString,
    declared_type: &GqlType,
    span: SourceSpan,
) -> Result<(), AnalysisError> {
    let semantic = crate::normalize_value_type(declared_type)
        .map_err(|source| AnalysisError::StructuralType { source, span })?;
    if let Some((prior_type, prior_span)) = declarations.get(&name) {
        if prior_type != &semantic {
            return Err(AnalysisError::ConflictingParameterTypes {
                name,
                declarations: vec![
                    (crate::lower_value_type(prior_type), *prior_span),
                    (declared_type.clone(), span),
                ],
            });
        }
    } else {
        declarations.insert(name, (semantic, span));
    }
    Ok(())
}
