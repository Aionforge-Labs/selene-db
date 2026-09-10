//! Function, aggregate, subquery, and CASE builders.

use pest::iterators::Pair;
use selene_core::DbString;

use crate::{
    ast::{
        BinaryOp, ExistsBody, NormalForm, SourceSpan, TemporalDurationQualifier, TrimSpec,
        ValueExpr, util::NonEmpty,
    },
    error::ParserError,
};

use super::{Rule, build_value_expr, first_child, literal};
use crate::parser::builders::{
    build_exists_match_body_pipeline, build_qualified_name, db_string_from_owned, pattern, span,
    unexpected_pair,
};

pub(super) enum PredicateKind {
    AllDifferent,
    Same,
}

pub(super) fn build_function_call(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let mut children = pair.into_inner();
    let name_pair = children
        .next()
        .ok_or_else(|| ParserError::syntax("function call is missing name", source_span, None))?;
    let name = build_qualified_name(name_pair)?;
    let mut args = Vec::new();
    for child in children {
        match child.as_rule() {
            Rule::arg_list => {
                args = child
                    .into_inner()
                    .filter(|arg| arg.as_rule() == Rule::expr)
                    .map(|arg| build_value_expr(arg))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            _ => return Err(unexpected_pair(child, "unexpected function-call child")),
        }
    }
    Ok(ValueExpr::FunctionCall {
        name: NonEmpty::try_from_vec(name).expect("grammar guarantees >= 1: qualified_name"),
        args,
        star: false,
        distinct: false,
        span: source_span,
    })
}

pub(super) fn build_elements_function(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    build_keyword_function(pair, "elements")
}

pub(super) fn build_scalar_keyword_function_call(
    pair: Pair<'_, Rule>,
) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let mut name = None;
    let mut args = Vec::new();
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::scalar_keyword_function_name => name = Some(lowercase_db_string(child)?),
            Rule::arg_list => {
                args = child
                    .into_inner()
                    .filter(|arg| arg.as_rule() == Rule::expr)
                    .map(build_value_expr)
                    .collect::<Result<Vec<_>, _>>()?;
            }
            _ => {
                return Err(unexpected_pair(
                    child,
                    "unexpected scalar keyword function child",
                ));
            }
        }
    }
    let name = name.ok_or_else(|| {
        ParserError::syntax("scalar keyword function is missing name", source_span, None)
    })?;
    Ok(ValueExpr::FunctionCall {
        name: NonEmpty::try_from_vec(vec![name])
            .expect("scalar keyword function name is non-empty"),
        args,
        star: false,
        distinct: false,
        span: source_span,
    })
}

pub(super) fn build_labels_function(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let arg = build_value_expr(first_child(pair)?)?;
    Ok(ValueExpr::FunctionCall {
        name: NonEmpty::try_from_vec(vec![db_string_from_owned(
            "labels".to_owned(),
            source_span,
            "keyword function name",
        )?])
        .expect("literal vector is non-empty"),
        args: vec![arg],
        star: false,
        distinct: false,
        span: source_span,
    })
}

pub(super) fn build_path_constructor(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let elements = pair
        .into_inner()
        .filter(|child| child.as_rule() == Rule::expr)
        .map(build_value_expr)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ValueExpr::PathConstructor {
        elements,
        span: source_span,
    })
}

pub(super) fn build_current_datetime_function(
    pair: Pair<'_, Rule>,
) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let function_pair = first_child(pair)?;
    let name = match function_pair.as_rule() {
        Rule::current_date_function => "current_date",
        Rule::current_time_function => "current_time",
        Rule::current_timestamp_function => "current_timestamp",
        Rule::local_timestamp_function => "local_datetime",
        Rule::local_time_function => "local_time",
        _ => {
            return Err(unexpected_pair(
                function_pair,
                "unexpected current-datetime function",
            ));
        }
    };
    Ok(ValueExpr::FunctionCall {
        name: NonEmpty::try_from_vec(vec![db_string_from_owned(
            name.to_owned(),
            source_span,
            "current-datetime function name",
        )?])
        .expect("literal vector is non-empty"),
        args: Vec::new(),
        star: false,
        distinct: false,
        span: source_span,
    })
}

pub(super) fn build_duration_between_expr(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let mut start = None;
    let mut end = None;
    let mut qualifier = TemporalDurationQualifier::DayToSecond;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::duration_between_kw => {}
            Rule::expr if start.is_none() => start = Some(build_value_expr(child)?),
            Rule::expr => end = Some(build_value_expr(child)?),
            Rule::temporal_duration_qualifier => qualifier = build_duration_qualifier(child)?,
            _ => return Err(unexpected_pair(child, "unexpected DURATION_BETWEEN child")),
        }
    }
    let start = start.ok_or_else(|| {
        ParserError::syntax(
            "DURATION_BETWEEN is missing start expression",
            source_span,
            None,
        )
    })?;
    let end = end.ok_or_else(|| {
        ParserError::syntax(
            "DURATION_BETWEEN is missing end expression",
            source_span,
            None,
        )
    })?;
    Ok(ValueExpr::DurationBetween {
        start: Box::new(start),
        end: Box::new(end),
        qualifier,
        span: source_span,
    })
}

fn build_duration_qualifier(
    pair: Pair<'_, Rule>,
) -> Result<TemporalDurationQualifier, ParserError> {
    let child = first_child(pair)?;
    match child.as_rule() {
        Rule::year_to_month_qualifier => Ok(TemporalDurationQualifier::YearToMonth),
        Rule::day_to_second_qualifier => Ok(TemporalDurationQualifier::DayToSecond),
        _ => Err(unexpected_pair(
            child,
            "unexpected temporal duration qualifier",
        )),
    }
}

fn build_keyword_function(pair: Pair<'_, Rule>, name: &str) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let mut args = Vec::new();
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::elements_synonym => {}
            Rule::arg_list => {
                args = child
                    .into_inner()
                    .filter(|arg| arg.as_rule() == Rule::expr)
                    .map(|arg| build_value_expr(arg))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            _ => return Err(unexpected_pair(child, "unexpected keyword-function child")),
        }
    }
    keyword_function_expr(name, args, source_span)
}

fn keyword_function_expr(
    name: &str,
    args: Vec<ValueExpr>,
    source_span: SourceSpan,
) -> Result<ValueExpr, ParserError> {
    Ok(ValueExpr::FunctionCall {
        name: NonEmpty::try_from_vec(vec![db_string_from_owned(
            name.to_owned(),
            source_span,
            "keyword function name",
        )?])
        .expect("literal vector is non-empty"),
        args,
        star: false,
        distinct: false,
        span: source_span,
    })
}

pub(super) fn build_aggregate_expr(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let mut name = None;
    let mut distinct = false;
    let mut quantifier = false;
    let mut star = false;
    let mut args = Vec::new();

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::aggregate_op | Rule::binary_aggregate_op => {
                name = Some(lowercase_db_string(child)?)
            }
            Rule::distinct_kw => {
                distinct = true;
                quantifier = true;
            }
            Rule::all_kw => quantifier = true,
            Rule::star => star = true,
            Rule::expr => args.push(build_value_expr(child)?),
            _ => return Err(unexpected_pair(child, "unexpected aggregate child")),
        }
    }

    let segment = name.ok_or_else(|| {
        ParserError::syntax("aggregate expression is missing name", source_span, None)
    })?;
    validate_aggregate_shape(&segment, quantifier, star, args.len(), source_span)?;
    Ok(ValueExpr::FunctionCall {
        name: NonEmpty::try_from_vec(vec![segment]).expect("grammar guarantees >= 1: aggregate_op"),
        args,
        star,
        distinct,
        span: source_span,
    })
}

fn validate_aggregate_shape(
    name: &DbString,
    quantifier: bool,
    star: bool,
    arg_count: usize,
    span: SourceSpan,
) -> Result<(), ParserError> {
    if star {
        if name.as_str() == "count" && !quantifier {
            return Ok(());
        }
        return Err(ParserError::syntax(
            "only COUNT(*) may use aggregate asterisk syntax",
            span,
            None,
        ));
    }
    if arg_count == 0 {
        return Err(ParserError::syntax(
            "aggregate function is missing value expression",
            span,
            None,
        ));
    }
    Ok(())
}

pub(super) fn build_normalize_expr(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let mut source = None;
    let mut form = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::expr => source = Some(build_value_expr(child)?),
            Rule::normal_form => form = Some(parse_normal_form(child.as_str())),
            _ => return Err(unexpected_pair(child, "unexpected NORMALIZE child")),
        }
    }
    Ok(ValueExpr::Normalize {
        source: Box::new(source.ok_or_else(|| {
            ParserError::syntax("NORMALIZE is missing source expression", source_span, None)
        })?),
        form,
        span: source_span,
    })
}

pub(super) fn build_trim_expr(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::trim_kw => {}
            Rule::trim_spec_operands => return build_trim_spec_operands(child, source_span),
            Rule::trim_value_operands => return build_trim_value_operands(child, source_span),
            _ => return Err(unexpected_pair(child, "unexpected TRIM child")),
        }
    }
    Err(ParserError::syntax(
        "TRIM is missing operands",
        source_span,
        None,
    ))
}

fn build_trim_spec_operands(
    pair: Pair<'_, Rule>,
    source_span: SourceSpan,
) -> Result<ValueExpr, ParserError> {
    let mut spec = TrimSpec::Both;
    let mut character = None;
    let mut source = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::trim_spec => spec = parse_trim_spec(child.as_str()),
            Rule::trim_char => {
                character = Some(Box::new(build_value_expr(first_child(child)?)?));
            }
            Rule::from_kw => {}
            Rule::expr => source = Some(build_value_expr(child)?),
            _ => return Err(unexpected_pair(child, "unexpected explicit TRIM child")),
        }
    }
    trim_from_expr(spec, character, source, source_span)
}

fn build_trim_value_operands(
    pair: Pair<'_, Rule>,
    source_span: SourceSpan,
) -> Result<ValueExpr, ParserError> {
    let mut children = pair.into_inner();
    let first_pair = children.next().ok_or_else(|| {
        ParserError::syntax("TRIM is missing source expression", source_span, None)
    })?;
    let first = build_value_expr(first_pair)?;
    match children.next() {
        None => keyword_function_expr("trim", vec![first], source_span),
        Some(tail) => {
            let tail = if tail.as_rule() == Rule::trim_value_tail {
                first_child(tail)?
            } else {
                tail
            };
            match tail.as_rule() {
                Rule::trim_from_tail => {
                    let source = expr_from_child(tail)?;
                    trim_from_expr(
                        TrimSpec::Both,
                        Some(Box::new(first)),
                        Some(source),
                        source_span,
                    )
                }
                Rule::trim_list_tail => {
                    let count = expr_from_child(tail)?;
                    keyword_function_expr("trim", vec![first, count], source_span)
                }
                _ => Err(unexpected_pair(tail, "unexpected TRIM operand tail")),
            }
        }
    }
}

fn trim_from_expr(
    spec: TrimSpec,
    character: Option<Box<ValueExpr>>,
    source: Option<ValueExpr>,
    source_span: SourceSpan,
) -> Result<ValueExpr, ParserError> {
    let source = source.ok_or_else(|| {
        ParserError::syntax("TRIM is missing source expression", source_span, None)
    })?;
    Ok(ValueExpr::Trim {
        spec,
        character,
        source: Box::new(source),
        span: source_span,
    })
}

pub(super) fn build_expr_list_predicate(
    pair: Pair<'_, Rule>,
    kind: PredicateKind,
) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let items = pair
        .into_inner()
        .filter(|child| child.as_rule() == Rule::expr)
        .map(|child| build_value_expr(child))
        .collect::<Result<Vec<_>, _>>()?;
    match kind {
        PredicateKind::AllDifferent => Ok(ValueExpr::AllDifferent {
            items,
            span: source_span,
        }),
        PredicateKind::Same => Ok(ValueExpr::Same {
            items,
            span: source_span,
        }),
    }
}

pub(super) fn build_property_exists(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let mut target = None;
    let mut key = None;
    let mut key_source_kind = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::expr => target = Some(build_value_expr(child)?),
            Rule::string_lit => {
                let (parsed_key, parsed_kind) = literal::parse_string_pair_with_kind(child)?;
                key = Some(parsed_key);
                key_source_kind = Some(parsed_kind);
            }
            _ => return Err(unexpected_pair(child, "unexpected PROPERTY_EXISTS child")),
        }
    }
    Ok(ValueExpr::PropertyExists {
        target: Box::new(target.ok_or_else(|| {
            ParserError::syntax("PROPERTY_EXISTS is missing target", source_span, None)
        })?),
        key: key.ok_or_else(|| {
            ParserError::syntax("PROPERTY_EXISTS is missing property key", source_span, None)
        })?,
        key_source_kind: key_source_kind.ok_or_else(|| {
            ParserError::syntax("PROPERTY_EXISTS is missing property key", source_span, None)
        })?,
        span: source_span,
    })
}

pub(super) fn build_exists(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let negated = pair.as_str().to_ascii_uppercase().starts_with("NOT");
    let body_pair = pair
        .into_inner()
        .find(|child| child.as_rule() == Rule::exists_body)
        .ok_or_else(|| ParserError::syntax("EXISTS is missing body", source_span, None))?;
    let body_child = body_pair
        .into_inner()
        .find(|child| {
            matches!(
                child.as_rule(),
                Rule::exists_match_body | Rule::graph_pattern_list
            )
        })
        .ok_or_else(|| ParserError::syntax("EXISTS is missing pattern", source_span, None))?;
    let body = match body_child.as_rule() {
        Rule::exists_match_body => {
            let mut children = body_child.clone().into_inner();
            let first = children.next().ok_or_else(|| {
                ParserError::syntax("EXISTS match body is missing MATCH", source_span, None)
            })?;
            if children.next().is_some() {
                ExistsBody::Query(Box::new(build_exists_match_body_pipeline(body_child)?))
            } else {
                ExistsBody::Match(Box::new(pattern::build_match_clause(first)?))
            }
        }
        Rule::graph_pattern_list => ExistsBody::Match(Box::new(
            pattern::build_match_clause_from_graph_pattern_list(body_child, source_span)?,
        )),
        _ => return Err(unexpected_pair(body_child, "unexpected EXISTS body")),
    };
    Ok(ValueExpr::Exists {
        body,
        negated,
        span: source_span,
    })
}

pub(super) fn build_value_subquery(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    let body_pair = pair
        .into_inner()
        .find(|child| child.as_rule() == Rule::query_specification)
        .ok_or_else(|| {
            ParserError::syntax("VALUE subquery is missing query body", source_span, None)
        })?;
    Ok(ValueExpr::ValueSubquery {
        body: Box::new(super::super::scopes::build_specification(body_pair)?),
        span: source_span,
    })
}

pub(super) fn build_case_expr(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    match pair.as_rule() {
        Rule::simple_case => build_simple_case(pair, source_span),
        Rule::searched_case => build_searched_case(pair, source_span),
        _ => Err(unexpected_pair(pair, "expected CASE expression")),
    }
}

fn is_case_keyword_token(rule: Rule) -> bool {
    matches!(
        rule,
        Rule::case_kw | Rule::when_kw | Rule::then_kw | Rule::else_kw | Rule::end_kw
    )
}

fn build_simple_case(
    pair: Pair<'_, Rule>,
    source_span: SourceSpan,
) -> Result<ValueExpr, ParserError> {
    let mut children = pair
        .into_inner()
        .filter(|child| !is_case_keyword_token(child.as_rule()));
    let base =
        build_value_expr(children.next().ok_or_else(|| {
            ParserError::syntax("simple CASE is missing input", source_span, None)
        })?)?;
    let mut branches = Vec::new();
    let mut else_branch = None;

    for child in children {
        match child.as_rule() {
            Rule::simple_when => branches.push(simple_when_branch(child, &base)?),
            Rule::else_clause => else_branch = Some(Box::new(expr_from_child(child)?)),
            _ => return Err(unexpected_pair(child, "unexpected CASE child")),
        }
    }

    Ok(ValueExpr::Case {
        branches,
        else_branch,
        span: source_span,
    })
}

fn simple_when_branch(
    pair: Pair<'_, Rule>,
    base: &ValueExpr,
) -> Result<(ValueExpr, ValueExpr), ParserError> {
    let when_span = span(&pair);
    let mut operands = pair
        .into_inner()
        .filter(|child| child.as_rule() == Rule::expr)
        .map(build_value_expr)
        .collect::<Result<Vec<_>, _>>()?;
    let then_value = operands
        .pop()
        .ok_or_else(|| ParserError::syntax("CASE THEN is missing expression", when_span, None))?;
    let mut operands = operands.into_iter();
    let first_operand = operands
        .next()
        .ok_or_else(|| ParserError::syntax("CASE WHEN is missing expression", when_span, None))?;
    let mut condition = simple_when_comparison(base, first_operand);
    for operand in operands {
        let rhs = simple_when_comparison(base, operand);
        condition = ValueExpr::BinaryOp {
            op: BinaryOp::Or,
            span: SourceSpan::merge(condition.span(), rhs.span()),
            lhs: Box::new(condition),
            rhs: Box::new(rhs),
        };
    }
    Ok((condition, then_value))
}

fn simple_when_comparison(base: &ValueExpr, operand: ValueExpr) -> ValueExpr {
    ValueExpr::BinaryOp {
        op: BinaryOp::Eq,
        span: SourceSpan::merge(base.span(), operand.span()),
        lhs: Box::new(base.clone()),
        rhs: Box::new(operand),
    }
}

fn build_searched_case(
    pair: Pair<'_, Rule>,
    source_span: SourceSpan,
) -> Result<ValueExpr, ParserError> {
    let mut branches = Vec::new();
    let mut else_branch = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            rule if is_case_keyword_token(rule) => {}
            Rule::when_clause => branches.push(searched_when_branch(child)?),
            Rule::else_clause => else_branch = Some(Box::new(expr_from_child(child)?)),
            _ => return Err(unexpected_pair(child, "unexpected CASE child")),
        }
    }
    Ok(ValueExpr::Case {
        branches,
        else_branch,
        span: source_span,
    })
}

fn searched_when_branch(pair: Pair<'_, Rule>) -> Result<(ValueExpr, ValueExpr), ParserError> {
    let when_span = span(&pair);
    let mut children = pair
        .into_inner()
        .filter(|child| child.as_rule() == Rule::expr);
    let condition =
        build_value_expr(children.next().ok_or_else(|| {
            ParserError::syntax("CASE WHEN is missing condition", when_span, None)
        })?)?;
    let value = build_value_expr(
        children
            .next()
            .ok_or_else(|| ParserError::syntax("CASE THEN is missing value", when_span, None))?,
    )?;
    Ok((condition, value))
}

fn expr_from_child(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    pair.into_inner()
        .find(|child| child.as_rule() == Rule::expr)
        .ok_or_else(|| ParserError::syntax("clause is missing expression", source_span, None))
        .and_then(|pair| build_value_expr(pair))
}

fn lowercase_db_string(pair: Pair<'_, Rule>) -> Result<DbString, ParserError> {
    let source_span = span(&pair);
    let canonical = pair.as_str().to_ascii_lowercase();
    db_string_from_owned(canonical, source_span, "aggregate name")
}

fn parse_normal_form(value: &str) -> NormalForm {
    match value.to_ascii_uppercase().as_str() {
        "NFD" => NormalForm::Nfd,
        "NFKC" => NormalForm::Nfkc,
        "NFKD" => NormalForm::Nfkd,
        _ => NormalForm::Nfc,
    }
}

fn parse_trim_spec(value: &str) -> TrimSpec {
    match value.to_ascii_uppercase().as_str() {
        "LEADING" => TrimSpec::Leading,
        "TRAILING" => TrimSpec::Trailing,
        _ => TrimSpec::Both,
    }
}
