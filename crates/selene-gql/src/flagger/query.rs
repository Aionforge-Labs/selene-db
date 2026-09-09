//! Query and top-level statement Flagger walk.

use selene_core::DbString;
use selene_profile::FeatureId;

use crate::{
    LimitValue, PipelineStatement, QueryPipeline, ReturnClause, SessionResetTarget, SetOp,
    Statement, ValueExpr, WithClause,
    ast::{
        pattern::{
            EdgeDirection, EdgePattern, GraphPattern, MatchClause, MatchMode, NodePattern,
            PathMode, PathSelector, PatternElement, Quantifier,
        },
        statement::{ForStatement, LetBinding, OrderTerm, RowExpansionPositionKind},
    },
};

use super::{FeatureUse, call, ddl, expr, mutation, record_feature};

pub(crate) fn statement(statement: &Statement, uses: &mut Vec<FeatureUse>) {
    match statement {
        Statement::Query(pipeline) => query_pipeline(pipeline, uses),
        Statement::Composite { first, rest, .. } => {
            query_pipeline(first, uses);
            for (op, pipeline) in rest {
                match op {
                    SetOp::Union | SetOp::UnionAll => {
                        record_feature(uses, FeatureId::GQ03, pipeline.span);
                    }
                    SetOp::Otherwise => record_feature(uses, FeatureId::GQ02, pipeline.span),
                    SetOp::Intersect => record_feature(uses, FeatureId::GQ06, pipeline.span),
                    SetOp::IntersectAll => record_feature(uses, FeatureId::GQ07, pipeline.span),
                    SetOp::Except => record_feature(uses, FeatureId::GQ04, pipeline.span),
                    SetOp::ExceptAll => record_feature(uses, FeatureId::GQ05, pipeline.span),
                }
                query_pipeline(pipeline, uses);
            }
        }
        Statement::Chained { blocks, span } => {
            record_feature(uses, FeatureId::GQ20, *span);
            for block in blocks {
                query_pipeline(block, uses);
            }
        }
        Statement::Mutate(pipeline) => mutation::pipeline(pipeline, uses),
        Statement::Ddl(statement) => ddl::statement(statement, uses),
        Statement::Call(call) => call::procedure_call(call, uses),
        Statement::Explain { inner, .. } => self::statement(inner, uses),
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. } => record_feature(uses, FeatureId::GT01, statement.span()),
        Statement::SessionSetValue {
            declared_type,
            value,
            span,
            ..
        } => {
            record_feature(uses, FeatureId::GS03, *span);
            if let Some(ty) = declared_type {
                expr::gql_type(ty, *span, uses);
            }
            expr::value(value, uses);
        }
        Statement::SessionSetTimeZone {
            zone_source_kind,
            span,
            ..
        } => {
            record_feature(uses, FeatureId::GS15, *span);
            expr::character_string_literal(*zone_source_kind, *span, uses);
        }
        Statement::SessionSetGraph { .. } => {}
        Statement::SessionReset { target, span } => match target {
            SessionResetTarget::AllCharacteristics => {
                record_feature(uses, FeatureId::GS04, *span);
            }
            SessionResetTarget::Schema => record_feature(uses, FeatureId::GS05, *span),
            SessionResetTarget::Graph => record_feature(uses, FeatureId::GS06, *span),
            SessionResetTarget::Parameters => record_feature(uses, FeatureId::GS08, *span),
            SessionResetTarget::TimeZone => record_feature(uses, FeatureId::GS07, *span),
            SessionResetTarget::Parameter(_) => {
                // ISO/IEC 39075:2024 section 7.2: `SESSION RESET PARAMETER <name>`
                // exercises both the RESET-PARAMETER surface (CR6 → GS08, shared
                // with `RESET ALL PARAMETERS`) and the parameter-name argument
                // (CR7 → GS16). A faithful Flagger (clause 24.6) stamps both.
                record_feature(uses, FeatureId::GS08, *span);
                record_feature(uses, FeatureId::GS16, *span);
            }
        },
        // SESSION CLOSE (ISO section 7.3) has no feature code (Conformance
        // Rules: None); it is unconditionally accepted.
        Statement::SessionClose { .. } => {}
    }
}

pub(crate) fn query_pipeline(pipeline: &QueryPipeline, uses: &mut Vec<FeatureUse>) {
    let mut projection_names = None;
    for (index, statement) in pipeline.statements.iter().enumerate() {
        if index > 0 && matches!(statement, PipelineStatement::Match(_)) {
            record_feature(uses, FeatureId::GQ20, statement.span());
        }
        pipeline_statement(statement, projection_names.as_deref(), uses);
        match statement {
            PipelineStatement::Return(clause) => {
                projection_names = Some(projected_names(&clause.items));
            }
            PipelineStatement::With(clause) => {
                projection_names = Some(projected_names(&clause.items));
            }
            PipelineStatement::Sorting(_)
            | PipelineStatement::Limit(_)
            | PipelineStatement::Offset(_) => {}
            _ => projection_names = None,
        }
    }
}

pub(crate) fn pipeline_statement(
    statement: &PipelineStatement,
    projection_names: Option<&[DbString]>,
    uses: &mut Vec<FeatureUse>,
) {
    match statement {
        PipelineStatement::Match(value) => match_clause(value, uses),
        PipelineStatement::Filter(value) => {
            record_feature(uses, FeatureId::GQ08, value.span());
            expr::value(value, uses);
        }
        PipelineStatement::Let(values) => let_bindings(values, uses),
        PipelineStatement::For(value) => for_statement(value, uses),
        PipelineStatement::Sorting(values) => order_terms(values, projection_names, uses),
        PipelineStatement::Limit(value) => {
            record_feature(uses, FeatureId::GQ13, value.span());
            limit_value(value, uses);
        }
        PipelineStatement::Offset(value) => {
            record_feature(uses, FeatureId::GQ12, value.span());
            limit_value(value, uses);
        }
        PipelineStatement::Return(value) => return_clause(value, uses),
        PipelineStatement::With(value) => with_clause(value, uses),
        PipelineStatement::Call(value) => call::procedure_call(value, uses),
        PipelineStatement::CallSubquery(value) => {
            record_feature(uses, FeatureId::GP01, value.span);
            if value.variable_scope.is_some() {
                record_feature(uses, FeatureId::GP03, value.span);
            } else {
                record_feature(uses, FeatureId::GP02, value.span);
            }
            query_pipeline(&value.body, uses);
        }
    }
}

fn projected_names(items: &[crate::ReturnItem]) -> Vec<DbString> {
    items.iter().filter_map(projection_name).collect()
}

fn projection_name(item: &crate::ReturnItem) -> Option<DbString> {
    item.alias.clone().or_else(|| match &item.expr {
        ValueExpr::Variable { name, .. } => Some(name.clone()),
        _ => None,
    })
}

fn limit_value(value: &LimitValue, uses: &mut Vec<FeatureUse>) {
    if let LimitValue::Parameter {
        declared_type: Some(ty),
        span,
        ..
    } = value
    {
        record_feature(uses, FeatureId::IM_TYPED_PARAMS, *span);
        expr::gql_type(ty, *span, uses);
    }
}

pub(crate) fn return_clause(clause: &ReturnClause, uses: &mut Vec<FeatureUse>) {
    if let Some(group_by) = &clause.group_by {
        record_feature(uses, FeatureId::GQ15, clause.span);
        for item in group_by {
            expr::value(item, uses);
        }
    }
    if let Some(having) = &clause.having {
        expr::value(having, uses);
    }
    for item in &clause.items {
        expr::value(&item.expr, uses);
    }
}

pub(crate) fn with_clause(clause: &WithClause, uses: &mut Vec<FeatureUse>) {
    if let Some(group_by) = &clause.group_by {
        record_feature(uses, FeatureId::GQ15, clause.span);
        for item in group_by {
            expr::value(item, uses);
        }
    }
    if let Some(having) = &clause.having {
        expr::value(having, uses);
    }
    if let Some(where_clause) = &clause.where_clause {
        expr::value(where_clause, uses);
    }
    for item in &clause.items {
        expr::value(&item.expr, uses);
    }
}

pub(crate) fn match_clause(clause: &MatchClause, uses: &mut Vec<FeatureUse>) {
    if let Some(mode) = clause.match_mode {
        match mode {
            MatchMode::DifferentEdges => record_feature(uses, FeatureId::G002, clause.span),
            MatchMode::RepeatableElements => record_feature(uses, FeatureId::G003, clause.span),
        }
    }
    if clause.path_mode_explicit && clause.path_mode == PathMode::Walk {
        record_feature(uses, FeatureId::G010, clause.span);
    }
    // ISO §16.6 <path or paths> (Annex A §16.6 CR5): the explicit PATH/PATHS
    // keyword is Feature G014. Record it IFF the keyword was written; absent it
    // is never stamped. Pure surface sugar (§1.2.4) — no runtime effect.
    if clause.path_or_paths {
        record_feature(uses, FeatureId::G014, clause.span);
    }
    match clause.path_mode {
        PathMode::Walk => {}
        PathMode::Trail => record_feature(uses, FeatureId::G011, clause.span),
        PathMode::Simple => record_feature(uses, FeatureId::G012, clause.span),
        PathMode::Acyclic => record_feature(uses, FeatureId::G013, clause.span),
    }
    if let Some(selector) = clause.selector {
        match selector {
            PathSelector::All => record_feature(uses, FeatureId::G015, clause.span),
            PathSelector::Any { .. } => record_feature(uses, FeatureId::G016, clause.span),
            PathSelector::AllShortest => record_feature(uses, FeatureId::G017, clause.span),
            PathSelector::AnyShortest => record_feature(uses, FeatureId::G018, clause.span),
            // Per ISO 39075:2024 §16.6 CR10/11: SHORTEST N PATHS is the counted
            // shortest path search (G019); SHORTEST [N] GROUP[S] is the counted
            // shortest group search (G020).
            PathSelector::CountedShortest { .. } => {
                record_feature(uses, FeatureId::G019, clause.span)
            }
            PathSelector::CountedShortestGroup { .. } => {
                record_feature(uses, FeatureId::G020, clause.span)
            }
        }
    }
    for pattern in &clause.patterns {
        graph_pattern(pattern, uses);
    }
    if let Some(where_clause) = &clause.where_clause {
        expr::value(where_clause, uses);
    }
}

pub(crate) fn graph_pattern(pattern: &GraphPattern, uses: &mut Vec<FeatureUse>) {
    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => node_pattern(node, uses),
            PatternElement::Edge(edge) => edge_pattern(edge, uses),
        }
    }
}

fn node_pattern(pattern: &NodePattern, uses: &mut Vec<FeatureUse>) {
    for (_, value) in &pattern.properties {
        expr::value(value, uses);
    }
    if let Some(inline_where) = &pattern.inline_where {
        expr::value(inline_where, uses);
    }
}

fn edge_pattern(pattern: &EdgePattern, uses: &mut Vec<FeatureUse>) {
    let complete = !matches!(
        pattern.direction,
        EdgeDirection::Right | EdgeDirection::Left | EdgeDirection::Any
    );
    if pattern.abbreviated {
        record_feature(
            uses,
            if complete {
                FeatureId::G045
            } else {
                FeatureId::G044
            },
            pattern.span,
        );
    } else if complete {
        record_feature(uses, FeatureId::G043, pattern.span);
    }
    if matches!(
        pattern.direction,
        EdgeDirection::Undirected
            | EdgeDirection::LeftOrUndirected
            | EdgeDirection::UndirectedOrRight
    ) {
        record_feature(uses, FeatureId::GH02, pattern.span);
    }
    if let Some(quantifier) = pattern.quantifier {
        record_feature(uses, FeatureId::G036, pattern.span);
        match quantifier {
            Quantifier::GraphPattern { max: Some(_), .. } => {
                record_feature(uses, FeatureId::G060, pattern.span);
            }
            Quantifier::GraphPattern { max: None, .. } => {
                record_feature(uses, FeatureId::G061, pattern.span);
            }
            Quantifier::Questioned => {
                record_feature(uses, FeatureId::G037, pattern.span);
            }
        }
    }
    for (_, value) in &pattern.properties {
        expr::value(value, uses);
    }
    if let Some(inline_where) = &pattern.inline_where {
        expr::value(inline_where, uses);
    }
}

fn let_bindings(bindings: &[LetBinding], uses: &mut Vec<FeatureUse>) {
    if let Some(first) = bindings.first() {
        record_feature(uses, FeatureId::GQ09, first.span);
    }
    for binding in bindings {
        if let Some(ty) = &binding.declared_type {
            expr::gql_type(ty, binding.span, uses);
        }
        expr::value(&binding.value, uses);
    }
}

fn for_statement(statement: &ForStatement, uses: &mut Vec<FeatureUse>) {
    record_feature(uses, FeatureId::GQ10, statement.span);
    if let Some(position) = &statement.position {
        let feature = match position.kind {
            RowExpansionPositionKind::Ordinality => FeatureId::GQ11,
            RowExpansionPositionKind::Offset => FeatureId::GQ24,
        };
        record_feature(uses, feature, statement.span);
    }
    expr::value(&statement.source, uses);
}

fn order_terms(
    terms: &[OrderTerm],
    projection_names: Option<&[DbString]>,
    uses: &mut Vec<FeatureUse>,
) {
    if let Some(first) = terms.first() {
        // Stamp every ORDER BY clause with GA07. The strict spec rule
        // (sort key must be a return alias without GA07) is a bind-pass
        // concern — the Flagger cannot tell at parse time whether a sort key
        // is an alias. The conservative gate records GA07 for any ORDER BY;
        // the generated profile marks GA07 supported, so this stamp does not
        // produce rejections.
        record_feature(uses, FeatureId::GA07, first.span);
    }
    for term in terms {
        sort_key_features(term, projection_names, uses);
        expr::value(&term.expr, uses);
    }
}

fn sort_key_features(
    term: &OrderTerm,
    projection_names: Option<&[DbString]>,
    uses: &mut Vec<FeatureUse>,
) {
    if !matches!(term.expr, ValueExpr::Variable { .. }) {
        record_feature(uses, FeatureId::GQ14, term.expr.span());
    }
    if contains_unprojected_variable(&term.expr, projection_names) {
        record_feature(uses, FeatureId::GQ16, term.expr.span());
    }
    if contains_aggregate_function(&term.expr) {
        record_feature(uses, FeatureId::GF20, term.expr.span());
    }
}

fn contains_unprojected_variable(value: &ValueExpr, projection_names: Option<&[DbString]>) -> bool {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        if let ValueExpr::Variable { name, .. } = value
            && !projection_names.is_some_and(|names| names.contains(name))
        {
            return true;
        }
        value.for_each_child(&mut |child| stack.push(child));
    }
    false
}

fn contains_aggregate_function(value: &ValueExpr) -> bool {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        if let ValueExpr::FunctionCall { name, .. } = value
            && is_aggregate_name(name.first())
        {
            return true;
        }
        value.for_each_child(&mut |child| stack.push(child));
    }
    false
}

fn is_aggregate_name(name: &DbString) -> bool {
    [
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "collect_list",
        "stddev_pop",
        "stddev_samp",
        "percentile_cont",
        "percentile_disc",
    ]
    .iter()
    .any(|candidate| name.as_str().eq_ignore_ascii_case(candidate))
}
