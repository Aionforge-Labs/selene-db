//! Preflight validation for read-side AST formatting.

use crate::ast::{
    EdgePattern, ExistsBody, GqlType, GraphPattern, InlineProcedureCall, IsCheckKind, MatchClause,
    NodePattern, PatternElement, ProcedureCall, QueryPipeline, RecordType, ReturnClause, Statement,
    ValueExpr, WithClause,
};

use super::FormatError;

/// Validate that a read-side AST can be rendered without semantic loss.
///
/// # Errors
///
/// Returns [`FormatError::Unsupported`] when the tree contains AST-only type
/// variants that the parser cannot read back from formatted source.
pub(crate) fn validate_formattable(stmt: &Statement) -> Result<(), FormatError> {
    match stmt {
        Statement::Query(pipeline) => validate_pipeline(pipeline),
        Statement::Composite { first, rest, .. } => {
            validate_pipeline(first)?;
            for (_, pipeline) in rest {
                validate_pipeline(pipeline)?;
            }
            Ok(())
        }
        Statement::Chained { blocks, .. } => {
            for block in blocks {
                validate_pipeline(block)?;
            }
            Ok(())
        }
        Statement::Mutate(_)
        | Statement::Ddl(_)
        | Statement::Call(_)
        | Statement::Explain { .. }
        | Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::SessionSetValue { .. }
        | Statement::SessionSetTimeZone { .. }
        | Statement::SessionSetGraph { .. }
        | Statement::SessionReset { .. }
        | Statement::SessionClose { .. } => Ok(()),
    }
}

fn validate_pipeline(pipeline: &QueryPipeline) -> Result<(), FormatError> {
    for statement in &pipeline.statements {
        match statement {
            crate::PipelineStatement::Match(value) => validate_match(value)?,
            crate::PipelineStatement::Filter(value) => validate_expr(value)?,
            crate::PipelineStatement::Let(values) => {
                for value in values {
                    validate_expr(&value.value)?;
                }
            }
            crate::PipelineStatement::For(value) => validate_expr(&value.source)?,
            crate::PipelineStatement::Sorting(values) => {
                for value in values {
                    validate_expr(&value.expr)?;
                }
            }
            crate::PipelineStatement::Limit(_) | crate::PipelineStatement::Offset(_) => {}
            crate::PipelineStatement::Return(value) => validate_return(value)?,
            crate::PipelineStatement::With(value) => validate_with(value)?,
            crate::PipelineStatement::Call(value) => validate_procedure_call(value)?,
            crate::PipelineStatement::CallSubquery(value) => validate_inline_call(value)?,
        }
    }
    Ok(())
}

fn validate_match(clause: &MatchClause) -> Result<(), FormatError> {
    for pattern in &clause.patterns {
        validate_graph_pattern(pattern)?;
    }
    if let Some(where_clause) = &clause.where_clause {
        validate_expr(where_clause)?;
    }
    Ok(())
}

fn validate_graph_pattern(pattern: &GraphPattern) -> Result<(), FormatError> {
    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => validate_node_pattern(node)?,
            PatternElement::Edge(edge) => validate_edge_pattern(edge)?,
        }
    }
    Ok(())
}

fn validate_node_pattern(node: &NodePattern) -> Result<(), FormatError> {
    for (_, value) in &node.properties {
        validate_expr(value)?;
    }
    if let Some(where_clause) = &node.inline_where {
        validate_expr(where_clause)?;
    }
    Ok(())
}

fn validate_edge_pattern(edge: &EdgePattern) -> Result<(), FormatError> {
    for (_, value) in &edge.properties {
        validate_expr(value)?;
    }
    if let Some(where_clause) = &edge.inline_where {
        validate_expr(where_clause)?;
    }
    Ok(())
}

fn validate_return(clause: &ReturnClause) -> Result<(), FormatError> {
    if clause.star && clause.group_by.is_some() {
        return Err(FormatError::Invalid {
            reason: "RETURN * cannot specify GROUP BY",
        });
    }
    for item in &clause.items {
        validate_expr(&item.expr)?;
    }
    if let Some(group_by) = &clause.group_by {
        for item in group_by {
            validate_expr(item)?;
        }
    }
    if let Some(having) = &clause.having {
        validate_expr(having)?;
    }
    Ok(())
}

fn validate_with(clause: &WithClause) -> Result<(), FormatError> {
    for item in &clause.items {
        validate_expr(&item.expr)?;
    }
    if let Some(group_by) = &clause.group_by {
        for item in group_by {
            validate_expr(item)?;
        }
    }
    if let Some(having) = &clause.having {
        validate_expr(having)?;
    }
    if let Some(where_clause) = &clause.where_clause {
        validate_expr(where_clause)?;
    }
    Ok(())
}

pub(super) fn validate_procedure_call(call: &ProcedureCall) -> Result<(), FormatError> {
    for arg in &call.args {
        validate_expr(arg)?;
    }
    Ok(())
}

pub(super) fn validate_inline_call(call: &InlineProcedureCall) -> Result<(), FormatError> {
    validate_pipeline(&call.body)
}

fn validate_expr(expr: &ValueExpr) -> Result<(), FormatError> {
    match expr {
        ValueExpr::Literal(_) | ValueExpr::Variable { .. } => Ok(()),
        ValueExpr::Parameter { declared_type, .. } => {
            if let Some(ty) = declared_type {
                validate_type(ty)?;
            }
            Ok(())
        }
        ValueExpr::PropertyAccess { target, .. } => validate_expr(target),
        ValueExpr::ListLiteral { items, .. } => validate_exprs(items),
        ValueExpr::PathConstructor { elements, .. } => validate_exprs(elements),
        ValueExpr::RecordLiteral { fields, .. } => {
            for (_, value) in fields {
                validate_expr(value)?;
            }
            Ok(())
        }
        ValueExpr::BinaryOp { lhs, rhs, .. } => {
            validate_expr(lhs)?;
            validate_expr(rhs)
        }
        ValueExpr::UnaryOp { operand, .. } => validate_expr(operand),
        ValueExpr::FunctionCall { args, .. } => validate_exprs(args),
        ValueExpr::DurationBetween { start, end, .. } => {
            validate_expr(start)?;
            validate_expr(end)
        }
        ValueExpr::Normalize { source, .. } => validate_expr(source),
        ValueExpr::Trim {
            character, source, ..
        } => {
            if let Some(character) = character {
                validate_expr(character)?;
            }
            validate_expr(source)
        }
        ValueExpr::IsCheck { operand, kind, .. } => {
            validate_expr(operand)?;
            validate_is_check(kind)
        }
        ValueExpr::InList { operand, list, .. } => {
            validate_expr(operand)?;
            validate_exprs(list)
        }
        ValueExpr::InListExpression { operand, list, .. } => {
            validate_expr(operand)?;
            validate_expr(list)
        }
        ValueExpr::AllDifferent { items, .. } | ValueExpr::Same { items, .. } => {
            validate_exprs(items)
        }
        ValueExpr::PropertyExists { target, .. } => validate_expr(target),
        ValueExpr::Case {
            branches,
            else_branch,
            ..
        } => {
            for (condition, result) in branches {
                validate_expr(condition)?;
                validate_expr(result)?;
            }
            if let Some(value) = else_branch {
                validate_expr(value)?;
            }
            Ok(())
        }
        ValueExpr::Exists { body, .. } => match body {
            ExistsBody::Match(pattern) => validate_match(pattern),
            ExistsBody::Query(pipeline) => validate_pipeline(pipeline),
        },
        ValueExpr::ValueSubquery { body, .. } => validate_pipeline(body),
        ValueExpr::Cast {
            value, target_type, ..
        } => {
            validate_type(target_type)?;
            validate_expr(value)
        }
    }
}

fn validate_exprs(items: &[ValueExpr]) -> Result<(), FormatError> {
    for item in items {
        validate_expr(item)?;
    }
    Ok(())
}

fn validate_is_check(kind: &IsCheckKind) -> Result<(), FormatError> {
    match kind {
        IsCheckKind::Typed(ty) => validate_type(ty),
        IsCheckKind::SourceOf(value) | IsCheckKind::DestinationOf(value) => validate_expr(value),
        IsCheckKind::Null
        | IsCheckKind::Directed
        | IsCheckKind::Labeled(_)
        | IsCheckKind::TruthValue(_)
        | IsCheckKind::Normalized(_) => Ok(()),
    }
}

fn validate_type(ty: &GqlType) -> Result<(), FormatError> {
    if let Some(variant) = ast_only_type_variant(ty) {
        return Err(FormatError::Unsupported { variant });
    }
    match ty {
        GqlType::List(inner)
        | GqlType::BoundedList {
            element_type: inner,
            ..
        } => validate_type(inner)?,
        GqlType::NotNull(inner) => validate_type(inner)?,
        // Closed record types render their field structure, so each field's
        // value type must also be formattable (a nested reference type would
        // otherwise slip past the gate). The open form has no fields to check.
        GqlType::Record(RecordType::Closed(fields)) => {
            for (_name, field_ty) in fields {
                validate_type(field_ty)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn ast_only_type_variant(ty: &GqlType) -> Option<&'static str> {
    match ty {
        GqlType::NotNull(inner) => ast_only_type_variant(inner),
        GqlType::GraphRef => Some("GraphRef"),
        GqlType::TableRef(_) => Some("TableRef"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::validate_formattable;
    use crate::{
        GqlType, IsCheckKind, Literal, PipelineStatement, QueryPipeline, RecordType, ReturnClause,
        ReturnItem, SourceSpan, Statement, ValueExpr, ast::FormatError,
    };

    #[test]
    fn preflight_accepts_open_and_closed_record_types() {
        // GV47 open and GV46 closed record TYPES round-trip through the
        // formatter, so the preflight gate must let them through.
        validate_formattable(&statement_with_type(GqlType::Record(RecordType::Open)))
            .expect("open record type is formattable");
        let closed = GqlType::Record(RecordType::Closed(vec![(
            selene_core::db_string("name").expect("db_string name"),
            GqlType::String,
        )]));
        validate_formattable(&statement_with_type(closed)).expect("closed record type formattable");
    }

    #[test]
    fn preflight_accepts_vector_types() {
        validate_formattable(&statement_with_type(GqlType::Vector))
            .expect("vector type is formattable");
        validate_formattable(&statement_with_type(GqlType::List(Box::new(
            GqlType::Vector,
        ))))
        .expect("list of vector type is formattable");
        validate_formattable(&statement_with_type(GqlType::BoundedList {
            element_type: Box::new(GqlType::Vector),
            max_len: 3,
        }))
        .expect("bounded list of vector type is formattable");
        validate_formattable(&parameter_statement_with_type(GqlType::Vector))
            .expect("typed vector parameter is formattable");
    }

    #[test]
    fn preflight_accepts_graph_element_reference_types() {
        validate_formattable(&statement_with_type(GqlType::NodeRef))
            .expect("NODE reference type is formattable");
        validate_formattable(&statement_with_type(GqlType::EdgeRef))
            .expect("EDGE reference type is formattable");
    }

    #[test]
    fn preflight_accepts_graph_element_reference_type_inside_closed_record_field() {
        let closed = GqlType::Record(RecordType::Closed(vec![(
            selene_core::db_string("ref").expect("db_string ref"),
            GqlType::NodeRef,
        )]));
        validate_formattable(&statement_with_type(closed))
            .expect("closed record with NODE reference field is formattable");
    }

    #[test]
    fn preflight_rejects_graph_ref_type() {
        assert_unsupported(GqlType::GraphRef, "GraphRef");
    }

    #[test]
    fn preflight_rejects_table_ref_type() {
        assert_unsupported(GqlType::TableRef(crate::BindingTableType::Any), "TableRef");
    }

    #[test]
    fn preflight_rejects_return_star_group_by() {
        let span = SourceSpan::default();
        let err = validate_formattable(&Statement::Query(QueryPipeline {
            working_scopes: Vec::new(),
            select_origin: None,
            statements: vec![PipelineStatement::Return(ReturnClause {
                distinct: false,
                star: true,
                items: Vec::new(),
                group_by: Some(vec![ValueExpr::Literal(Literal::Integer(1, span))]),
                having: None,
                span,
            })],
            span,
        }))
        .expect_err("RETURN * GROUP BY is invalid");
        match err {
            FormatError::Invalid { reason } => {
                assert_eq!(reason, "RETURN * cannot specify GROUP BY");
            }
            FormatError::Unsupported { .. } | FormatError::Fmt(_) => {
                panic!("expected invalid AST")
            }
        }
    }

    fn assert_unsupported(ty: GqlType, expected: &'static str) {
        let err = validate_formattable(&statement_with_type(ty)).expect_err("type is unsupported");
        match err {
            FormatError::Unsupported { variant } => assert_eq!(variant, expected),
            FormatError::Invalid { .. } | FormatError::Fmt(_) => {
                panic!("expected unsupported variant")
            }
        }
    }

    fn statement_with_type(ty: GqlType) -> Statement {
        let span = SourceSpan::default();
        Statement::Query(QueryPipeline {
            working_scopes: Vec::new(),
            select_origin: None,
            statements: vec![PipelineStatement::Return(ReturnClause {
                distinct: false,
                star: false,
                items: vec![ReturnItem {
                    expr: ValueExpr::IsCheck {
                        operand: Box::new(ValueExpr::Literal(Literal::Null(span))),
                        kind: IsCheckKind::Typed(ty),
                        negated: false,
                        span,
                    },
                    alias: None,
                    span,
                }],
                group_by: None,
                having: None,
                span,
            })],
            span,
        })
    }

    fn parameter_statement_with_type(ty: GqlType) -> Statement {
        let span = SourceSpan::default();
        Statement::Query(QueryPipeline {
            working_scopes: Vec::new(),
            select_origin: None,
            statements: vec![PipelineStatement::Return(ReturnClause {
                distinct: false,
                star: false,
                items: vec![ReturnItem {
                    expr: ValueExpr::Parameter {
                        name: selene_core::db_string("value").expect("db_string parameter name"),
                        declared_type: Some(ty),
                        span,
                    },
                    alias: None,
                    span,
                }],
                group_by: None,
                having: None,
                span,
            })],
            span,
        })
    }
}
