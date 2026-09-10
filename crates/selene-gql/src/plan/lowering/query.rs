//! Query-pipeline lowering.

use crate::{
    GqlType, InlineProcedureCall, LimitValue, PipelineStatement, ProcedureRegistry, QueryPipeline,
    ReturnClause, SourceSpan, WithClause, YieldColumn,
    analyze::{AnalyzedStatement, AnalyzedType},
    plan::{
        BindingElement, BindingTableColumn, BindingTableSchema, ExecutionPlan, ImplDefinedCaps,
        LimitAmount, PipelineOp, PlannedTableSubquery, PlannedTableSubqueryYield, PlannerError,
        ProjectExpr,
    },
};

use super::{aggregate, call, expr, match_clause, sequential_match};

pub(super) fn lower_query_pipeline(
    pipeline: &QueryPipeline,
    registry: &dyn ProcedureRegistry,
    analyzed: &AnalyzedStatement,
    max_quantifier: u32,
) -> Result<ExecutionPlan, PlannerError> {
    let (matches, tail_start) = leading_matches(&pipeline.statements);
    let pattern_plan = match_clause::lower_match_prefix(&matches, analyzed, max_quantifier)?;
    let mut visible = visible_after_pattern(pattern_plan.as_ref());
    let mut ops = Vec::new();
    let tail = &pipeline.statements[tail_start..];
    let mut index = 0;
    // Set when RETURN appended sort carriers. ISO §14.10 GR 1)b)ii drops them
    // after the whole ordering-and-page statement, and only OFFSET/LIMIT may
    // follow a RETURN, so emitting the trim once the tail is exhausted is that
    // position.
    let mut pending_trim: Option<usize> = None;
    // Plan-owned ids for carriers the analyzer never minted a cell for; seeded
    // past every analyzer id so the two spaces cannot collide.
    let mut next_expr_id = super::next_expr_id(analyzed);
    while index < tail.len() {
        match &tail[index] {
            PipelineStatement::Match(clause) => {
                sequential_match::lower(clause, analyzed, &mut ops, &mut visible, max_quantifier)?;
            }
            PipelineStatement::Filter(value) => {
                ops.push(PipelineOp::Filter(expr::filter_predicate(value, analyzed)?));
            }
            PipelineStatement::Let(bindings) => {
                let projects = bindings
                    .iter()
                    .map(|binding| {
                        let mut project = expr::project_expr(
                            &binding.value,
                            Some(binding.alias.clone()),
                            analyzed,
                        )?;
                        if let Some(declared_type) = &binding.declared_type {
                            project.ty = AnalyzedType::Resolved(declared_type.clone());
                            project.declared_type = Some(declared_type.clone());
                        }
                        Ok(project)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                for project in &projects {
                    visible.push(BindingTableColumn {
                        name: project.alias.clone(),
                        hidden: None,
                        ty: project.ty.clone(),
                    });
                }
                // Why: LET is an extend, not a project — earlier bindings stay
                // visible. PipelineOp::Let preserves them; PipelineOp::Project
                // would drop everything outside the new aliases.
                ops.push(PipelineOp::Let(projects));
            }
            PipelineStatement::For(statement) => {
                let source = expr::project_expr(&statement.source, None, analyzed)?;
                visible.push(BindingTableColumn {
                    name: Some(statement.alias.clone()),
                    hidden: None,
                    ty: AnalyzedType::DYNAMIC,
                });
                if let Some(position) = &statement.position {
                    visible.push(BindingTableColumn {
                        name: Some(position.alias.clone()),
                        hidden: None,
                        ty: AnalyzedType::Resolved(GqlType::Integer),
                    });
                }
                ops.push(PipelineOp::Unwind {
                    source,
                    alias: statement.alias.clone(),
                    position: statement.position.clone(),
                    span: statement.span,
                });
            }
            PipelineStatement::Sorting(terms) => {
                ops.push(PipelineOp::OrderBy(
                    terms
                        .iter()
                        .map(|term| expr::order_key(term, analyzed))
                        .collect::<Result<Vec<_>, _>>()?,
                ));
            }
            PipelineStatement::Offset(offset) => {
                if let Some(PipelineStatement::Limit(limit)) = tail.get(index + 1) {
                    ops.push(PipelineOp::Limit {
                        offset: limit_amount(offset, analyzed),
                        count: limit_amount(limit, analyzed),
                    });
                    index += 1;
                } else {
                    ops.push(PipelineOp::Limit {
                        offset: limit_amount(offset, analyzed),
                        count: LimitAmount::Literal(u64::MAX),
                    });
                }
            }
            PipelineStatement::Limit(limit) => {
                // Why: parsers may emit either `OFFSET .. LIMIT ..` or
                // `LIMIT .. OFFSET ..`. Both forms must collapse to one
                // `Limit { offset, count }` op so executors see "skip then
                // take" semantics regardless of source order.
                if let Some(PipelineStatement::Offset(offset)) = tail.get(index + 1) {
                    ops.push(PipelineOp::Limit {
                        offset: limit_amount(offset, analyzed),
                        count: limit_amount(limit, analyzed),
                    });
                    index += 1;
                } else {
                    ops.push(PipelineOp::Limit {
                        offset: LimitAmount::Literal(0),
                        count: limit_amount(limit, analyzed),
                    });
                }
            }
            PipelineStatement::Return(clause) => {
                // ISO §14.10: a RETURN and the ordering-and-page statement that
                // follows it are one primitive result statement, so the sort
                // keys have to be in hand while the projection is being built.
                let sort_terms = match tail.get(index + 1) {
                    Some(PipelineStatement::Sorting(terms)) => Some(terms.as_slice()),
                    _ => None,
                };
                pending_trim = lower_return(
                    clause,
                    analyzed,
                    &mut ops,
                    &mut visible,
                    sort_terms,
                    &mut next_expr_id,
                )?;
            }
            PipelineStatement::With(clause) => {
                lower_with(clause, analyzed, &mut ops, &mut visible)?;
            }
            PipelineStatement::Call(call) => {
                let planned = call::plan_call(call, registry, analyzed)?;
                visible.extend(planned.yield_schema.clone());
                ops.push(PipelineOp::Call(planned));
            }
            PipelineStatement::CallSubquery(call) => {
                let planned = lower_call_subquery(call, registry, analyzed, max_quantifier)?;
                visible.extend(planned.yield_schema.clone());
                ops.push(PipelineOp::CallSubquery(Box::new(planned)));
            }
        }
        index += 1;
    }
    if let Some(projected_width) = pending_trim {
        ops.push(PipelineOp::TrimOrderCarriers { projected_width });
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
        next_expr_id,
        next_pipeline_op_id,
    })
}

fn lower_call_subquery(
    call: &InlineProcedureCall,
    registry: &dyn ProcedureRegistry,
    analyzed: &AnalyzedStatement,
    max_quantifier: u32,
) -> Result<PlannedTableSubquery, PlannerError> {
    // GP03: explicit variable scope is bound in the analyzer (the body sees only
    // the named imports); the import set flows into `outer_binding_refs` below
    // via `outer_binding_refs_in_span` because body references resolve to the
    // outer bindings' ids. No planner-side scope handling is required.
    let mut body = lower_query_pipeline(&call.body, registry, analyzed, max_quantifier)?;
    expr::populate_plan_subqueries(&mut body, analyzed, registry, max_quantifier)?;
    let yield_items = table_subquery_yields(call, &body.output_schema)?;
    let yield_schema = yield_items
        .iter()
        .map(|item| {
            let column = body_column(&body.output_schema, item.source.clone(), item.span)?;
            Ok(BindingTableColumn {
                name: Some(item.output.clone()),
                hidden: None,
                ty: nullable_call_yield_type(column.ty.clone(), call.optional),
            })
        })
        .collect::<Result<Vec<_>, PlannerError>>()?;
    Ok(PlannedTableSubquery {
        optional: call.optional,
        body: Box::new(body),
        outer_binding_refs: expr::outer_binding_refs_in_span(call.body.span, analyzed)?,
        yield_items,
        yield_schema,
        span: call.span,
    })
}

fn table_subquery_yields(
    call: &InlineProcedureCall,
    output_schema: &BindingTableSchema,
) -> Result<Vec<PlannedTableSubqueryYield>, PlannerError> {
    let mut yields = Vec::new();
    for item in &call.yield_items {
        match &item.column {
            YieldColumn::Star => {
                for column in output_schema
                    .columns
                    .iter()
                    .filter_map(|column| column.name.clone())
                {
                    yields.push(PlannedTableSubqueryYield {
                        source: column.clone(),
                        output: column,
                        span: item.span,
                    });
                }
            }
            YieldColumn::Named(source) => {
                let output = item.alias.clone().unwrap_or_else(|| source.clone());
                body_column(output_schema, source.clone(), item.span)?;
                yields.push(PlannedTableSubqueryYield {
                    source: source.clone(),
                    output,
                    span: item.span,
                });
            }
        }
    }
    Ok(yields)
}

pub(super) fn nullable_call_yield_type(ty: AnalyzedType, optional: bool) -> AnalyzedType {
    if !optional {
        return ty;
    }
    match ty {
        AnalyzedType::Resolved(ty) => AnalyzedType::Resolved(nullable_gql_type(ty)),
        AnalyzedType::Dynamic => AnalyzedType::Dynamic,
    }
}

fn nullable_gql_type(ty: GqlType) -> GqlType {
    match ty {
        GqlType::NotNull(inner) => *inner,
        other => other,
    }
}

fn body_column(
    schema: &BindingTableSchema,
    name: selene_core::DbString,
    span: SourceSpan,
) -> Result<&BindingTableColumn, PlannerError> {
    schema
        .columns
        .iter()
        .find(|column| column.name == Some(name.clone()))
        .ok_or(PlannerError::ProcedureMetadataMismatch {
            procedure: Box::new([]),
            detail: "CALL subquery yield column missing from body output schema",
            span,
        })
}

/// Collect the binding-variable references a sort key needs, in first-seen
/// order.
///
/// ISO §14.10 SR 4)c)i)2)A)VIII appends `REF AS REF` — the *reference*, not the
/// whole sort key — so `ORDER BY d.version` carries the node `d`, and
/// `ORDER BY d.version + 0` carries it too without any special handling of the
/// expression around it.
/// A reference a sort key makes that SR VIII may have to carry.
enum CarrierRef<'a> {
    /// A `<binding variable reference>` written in the sort key itself. The
    /// analyzer typed it, so the ordinary projection path applies.
    Node(&'a crate::ValueExpr),
    /// A free outer reference inside a subquery body. There is no `Variable`
    /// node to point at, so the carrier is built from the binding's own
    /// declaration.
    Binding {
        binding: crate::analyze::BindingId,
        name: selene_core::DbString,
        span: SourceSpan,
    },
}

impl CarrierRef<'_> {
    fn name(&self) -> Option<&selene_core::DbString> {
        match self {
            Self::Node(crate::ValueExpr::Variable { name, .. }) | Self::Binding { name, .. } => {
                Some(name)
            }
            Self::Node(_) => None,
        }
    }
}

/// Collect the references a sort key makes, in first-seen order and deduplicated
/// by name.
///
/// A subquery body is a `MatchClause` / `QueryPipeline`, not a `ValueExpr`
/// child, so `for_each_child` walks straight past it and a plain `Variable`
/// sweep sees nothing inside an `EXISTS`. Its free outer references are
/// nonetheless `<binding variable reference>`s *contained in* the sort key —
/// ISO §5.3.2.1 defines "contain" transitively — so SR IV covers them and
/// SR VIII has to carry them.
fn sort_key_variables<'a>(
    terms: &'a [crate::OrderTerm],
    analyzed: &AnalyzedStatement,
) -> Result<Vec<CarrierRef<'a>>, PlannerError> {
    let mut found: Vec<CarrierRef<'a>> = Vec::new();
    for term in terms {
        let mut pending = vec![&term.expr];
        while let Some(expr) = pending.pop() {
            let candidates: Vec<CarrierRef<'a>> = match expr {
                crate::ValueExpr::Variable { .. } => vec![CarrierRef::Node(expr)],
                crate::ValueExpr::Exists { body, span, .. } => {
                    let uses = match body {
                        crate::ExistsBody::Match(pattern) => {
                            expr::outer_binding_uses_in_match(pattern, *span, analyzed)?
                        }
                        crate::ExistsBody::Query(pipeline) => expr::outer_binding_uses_in_span(
                            pipeline.span,
                            pipeline.span,
                            analyzed,
                        )?,
                    };
                    uses.into_iter()
                        .map(|(binding, name, span)| CarrierRef::Binding {
                            binding,
                            name,
                            span,
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            for candidate in candidates {
                let duplicate = candidate.name().is_some_and(|name| {
                    found
                        .iter()
                        .any(|other| other.name().is_some_and(|seen| seen == name))
                });
                if !duplicate {
                    found.push(candidate);
                }
            }
            expr.for_each_child(&mut |child| pending.push(child));
        }
    }
    Ok(found)
}

/// Build the carrier projections that keep a discarded binding alive across the
/// projection so `ORDER BY` can still reach it.
///
/// ISO §14.10 SR 4)c)i)2)A)VIII appends `REF AS REF` for every reference in
/// ORDER_REFS that RETURN_IDENTIFIERS does not already cover.
///
/// There is deliberately no ORDER_REFS test here. SR III lives in the analyzer
/// (`analyze::bind::query::order_refs`) and SR IV makes it a hard rejection, so
/// every reference that reaches this point is already in the set: it is either a
/// return identifier, which the alias check below skips, or a binding ORDER_REFS
/// admits, which is exactly what SR VIII carries. A second copy of SR III here
/// could only ever disagree with the first — which is precisely how `GROUP BY`
/// sort keys came to be admitted by the analyzer and then left with no column to
/// read.
///
/// Narrowing to the references a sort key actually makes is deliberate. SR VIII
/// appends the whole set, but GR 1)b)ii drops every appended column again and
/// only the sort keys read them, so carrying the unreferenced remainder is
/// unobservable work.
fn carrier_projects(
    sort_terms: Option<&[crate::OrderTerm]>,
    projects: &[ProjectExpr],
    analyzed: &AnalyzedStatement,
    next_expr_id: &mut crate::ExprId,
) -> Result<Vec<ProjectExpr>, PlannerError> {
    let Some(terms) = sort_terms else {
        return Ok(Vec::new());
    };
    let mut carriers = Vec::new();
    for carrier in sort_key_variables(terms, analyzed)? {
        let Some(name) = carrier.name().cloned() else {
            continue;
        };
        // A return alias wins over an incoming column of the same name: SR VIII
        // appends only for references RETURN_IDENTIFIERS does not already cover.
        if projects
            .iter()
            .any(|project| project.alias.as_ref() == Some(&name))
        {
            continue;
        }
        carriers.push(match carrier {
            CarrierRef::Node(variable) => expr::project_expr(variable, Some(name), analyzed)?,
            CarrierRef::Binding {
                binding,
                name,
                span,
            } => {
                // The reference lives in a subquery body, so the analyzer never
                // minted an expression cell for it. Take the type from the
                // binding's declaration and allocate a plan-owned id: the
                // executor resolves a `Variable` by name against the row schema
                // (`runtime::evaluator::lookup_variable`), so the id is metadata
                // here rather than a lookup key.
                let declaration = analyzed
                    .scopes
                    .declaration(binding)
                    .ok_or(PlannerError::BindingResolutionLost { binding, span })?;
                let expr_id = *next_expr_id;
                *next_expr_id = crate::ExprId::new(expr_id.get().saturating_add(1));
                ProjectExpr {
                    expr: crate::ValueExpr::Variable {
                        name: name.clone(),
                        span,
                    },
                    expr_id,
                    ty: declaration.ty().clone(),
                    declared_type: None,
                    alias: Some(name),
                    binding_refs: vec![binding],
                    span,
                }
            }
        });
    }
    Ok(carriers)
}

pub(super) fn lower_return(
    clause: &ReturnClause,
    analyzed: &AnalyzedStatement,
    ops: &mut Vec<PipelineOp>,
    visible: &mut Vec<BindingTableColumn>,
    sort_terms: Option<&[crate::OrderTerm]>,
    next_expr_id: &mut crate::ExprId,
) -> Result<Option<usize>, PlannerError> {
    let aggregate_rewrite = aggregate::push_grouping(
        &clause.group_by,
        &clause.items,
        clause.having.as_ref(),
        analyzed,
        ops,
    )?;
    if let Some(having) = &clause.having {
        ops.push(PipelineOp::Filter(aggregate::filter_predicate(
            having,
            analyzed,
            &aggregate_rewrite.names_by_expr_id,
        )?));
    }
    let mut trim = None;
    if clause.star {
        // Why: RETURN * keeps the currently-visible binding table; emitting an
        // empty Project would erase columns that output_schema still advertises.
    } else {
        let mut projects =
            aggregate::project_items(&clause.items, analyzed, &aggregate_rewrite.names_by_expr_id)?;
        *visible = projects_to_columns(&projects);
        let carriers = carrier_projects(sort_terms, &projects, analyzed, next_expr_id)?;
        if !carriers.is_empty() {
            // `visible` stays the projected width, so the carriers are invisible
            // to output_schema; the trim op is what makes the runtime table
            // agree with it again once the sort has consumed them.
            trim = Some(projects.len());
            projects.extend(carriers);
        }
        ops.push(PipelineOp::Project(projects));
    }
    if clause.distinct {
        ops.push(PipelineOp::Distinct);
    }
    Ok(trim)
}

fn lower_with(
    clause: &WithClause,
    analyzed: &AnalyzedStatement,
    ops: &mut Vec<PipelineOp>,
    visible: &mut Vec<BindingTableColumn>,
) -> Result<(), PlannerError> {
    let aggregate_rewrite = aggregate::push_grouping(
        &clause.group_by,
        &clause.items,
        clause.having.as_ref(),
        analyzed,
        ops,
    )?;
    if let Some(having) = &clause.having {
        ops.push(PipelineOp::Filter(aggregate::filter_predicate(
            having,
            analyzed,
            &aggregate_rewrite.names_by_expr_id,
        )?));
    }
    let projects =
        aggregate::project_items(&clause.items, analyzed, &aggregate_rewrite.names_by_expr_id)?;
    *visible = projects_to_columns(&projects);
    ops.push(PipelineOp::Project(projects));
    if clause.distinct {
        ops.push(PipelineOp::Distinct);
    }
    if let Some(where_clause) = &clause.where_clause {
        ops.push(PipelineOp::Filter(expr::filter_predicate(
            where_clause,
            analyzed,
        )?));
    }
    Ok(())
}

fn projects_to_columns(projects: &[ProjectExpr]) -> Vec<BindingTableColumn> {
    projects
        .iter()
        .map(|project| BindingTableColumn {
            name: project.alias.clone(),
            hidden: None,
            ty: project.ty.clone(),
        })
        .collect()
}

pub(super) fn visible_after_pattern(
    pattern: Option<&crate::plan::PatternPlan>,
) -> Vec<BindingTableColumn> {
    pattern
        .map(|plan| {
            plan.bindings
                .iter()
                .filter(|binding| binding.element != BindingElement::Path)
                .map(|binding| BindingTableColumn {
                    name: Some(binding.name.clone()),
                    hidden: None,
                    ty: binding.ty.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn leading_matches(statements: &[PipelineStatement]) -> (Vec<&crate::MatchClause>, usize) {
    let mut matches = Vec::new();
    for (index, statement) in statements.iter().enumerate() {
        match statement {
            PipelineStatement::Match(clause) => matches.push(clause),
            _ => return (matches, index),
        }
    }
    let len = statements.len();
    (matches, len)
}

fn limit_amount(value: &LimitValue, analyzed: &AnalyzedStatement) -> LimitAmount {
    match value {
        LimitValue::Count(value, _) => LimitAmount::Literal(*value),
        LimitValue::Parameter {
            name,
            declared_type,
            span,
        } => LimitAmount::Parameter {
            name: name.clone(),
            declared_type: analyzed
                .expr_ids
                .parameter_type(name)
                .or(declared_type.as_ref())
                .cloned(),
            span: *span,
        },
    }
}
