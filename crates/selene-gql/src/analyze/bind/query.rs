//! Read-query bind handling.

use std::collections::BTreeSet;

use crate::{
    ForStatement, GqlType, InlineProcedureCall, LetBinding, LimitValue, OrderTerm,
    PipelineStatement, ProcedureMutability, QueryPipeline, ReturnClause, ReturnItem, SourceSpan,
    ValueExpr, WithClause, YieldColumn,
    analyze::{
        BindingDeclKind, BindingId, ScopeId,
        error::{AnalysisError, ConditionClause, ExpectedType, TypeMismatchContext},
        types::AnalyzedType,
    },
};

use super::{BindContext, call, expr, expr_depth, pattern};
use crate::analyze::bind::aggregate_rules;
use crate::analyze::scope::ScopeKind;

pub(crate) fn bind_query_pipeline(
    ctx: &mut BindContext,
    pipeline: &QueryPipeline,
) -> Result<(), AnalysisError> {
    ctx.with_working_scopes(&pipeline.working_scopes, |ctx| {
        bind_query_body(ctx, pipeline)
    })
}

fn bind_query_body(ctx: &mut BindContext, pipeline: &QueryPipeline) -> Result<(), AnalysisError> {
    super::parameters::validate_parameter_declarations(pipeline)?;
    let mut return_sort_context = None;
    for statement in &pipeline.statements {
        match statement {
            PipelineStatement::Return(clause) => {
                bind_return_clause(ctx, clause)?;
                return_sort_context = Some(ReturnSortContext::from_return_clause(clause));
            }
            PipelineStatement::Sorting(terms) => {
                bind_sorting(ctx, terms, return_sort_context.as_ref())?;
                return_sort_context = None;
            }
            _ => {
                bind_pipeline_statement(ctx, statement)?;
                return_sort_context = None;
            }
        }
    }
    Ok(())
}

pub(crate) fn bind_pipeline_statement(
    ctx: &mut BindContext,
    statement: &PipelineStatement,
) -> Result<(), AnalysisError> {
    match statement {
        PipelineStatement::Match(clause) => pattern::bind_match_clause(ctx, clause),
        PipelineStatement::Filter(value) => {
            expr::bind_condition(ctx, value, ConditionClause::Filter)?;
            Ok(())
        }
        PipelineStatement::Let(bindings) => bind_let(ctx, bindings),
        PipelineStatement::For(statement) => bind_for(ctx, statement),
        PipelineStatement::Sorting(terms) => bind_sorting(ctx, terms, None),
        PipelineStatement::Limit(value) | PipelineStatement::Offset(value) => {
            bind_limit_value(ctx, value)
        }
        PipelineStatement::Return(clause) => bind_return_clause(ctx, clause),
        PipelineStatement::With(clause) => bind_with_clause(ctx, clause),
        PipelineStatement::Call(call) => {
            let metadata = call::lookup_metadata(ctx, call)?;
            if matches!(
                metadata.mutability,
                ProcedureMutability::SchemaWrite | ProcedureMutability::MaintenanceWrite
            ) {
                return Err(AnalysisError::MutatingProcedureInReadPipeline {
                    procedure: call.name.clone().into_vec().into_boxed_slice(),
                    mutability: metadata.mutability,
                    span: call.span,
                });
            }
            call::bind_procedure_call_with_metadata(ctx, call, metadata)?;
            Ok(())
        }
        PipelineStatement::CallSubquery(call) => bind_inline_call(ctx, call),
    }
}

fn bind_inline_call(
    ctx: &mut BindContext,
    call: &InlineProcedureCall,
) -> Result<(), AnalysisError> {
    expr_depth::check_query_subquery_depth(&call.body, 1)?;
    let bind_result = match &call.variable_scope {
        // GP03 (ISO §15.2): explicit variable scope — the body sees ONLY the
        // named imports. An empty list (`CALL () { ... }`) is fully isolated.
        Some(imports) => ctx.with_imported_scope(imports, call.span, |ctx| {
            bind_query_pipeline(ctx, &call.body)
        }),
        // GP02: implicit scope — the body inherits all outer bindings.
        None => ctx.with_child_scope(ScopeKind::Subquery, call.span, false, |ctx| {
            bind_query_pipeline(ctx, &call.body)
        }),
    };
    if let Err(AnalysisError::MutatingProcedureInReadPipeline { span, .. }) = bind_result {
        return Err(AnalysisError::NotImplemented {
            message: "write operations inside CALL { ... } are not yet supported".into(),
            span,
            hint: None,
        });
    }
    bind_result?;
    let outputs = inline_call_outputs(ctx, &call.body)?;
    declare_inline_call_yields(ctx, call, &outputs)
}

#[derive(Clone)]
struct InlineCallOutput {
    name: selene_core::DbString,
    ty: AnalyzedType,
    span: crate::SourceSpan,
}

fn inline_call_outputs(
    ctx: &BindContext,
    body: &QueryPipeline,
) -> Result<Vec<InlineCallOutput>, AnalysisError> {
    let Some(return_clause) = body.statements.iter().rev().find_map(|statement| {
        if let PipelineStatement::Return(clause) = statement {
            Some(clause)
        } else {
            None
        }
    }) else {
        return Ok(Vec::new());
    };
    if return_clause.star {
        return Ok(Vec::new());
    }
    Ok(return_clause
        .items
        .iter()
        .filter_map(|item| {
            projection_name(item).map(|name| {
                let ty = ctx
                    .expr_id(&item.expr)
                    .map(|id| ctx.expr_type(id).clone())
                    .unwrap_or(AnalyzedType::Dynamic);
                InlineCallOutput {
                    name,
                    ty,
                    span: item.span,
                }
            })
        })
        .collect::<Vec<_>>())
}

fn declare_inline_call_yields(
    ctx: &mut BindContext,
    call: &InlineProcedureCall,
    outputs: &[InlineCallOutput],
) -> Result<(), AnalysisError> {
    if call.yield_items.is_empty() {
        return Ok(());
    }
    for item in &call.yield_items {
        match &item.column {
            YieldColumn::Star => {
                for output in outputs {
                    ctx.declare_strict_typed(
                        BindingDeclKind::YieldColumn,
                        output.name.clone(),
                        item.span,
                        call::nullable_call_yield_type(output.ty.clone(), call.optional),
                    )?;
                }
            }
            YieldColumn::Named(column) => {
                let Some(output) = outputs.iter().find(|output| output.name == *column) else {
                    return Err(AnalysisError::UnknownYieldColumn {
                        procedure: Box::new([]),
                        column: column.clone(),
                        span: item.span,
                    });
                };
                ctx.declare_strict_typed(
                    BindingDeclKind::YieldColumn,
                    item.alias.clone().unwrap_or_else(|| column.clone()),
                    output.span,
                    call::nullable_call_yield_type(output.ty.clone(), call.optional),
                )?;
            }
        }
    }
    Ok(())
}

fn projection_name(item: &ReturnItem) -> Option<selene_core::DbString> {
    item.alias.clone().or({
        if let ValueExpr::Variable { name, .. } = &item.expr {
            Some(name.clone())
        } else {
            None
        }
    })
}

pub(crate) fn bind_return_clause(
    ctx: &mut BindContext,
    clause: &ReturnClause,
) -> Result<(), AnalysisError> {
    if clause.star && !ctx.current_scope_has_visible_bindings() {
        return Err(AnalysisError::ReturnStarRequiresInput { span: clause.span });
    }
    bind_return_inputs(
        ctx,
        &clause.items,
        clause.group_by.as_deref(),
        clause.having.as_ref(),
    )?;
    // Non-boundary projection scope: ISO GA07 lets ORDER BY / OFFSET /
    // LIMIT reach the pre-RETURN bindings, and `RETURN *` keeps the whole
    // input row visible by virtue of the parent walk.
    ctx.enter_projection_scope(clause.span, false);
    declare_projection_items(ctx, &clause.items)
}

fn bind_with_clause(ctx: &mut BindContext, clause: &WithClause) -> Result<(), AnalysisError> {
    bind_return_inputs(
        ctx,
        &clause.items,
        clause.group_by.as_deref(),
        clause.having.as_ref(),
    )?;
    // Boundary projection scope: pre-WITH bindings end here. Post-WITH
    // clauses see only the projection aliases declared below.
    ctx.enter_projection_scope(clause.span, true);
    declare_projection_items(ctx, &clause.items)?;
    if let Some(where_clause) = &clause.where_clause {
        expr::bind_condition(ctx, where_clause, ConditionClause::WithWhere)?;
    }
    Ok(())
}

fn bind_return_inputs(
    ctx: &mut BindContext,
    items: &[ReturnItem],
    group_by: Option<&[ValueExpr]>,
    having: Option<&ValueExpr>,
) -> Result<(), AnalysisError> {
    let row_scope = ctx.current_scope();
    for item in items {
        expr::bind_value_expr(ctx, &item.expr)?;
    }
    if let Some(values) = group_by {
        for value in values {
            expr::bind_value_expr(ctx, value)?;
        }
    }
    if let Some(value) = having {
        expr::bind_condition(ctx, value, ConditionClause::Having)?;
    }
    aggregate_rules::validate_aggregate_nesting(items, having)?;
    aggregate_rules::validate_grouped_projection_items(items, group_by)?;
    validate_percentile_independent_refs(ctx, row_scope, items, group_by, having)?;
    Ok(())
}

fn validate_percentile_independent_refs(
    ctx: &BindContext<'_>,
    row_scope: ScopeId,
    items: &[ReturnItem],
    group_by: Option<&[ValueExpr]>,
    having: Option<&ValueExpr>,
) -> Result<(), AnalysisError> {
    let group_bindings = group_binding_refs(ctx, group_by);
    for item in items {
        validate_percentile_independent_refs_in_expr(ctx, row_scope, &group_bindings, &item.expr)?;
    }
    if let Some(having) = having {
        validate_percentile_independent_refs_in_expr(ctx, row_scope, &group_bindings, having)?;
    }
    Ok(())
}

fn group_binding_refs(
    ctx: &BindContext<'_>,
    group_by: Option<&[ValueExpr]>,
) -> BTreeSet<BindingId> {
    let mut bindings = BTreeSet::new();
    if let Some(values) = group_by {
        for value in values {
            if let ValueExpr::Variable { span, .. } = value {
                bindings.extend(
                    binding_refs_in_span(ctx, *span)
                        .filter(|use_| use_.span == *span)
                        .map(|use_| use_.binding),
                );
            }
        }
    }
    bindings
}

fn validate_percentile_independent_refs_in_expr(
    ctx: &BindContext<'_>,
    row_scope: ScopeId,
    group_bindings: &BTreeSet<BindingId>,
    value: &ValueExpr,
) -> Result<(), AnalysisError> {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            ValueExpr::FunctionCall { name, args, .. }
                if name.len() == 1
                    && matches!(name.first().as_str(), "percentile_cont" | "percentile_disc") =>
            {
                if let Some(independent) = args.get(1) {
                    validate_percentile_independent_arg(
                        ctx,
                        row_scope,
                        group_bindings,
                        independent,
                    )?;
                }
                if let Some(dependent) = args.first() {
                    stack.push(dependent);
                }
            }
            ValueExpr::PropertyAccess { target, .. }
            | ValueExpr::UnaryOp {
                operand: target, ..
            }
            | ValueExpr::PropertyExists { target, .. }
            | ValueExpr::Cast { value: target, .. }
            | ValueExpr::Normalize { source: target, .. } => stack.push(target),
            ValueExpr::ListLiteral { items, .. }
            | ValueExpr::PathConstructor {
                elements: items, ..
            }
            | ValueExpr::AllDifferent { items, .. }
            | ValueExpr::Same { items, .. }
            | ValueExpr::FunctionCall { args: items, .. } => {
                stack.extend(items.iter());
            }
            ValueExpr::DurationBetween { start, end, .. } => {
                stack.push(end);
                stack.push(start);
            }
            ValueExpr::RecordLiteral { fields, .. } => {
                stack.extend(fields.iter().map(|(_, field)| field));
            }
            ValueExpr::BinaryOp { lhs, rhs, .. } => {
                stack.push(rhs);
                stack.push(lhs);
            }
            ValueExpr::IsCheck { operand, kind, .. } => {
                stack.push(operand);
                match kind {
                    crate::IsCheckKind::SourceOf(value)
                    | crate::IsCheckKind::DestinationOf(value) => stack.push(value),
                    crate::IsCheckKind::Null
                    | crate::IsCheckKind::Directed
                    | crate::IsCheckKind::Labeled(_)
                    | crate::IsCheckKind::TruthValue(_)
                    | crate::IsCheckKind::Typed(_)
                    | crate::IsCheckKind::Normalized(_) => {}
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
            ValueExpr::Literal(_)
            | ValueExpr::Variable { .. }
            | ValueExpr::Parameter { .. }
            | ValueExpr::Exists { .. }
            | ValueExpr::ValueSubquery { .. } => {}
        }
    }
    Ok(())
}

fn validate_percentile_independent_arg(
    ctx: &BindContext<'_>,
    row_scope: ScopeId,
    group_bindings: &BTreeSet<BindingId>,
    independent: &ValueExpr,
) -> Result<(), AnalysisError> {
    for reference in binding_refs_in_span(ctx, independent.span()) {
        if group_bindings.contains(&reference.binding)
            || binding_declared_outside_current_subquery(ctx, row_scope, reference.binding)
        {
            continue;
        }
        return Err(AnalysisError::InvalidReference {
            message: "PERCENTILE independent expression cannot reference a per-row binding unless that binding is part of GROUP BY".to_owned(),
            span: reference.span,
        });
    }
    Ok(())
}

fn binding_refs_in_span<'ctx>(
    ctx: &'ctx BindContext<'_>,
    span: SourceSpan,
) -> impl Iterator<Item = &'ctx crate::analyze::BindingUse> {
    ctx.references
        .iter()
        .filter(move |reference| span_contains(span, reference.span))
}

fn binding_declared_outside_current_subquery(
    ctx: &BindContext<'_>,
    row_scope: ScopeId,
    binding: BindingId,
) -> bool {
    let Some(subquery_scope) = nearest_subquery_scope(ctx, row_scope) else {
        return false;
    };
    let Some(declaration_scope) = declaration_scope(ctx, binding) else {
        return false;
    };
    !scope_is_descendant_of(ctx, declaration_scope, subquery_scope)
}

fn nearest_subquery_scope(ctx: &BindContext<'_>, scope: ScopeId) -> Option<ScopeId> {
    let mut cursor = Some(scope);
    while let Some(scope_id) = cursor {
        let scope = ctx.scopes.scope(scope_id)?;
        if scope.kind == ScopeKind::Subquery {
            return Some(scope_id);
        }
        cursor = scope.parent;
    }
    None
}

fn declaration_scope(ctx: &BindContext<'_>, binding: BindingId) -> Option<ScopeId> {
    ctx.scopes
        .scopes()
        .iter()
        .enumerate()
        .find_map(|(index, scope)| {
            scope
                .locals
                .contains(&binding)
                .then(|| ScopeId::new(index as u32))
        })
}

fn scope_is_descendant_of(ctx: &BindContext<'_>, scope: ScopeId, ancestor: ScopeId) -> bool {
    let mut cursor = Some(scope);
    while let Some(scope_id) = cursor {
        if scope_id == ancestor {
            return true;
        }
        cursor = ctx.scopes.scope(scope_id).and_then(|scope| scope.parent);
    }
    false
}

fn span_contains(outer: SourceSpan, inner: SourceSpan) -> bool {
    outer.byte_offset <= inner.byte_offset && inner.end() <= outer.end()
}

fn declare_projection_items(
    ctx: &mut BindContext,
    items: &[ReturnItem],
) -> Result<(), AnalysisError> {
    for item in items {
        let ty = projection_item_type(ctx, item);
        if let Some(alias) = &item.alias {
            ctx.declare_strict_typed(
                BindingDeclKind::ProjectionAlias,
                alias.clone(),
                item.span,
                ty,
            )?;
        } else if let ValueExpr::Variable { name, span } = &item.expr {
            ctx.declare_strict_typed(BindingDeclKind::ProjectionAlias, name.clone(), *span, ty)?;
        }
    }
    Ok(())
}

fn bind_let(ctx: &mut BindContext, bindings: &[LetBinding]) -> Result<(), AnalysisError> {
    for binding in bindings {
        let id = expr::bind_value_expr(ctx, &binding.value)?;
        let ty = binding.declared_type.as_ref().map_or_else(
            || ctx.expr_type(id).clone(),
            |declared_type| AnalyzedType::Resolved(declared_type.clone()),
        );
        ctx.declare_strict_typed(
            BindingDeclKind::LetAlias,
            binding.alias.clone(),
            binding.span,
            ty,
        )?;
    }
    Ok(())
}

fn bind_for(ctx: &mut BindContext, statement: &ForStatement) -> Result<(), AnalysisError> {
    let id = expr::bind_value_expr(ctx, &statement.source)?;
    let ty = match ctx.expr_type(id) {
        AnalyzedType::Resolved(crate::GqlType::List(inner))
        | AnalyzedType::Resolved(crate::GqlType::BoundedList {
            element_type: inner,
            ..
        }) => AnalyzedType::Resolved((**inner).clone()),
        _ => AnalyzedType::Dynamic,
    };
    ctx.declare_strict_typed(
        BindingDeclKind::ForAlias,
        statement.alias.clone(),
        statement.span,
        ty,
    )?;
    if let Some(position) = &statement.position {
        ctx.declare_strict_typed(
            BindingDeclKind::ForAlias,
            position.alias.clone(),
            statement.span,
            AnalyzedType::Resolved(crate::GqlType::Integer),
        )?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ReturnSortContext {
    allows_aggregate_sort_key: bool,
    order_refs: OrderRefs,
}

/// ORDER_REFS, the set a sort key's binding-variable references must come from.
///
/// ISO/IEC 39075:2024 §14.10 SR 4)c)i)2)A)III defines it in three cases, and SR
/// IV makes membership mandatory rather than advisory.
///
/// This is the only copy of SR III. The planner does not re-derive it: SR IV
/// makes membership a hard rejection here, so by lowering time every surviving
/// sort-key reference is already in the set and the planner can carry it
/// unconditionally (ISO SR VIII).
#[derive(Clone, Debug)]
enum OrderRefs {
    /// Plain `RETURN`: the return aliases plus every column of the incoming
    /// working table. Nothing to check — the binder's non-boundary projection
    /// scope resolves exactly that set already, and the planner carries the
    /// referenced bindings across the projection so the sort can read them.
    IncomingTableAndAliases,
    /// `GROUP BY`, `DISTINCT`, or an aggregate return item: the pre-projection
    /// row does not survive into the ordering, so ORDER_REFS closes to a fixed
    /// set of names and anything outside it must be rejected.
    Closed(Vec<selene_core::DbString>),
}

impl ReturnSortContext {
    fn from_return_clause(clause: &ReturnClause) -> Self {
        Self {
            allows_aggregate_sort_key: clause.group_by.is_some()
                && clause
                    .items
                    .iter()
                    .any(|item| aggregate_rules::contains_aggregate_function(&item.expr)),
            order_refs: order_refs(clause),
        }
    }
}

/// Compute ORDER_REFS for a `RETURN` clause per ISO §14.10 SR 4)c)i)2)A)III.
fn order_refs(clause: &ReturnClause) -> OrderRefs {
    // `RETURN *` keeps the whole input row, so every incoming column stays a
    // legal sort reference regardless of the rest of the clause.
    if clause.star {
        return OrderRefs::IncomingTableAndAliases;
    }
    let has_aggregate_item = clause
        .items
        .iter()
        .any(|item| aggregate_rules::contains_aggregate_function(&item.expr));
    let mut names: Vec<selene_core::DbString> =
        clause.items.iter().filter_map(projection_name).collect();
    match &clause.group_by {
        // RETURN_IDENTIFIERS plus every binding variable reference in GROUP BY.
        Some(keys) => {
            for key in keys {
                collect_variable_names(key, &mut names);
            }
            OrderRefs::Closed(names)
        }
        // RETURN_IDENTIFIERS alone.
        None if clause.distinct || has_aggregate_item => OrderRefs::Closed(names),
        None => OrderRefs::IncomingTableAndAliases,
    }
}

/// Collect binding-variable names referenced directly by an expression.
///
/// `for_each_child` does not descend into `EXISTS` or a value subquery, so this
/// sees only references written at the top level of `expr`. That is the right
/// reach for its one remaining caller — building ORDER_REFS from `GROUP BY`
/// keys, where SR III case 2 asks for the references the keys themselves make.
///
/// It is deliberately NOT what enforces SR IV; see [`sort_key_outer_references`].
fn collect_variable_names(expr: &ValueExpr, out: &mut Vec<selene_core::DbString>) {
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        if let ValueExpr::Variable { name, .. } = expr
            && !out.contains(name)
        {
            out.push(name.clone());
        }
        push_value_expr_children(expr, &mut pending);
    }
}

fn bind_sorting(
    ctx: &mut BindContext,
    terms: &[OrderTerm],
    return_context: Option<&ReturnSortContext>,
) -> Result<(), AnalysisError> {
    for term in terms {
        if sort_key_contains_nested_query(&term.expr) {
            return Err(AnalysisError::SortKeyContainsNestedQuery {
                span: term.expr.span(),
            });
        }
        if return_context.is_some_and(|context| !context.allows_aggregate_sort_key)
            && aggregate_rules::contains_aggregate_function(&term.expr)
        {
            return Err(AnalysisError::SortKeyContainsAggregate {
                span: term.expr.span(),
            });
        }
        // Bind before the SR IV check, not after. A sort key's references are
        // only enumerable once they are resolved, and that is what lets one rule
        // cover both a reference written in the key and a free outer reference
        // inside a subquery body. Binding cannot pre-empt the SR IV diagnostic:
        // the sort key is bound in a scope that still sees the pre-projection
        // row — that is what makes `ORDER BY n.score` legal without DISTINCT —
        // so resolution succeeds either way and SR IV is what decides.
        expr::bind_value_expr(ctx, &term.expr)?;
        if let Some(OrderRefs::Closed(allowed)) = return_context.map(|context| &context.order_refs)
        {
            // ISO §14.10 SR IV: every binding-variable reference in a sort key
            // must be in ORDER_REFS. Under GROUP BY / DISTINCT / aggregation the
            // pre-projection row is gone, so a reference outside the set could
            // only ever evaluate to NULL — which is a sort that silently does
            // nothing, the exact failure this rule exists to prevent.
            let referenced = sort_key_outer_references(ctx, term.expr.span());
            if let Some(name) = referenced.into_iter().find(|name| !allowed.contains(name)) {
                return Err(AnalysisError::SortKeyReferenceNotInScope {
                    name: name.as_str().to_owned(),
                    span: term.expr.span(),
                });
            }
        }
    }
    Ok(())
}

/// The names a sort key references that it does not itself bind.
///
/// ISO §5.3.2.1 makes "contain" transitive, so SR IV reaches a
/// `<binding variable reference>` anywhere inside the sort key — including one
/// inside an `EXISTS` body, which is a genuine reference to an outer binding
/// rather than something the subquery defines.
///
/// The declaration-span test is what separates the two. In
/// `ORDER BY EXISTS { MATCH (n)-[:KNOWS]->(m) }`, `n` resolves to a declaration
/// in the enclosing MATCH and so is subject to SR IV, while `m` is declared
/// inside the sort key and is not a reference to anything discarded. §14.10
/// CR 4 names that distinction directly, exempting a binding variable "defined
/// by an intervening BNF non-terminal instance simply contained in the
/// `<sort key>`".
fn sort_key_outer_references(
    ctx: &BindContext<'_>,
    term_span: SourceSpan,
) -> Vec<selene_core::DbString> {
    let mut names = Vec::new();
    for reference in binding_refs_in_span(ctx, term_span) {
        let declared_inside = ctx
            .scopes
            .declaration(reference.binding)
            .is_some_and(|declaration| span_contains(term_span, declaration.span()));
        if !declared_inside && !names.contains(&reference.name) {
            names.push(reference.name.clone());
        }
    }
    names
}

/// ISO §14.10 SR 4)c)i)2)A)I: no sort key may contain a
/// `<nested query specification>`.
///
/// Two spellings reach it. `VALUE { ... }` is a `<value query expression>`
/// (§20.6), whose BNF is `VALUE <nested query specification>`. And the fifth
/// `<exists predicate>` alternative (§19.4) *is* a `<nested query
/// specification>` — the other four admit only a `<graph pattern>` or a
/// `<match statement block>`, neither of which can contain a RETURN, so a body
/// the parser resolved to a query pipeline is that alternative.
///
/// Those other four forms stay legal here. §19.4 SR 2/3 do rewrite them into a
/// nested query specification, but §5.3.2.4 applies the Syntax Rules of a
/// contained element "at the same time as" those of its container — contrast
/// the General Rules, which it applies contained-first — so that rewrite feeds
/// §19.4's own later rules, not this one. This rule sees the syntax as written.
///
/// §5.3.2.1 makes *contain* transitive, so the search descends through a legal
/// `EXISTS { <graph pattern> }` body rather than stopping at it: the body's own
/// form is fine, but a `VALUE { ... }` inside its `WHERE` or an inline property
/// value is still contained in the sort key and still violates SR I. That
/// descent is why the body is walked with
/// [`MatchClause::for_each_expr`](crate::MatchClause::for_each_expr) instead of
/// being rejected on arrival — rejecting on arrival would undo the reading
/// above and outlaw the four legal spellings.
///
/// The sibling `SortKeyContainsAggregate` check keeps the shallow shape and is
/// deliberately not changed here: `contains_aggregate_function` walks only
/// `for_each_child`, so it stops at a subquery boundary. Whether an aggregate
/// inside an `EXISTS` body belongs to the sort key or to the subquery's own
/// scope is a separate question from SR I's transitive *contain*, and this
/// change does not answer it.
fn sort_key_contains_nested_query(expr: &ValueExpr) -> bool {
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        match expr {
            ValueExpr::ValueSubquery { .. }
            | ValueExpr::Exists {
                body: crate::ExistsBody::Query(_),
                ..
            } => return true,
            // The pattern and match-block spellings are legal themselves, and
            // `for_each_child` yields nothing for a subquery node, so this is
            // the only way into the body.
            ValueExpr::Exists {
                body: crate::ExistsBody::Match(clause),
                ..
            } => clause.for_each_expr(&mut |child| pending.push(child)),
            _ => {}
        }
        push_value_expr_children(expr, &mut pending);
    }
    false
}

fn push_value_expr_children<'a>(expr: &'a ValueExpr, pending: &mut Vec<&'a ValueExpr>) {
    expr.for_each_child(&mut |child| pending.push(child));
}

fn bind_limit_value(ctx: &BindContext, value: &LimitValue) -> Result<(), AnalysisError> {
    if let LimitValue::Parameter {
        name,
        declared_type,
        span,
    } = value
        && let Some(declared_type) = ctx.expr_ids.parameter_type(name).or(declared_type.as_ref())
        && !is_limit_amount_type(declared_type)
    {
        return Err(AnalysisError::TypeMismatch {
            context: TypeMismatchContext::LimitAmount,
            expected: ExpectedType::LimitAmount,
            found: declared_type.clone(),
            span: *span,
        });
    }
    Ok(())
}

fn is_limit_amount_type(ty: &GqlType) -> bool {
    matches!(
        ty.strip_not_null(),
        GqlType::Integer
            | GqlType::Int8
            | GqlType::Int16
            | GqlType::Int32
            | GqlType::Int64
            | GqlType::SmallInt
            | GqlType::BigInt
            | GqlType::Int128
            | GqlType::Decimal
            | GqlType::DecimalExact(_)
            | GqlType::Uint8
            | GqlType::Uint16
            | GqlType::Uint32
            | GqlType::Uint64
            | GqlType::USmallInt
            | GqlType::Uint
            | GqlType::UBigInt
            | GqlType::Uint128
    )
}

fn projection_item_type(ctx: &BindContext, item: &ReturnItem) -> AnalyzedType {
    ctx.expr_id(&item.expr)
        .map(|id| ctx.expr_type(id).clone())
        .unwrap_or(AnalyzedType::Dynamic)
}
