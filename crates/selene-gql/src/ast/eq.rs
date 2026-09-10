//! Span-erasing AST equality helpers.

use crate::ast::{
    DdlStatement, EdgePattern, ExistsBody, GraphPattern, InlineProcedureCall, MatchClause,
    MutationPipeline, MutationStatement, MutationTerminator, NodePattern, PatternElement,
    ProcedureCall, QueryPipeline, ReturnClause, ReturnItem, SetItem, Statement,
    TypePropertyConstraint, TypePropertyDef, ValueExpr, WithClause,
};

use super::{CharacterStringLiteralKind, Literal, SourceSpan};

/// Return true when two statements are structurally equal after removing spans.
#[must_use]
pub fn structurally_eq(a: &Statement, b: &Statement) -> bool {
    let mut left = a.clone();
    let mut right = b.clone();
    scrub_statement(&mut left);
    scrub_statement(&mut right);
    left == right
}

/// Return true when two value expressions are structurally equal after removing
/// spans and source-spelling metadata.
pub(crate) fn value_structurally_eq(a: &ValueExpr, b: &ValueExpr) -> bool {
    let mut left = a.clone();
    let mut right = b.clone();
    scrub_value(&mut left);
    scrub_value(&mut right);
    left == right
}

fn scrub_statement(statement: &mut Statement) {
    match statement {
        Statement::Query(pipeline) => scrub_query_pipeline(pipeline),
        Statement::Composite { first, rest, span } => {
            *span = SourceSpan::default();
            scrub_query_pipeline(first);
            for (_, pipeline) in rest {
                scrub_query_pipeline(pipeline);
            }
        }
        Statement::Chained { blocks, span } => {
            *span = SourceSpan::default();
            for pipeline in blocks {
                scrub_query_pipeline(pipeline);
            }
        }
        Statement::Mutate(pipeline) => scrub_mutation_pipeline(pipeline),
        Statement::Ddl(statement) => scrub_ddl(statement),
        Statement::Call(call) => scrub_call(call),
        Statement::Explain { inner, span } => {
            *span = SourceSpan::default();
            scrub_statement(inner);
        }
        Statement::StartTransaction { span }
        | Statement::Commit { span }
        | Statement::Rollback { span } => *span = SourceSpan::default(),
        Statement::SessionSetValue { value, span, .. } => {
            *span = SourceSpan::default();
            scrub_value(value);
        }
        Statement::SessionSetTimeZone {
            zone_source_kind,
            span,
            ..
        } => {
            *span = SourceSpan::default();
            *zone_source_kind = CharacterStringLiteralKind::Escaped;
        }
        Statement::SessionSetGraph { target, span } => {
            *span = SourceSpan::default();
            if let crate::SessionSetGraphTarget::CatalogReference(reference)
            | crate::SessionSetGraphTarget::SchemaReference(reference) = target
            {
                reference.span = SourceSpan::default();
            }
        }
        Statement::SessionReset { span, .. } | Statement::SessionClose { span } => {
            *span = SourceSpan::default();
        }
    }
}

fn scrub_query_pipeline(pipeline: &mut QueryPipeline) {
    pipeline.span = SourceSpan::default();
    pipeline.select_origin = None;
    for scope in &mut pipeline.working_scopes {
        match scope {
            crate::WorkingScopeClause::At { reference, span } => {
                reference.span = SourceSpan::default();
                *span = SourceSpan::default();
            }
            crate::WorkingScopeClause::Use { expression, span } => {
                *span = SourceSpan::default();
                match expression {
                    crate::GraphExpression::Reference { reference, .. } => {
                        reference.span = SourceSpan::default()
                    }
                    crate::GraphExpression::Variable { span, .. }
                    | crate::GraphExpression::Current { span, .. } => *span = SourceSpan::default(),
                }
            }
            crate::WorkingScopeClause::Nested(span) => *span = SourceSpan::default(),
        }
    }
    for statement in &mut pipeline.statements {
        match statement {
            crate::PipelineStatement::Match(value) => scrub_match(value),
            crate::PipelineStatement::Filter(value) => scrub_value(value),
            crate::PipelineStatement::Let(values) => {
                for value in values {
                    value.span = SourceSpan::default();
                    scrub_value(&mut value.value);
                }
            }
            crate::PipelineStatement::For(value) => {
                value.span = SourceSpan::default();
                scrub_value(&mut value.source);
            }
            crate::PipelineStatement::Sorting(values) => {
                for value in values {
                    value.span = SourceSpan::default();
                    scrub_value(&mut value.expr);
                }
            }
            crate::PipelineStatement::Limit(value) | crate::PipelineStatement::Offset(value) => {
                match value {
                    crate::LimitValue::Count(_, span)
                    | crate::LimitValue::Parameter { span, .. } => *span = SourceSpan::default(),
                }
            }
            crate::PipelineStatement::Return(value) => scrub_return(value),
            crate::PipelineStatement::With(value) => scrub_with(value),
            crate::PipelineStatement::Call(value) => scrub_call(value),
            crate::PipelineStatement::CallSubquery(value) => scrub_inline_call(value),
        }
    }
}

fn scrub_return(clause: &mut ReturnClause) {
    clause.span = SourceSpan::default();
    for item in &mut clause.items {
        scrub_return_item(item);
    }
    if let Some(group_by) = &mut clause.group_by {
        for item in group_by {
            scrub_value(item);
        }
    }
    if let Some(having) = &mut clause.having {
        scrub_value(having);
    }
}

fn scrub_return_item(item: &mut ReturnItem) {
    item.span = SourceSpan::default();
    scrub_value(&mut item.expr);
}

fn scrub_with(clause: &mut WithClause) {
    clause.span = SourceSpan::default();
    for item in &mut clause.items {
        scrub_return_item(item);
    }
    if let Some(group_by) = &mut clause.group_by {
        for item in group_by {
            scrub_value(item);
        }
    }
    if let Some(having) = &mut clause.having {
        scrub_value(having);
    }
    if let Some(where_clause) = &mut clause.where_clause {
        scrub_value(where_clause);
    }
}

fn scrub_match(clause: &mut MatchClause) {
    clause.span = SourceSpan::default();
    for pattern in &mut clause.patterns {
        scrub_graph_pattern(pattern);
    }
    if let Some(where_clause) = &mut clause.where_clause {
        scrub_value(where_clause);
    }
}

fn scrub_graph_pattern(pattern: &mut GraphPattern) {
    pattern.span = SourceSpan::default();
    for element in &mut pattern.elements {
        match element {
            PatternElement::Node(node) => scrub_node_pattern(node),
            PatternElement::Edge(edge) => scrub_edge_pattern(edge),
        }
    }
}

fn scrub_node_pattern(pattern: &mut NodePattern) {
    pattern.span = SourceSpan::default();
    for (_, value) in &mut pattern.properties {
        scrub_value(value);
    }
    if let Some(value) = &mut pattern.inline_where {
        scrub_value(value);
    }
}

fn scrub_edge_pattern(pattern: &mut EdgePattern) {
    pattern.span = SourceSpan::default();
    for (_, value) in &mut pattern.properties {
        scrub_value(value);
    }
    if let Some(value) = &mut pattern.inline_where {
        scrub_value(value);
    }
}

fn scrub_value(value: &mut ValueExpr) {
    // Erase this node's own span, then recurse into direct `ValueExpr` children
    // (which includes `IS [SOURCE|DESTINATION] OF` operands). Subquery bodies
    // are `MatchClause` / `QueryPipeline`, not `ValueExpr` children, so they are
    // descended explicitly below.
    value.for_each_span_mut(&mut |span| *span = SourceSpan::default());
    scrub_literal_source_kind(value);
    value.for_each_child_mut(&mut scrub_value);
    match value {
        ValueExpr::PropertyExists {
            key_source_kind, ..
        } => {
            *key_source_kind = CharacterStringLiteralKind::Escaped;
        }
        ValueExpr::Exists { body, .. } => match body {
            ExistsBody::Match(pattern) => scrub_match(pattern),
            ExistsBody::Query(pipeline) => scrub_query_pipeline(pipeline),
        },
        ValueExpr::ValueSubquery { body, .. } => scrub_query_pipeline(body),
        _ => {}
    }
}

fn scrub_literal_source_kind(value: &mut ValueExpr) {
    let ValueExpr::Literal(literal) = value else {
        return;
    };
    match literal {
        Literal::String(_, _, kind)
        | Literal::Uuid(_, _, kind)
        | Literal::ZonedDateTime(_, _, kind)
        | Literal::LocalDateTime(_, _, kind)
        | Literal::Date(_, _, kind)
        | Literal::ZonedTime(_, _, kind)
        | Literal::LocalTime(_, _, kind)
        | Literal::Duration(_, _, kind) => *kind = CharacterStringLiteralKind::Escaped,
        Literal::Bool(_, _)
        | Literal::Integer(_, _)
        | Literal::RadixInteger(_, _, _)
        | Literal::Decimal(_, _, _)
        | Literal::Float(_, _, _)
        | Literal::Bytes(_, _)
        | Literal::Null(_) => {}
    }
}

fn scrub_mutation_pipeline(pipeline: &mut MutationPipeline) {
    pipeline.span = SourceSpan::default();
    for statement in &mut pipeline.statements {
        match statement {
            MutationStatement::Match(value) => scrub_match(value),
            MutationStatement::Filter(value) => scrub_value(value),
            MutationStatement::Insert(value) => {
                value.span = SourceSpan::default();
                for pattern in &mut value.patterns {
                    scrub_graph_pattern(pattern);
                }
            }
            MutationStatement::Set(values) => {
                for value in values {
                    match value {
                        SetItem::Property { value, span, .. } => {
                            *span = SourceSpan::default();
                            scrub_value(value);
                        }
                        SetItem::PropertyMerge {
                            properties, span, ..
                        } => {
                            *span = SourceSpan::default();
                            for (_, value) in properties {
                                scrub_value(value);
                            }
                        }
                        SetItem::Label { span, .. } => *span = SourceSpan::default(),
                    }
                }
            }
            MutationStatement::Remove(values) => {
                for value in values {
                    match value {
                        crate::RemoveItem::Property { span, .. }
                        | crate::RemoveItem::Label { span, .. } => *span = SourceSpan::default(),
                    }
                }
            }
            MutationStatement::Delete(value) => value.span = SourceSpan::default(),
        }
    }
    if let Some(terminator) = &mut pipeline.terminator {
        match terminator {
            MutationTerminator::Return(value) => scrub_return(value),
            MutationTerminator::Finish(span) => *span = SourceSpan::default(),
        }
    }
}

fn scrub_ddl(statement: &mut DdlStatement) {
    match statement {
        DdlStatement::CreateSchema {
            reference, span, ..
        }
        | DdlStatement::DropSchema {
            reference, span, ..
        }
        | DdlStatement::DropGraph {
            reference, span, ..
        }
        | DdlStatement::DropGraphType {
            reference, span, ..
        } => {
            *span = SourceSpan::default();
            reference.span = SourceSpan::default();
        }
        DdlStatement::CreateGraph {
            reference,
            graph_type,
            span,
            ..
        } => {
            *span = SourceSpan::default();
            reference.span = SourceSpan::default();
            if let Some(graph_type) = graph_type {
                graph_type.span = SourceSpan::default();
            }
        }
        DdlStatement::CreateGraphType {
            reference,
            definition,
            span,
            ..
        } => {
            *span = SourceSpan::default();
            reference.span = SourceSpan::default();
            definition.span = SourceSpan::default();
            for node_type in &mut definition.node_types {
                node_type.span = SourceSpan::default();
            }
        }
        DdlStatement::DropNodeType { span, .. }
        | DdlStatement::DropEdgeType { span, .. }
        | DdlStatement::TruncateNodeType { span, .. }
        | DdlStatement::TruncateEdgeType { span, .. }
        | DdlStatement::CreateIndex { span, .. }
        | DdlStatement::DropIndex { span, .. } => *span = SourceSpan::default(),
        DdlStatement::CreateNodeType {
            properties,
            key_label_set,
            span,
            ..
        }
        | DdlStatement::CreateEdgeType {
            properties,
            key_label_set,
            span,
            ..
        } => {
            *span = SourceSpan::default();
            // The explicit GG21 key-label-set carries its own source span;
            // erase it too so `structurally_eq` stays span-insensitive for
            // identical DDL parsed at different offsets (e.g. via `parse_many`).
            if let Some(key_label_set) = key_label_set {
                key_label_set.span = SourceSpan::default();
            }
            for property in properties {
                scrub_property_def(property);
            }
        }
        DdlStatement::AlterNodeType {
            properties, span, ..
        }
        | DdlStatement::AlterEdgeType {
            properties, span, ..
        } => {
            *span = SourceSpan::default();
            for property in properties {
                scrub_property_def(property);
            }
        }
        DdlStatement::ShowNodeTypes(span)
        | DdlStatement::ShowEdgeTypes(span)
        | DdlStatement::ShowIndexes(span)
        | DdlStatement::ShowProcedures(span) => {
            *span = SourceSpan::default();
        }
    }
}

fn scrub_property_def(property: &mut TypePropertyDef) {
    property.span = SourceSpan::default();
    for constraint in &mut property.constraints {
        match constraint {
            TypePropertyConstraint::NotNull(span)
            | TypePropertyConstraint::Immutable(span)
            | TypePropertyConstraint::Unique(span) => *span = SourceSpan::default(),
            TypePropertyConstraint::Indexed { span, .. } => *span = SourceSpan::default(),
            TypePropertyConstraint::Default(value, span) => {
                *span = SourceSpan::default();
                scrub_value(value);
            }
        }
    }
}

fn scrub_call(call: &mut ProcedureCall) {
    call.span = SourceSpan::default();
    for arg in &mut call.args {
        scrub_value(arg);
    }
    for item in &mut call.yield_items {
        item.span = SourceSpan::default();
    }
}

fn scrub_inline_call(call: &mut InlineProcedureCall) {
    call.span = SourceSpan::default();
    scrub_query_pipeline(&mut call.body);
    for item in &mut call.yield_items {
        item.span = SourceSpan::default();
    }
}

#[cfg(test)]
mod tests {
    use crate::{Statement, parse};

    use super::structurally_eq;

    #[test]
    fn ignores_span_differences() {
        let left = parse("RETURN 1").expect("parse");
        let right = parse("\nRETURN 1").expect("parse");
        assert!(structurally_eq(&left, &right));
    }

    #[test]
    fn preserves_structural_differences() {
        let left = parse("RETURN 1").expect("parse");
        let right = parse("RETURN 2").expect("parse");
        assert!(!structurally_eq(&left, &right));
        assert!(matches!(left, Statement::Query(_)));
    }

    #[test]
    fn ignores_gg21_key_label_set_span_differences() {
        // The explicit GG21 `<...type key label set>` carries its own source
        // span; two identical DDL statements parsed at different offsets must
        // still compare structurally equal (the scrub clears `key_label_set.span`).
        let left = parse("CREATE NODE TYPE :Person => (name :: STRING)").expect("parse");
        let right = parse("   CREATE NODE TYPE :Person => (name :: STRING)").expect("parse");
        assert!(structurally_eq(&left, &right));
    }
}
