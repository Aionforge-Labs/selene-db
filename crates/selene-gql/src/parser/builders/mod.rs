//! Pair-to-AST builders.

pub(super) mod call;
pub(super) mod catalog_ddl;
pub(super) mod ddl;
pub(super) mod explain;
pub(super) mod expr;
pub(super) mod let_stmt;
pub(super) mod mutation;
pub(super) mod pattern;
pub(super) mod query;
pub(super) mod scopes;
pub(super) mod session;
pub(super) mod transaction;

use std::borrow::Cow;

use pest::iterators::Pair;
use selene_core::DbString;
use selene_profile::{FeatureId, capability};

use crate::{
    ast::{GqlType, QueryPipeline, SetOp, SourceSpan, Statement, util::NonEmpty},
    error::ParserError,
};

use super::Rule;

pub(crate) fn build_statement(program_pair: Pair<'_, Rule>) -> Result<Statement, ParserError> {
    match program_pair.as_rule() {
        Rule::gql_program => {
            let child = first_non_eoi(program_pair)?;
            build_statement(child)
        }
        Rule::query_pipeline => query::build_query_pipeline(program_pair).map(Statement::Query),
        Rule::schema_query => scopes::build_specification(program_pair).map(Statement::Query),
        Rule::call_query_pipeline => {
            query::build_call_query_pipeline(program_pair).map(Statement::Query)
        }
        Rule::composite_query => build_composite(program_pair),
        Rule::chained_query => build_chained(program_pair),
        Rule::pipeline_statement => {
            let span = span(&program_pair);
            let statement = query::build_pipeline_statement(program_pair)?;
            Ok(Statement::Query(QueryPipeline {
                statements: vec![statement],
                span,
                ..QueryPipeline::default()
            }))
        }
        Rule::select_stmt => query::build_select_pipeline(program_pair).map(Statement::Query),
        Rule::mutation_pipeline => {
            mutation::build_mutation_pipeline(program_pair).map(Statement::Mutate)
        }
        Rule::ddl_statement => ddl::build_ddl_statement(program_pair).map(Statement::Ddl),
        Rule::catalog_statement_chain => {
            Err(catalog_ddl::reject_catalog_statement_chain(&program_pair))
        }
        Rule::call_stmt => call::build_top_level_call(program_pair),
        Rule::explain_stmt => explain::build_explain_statement(program_pair),
        Rule::transaction_control => transaction::build_transaction_control(program_pair),
        Rule::session_command => session::build_session_command(program_pair),
        _ => Err(unexpected_pair(program_pair, "expected a GQL program")),
    }
}

pub(super) fn unsupported_feature(pair: &Pair<'_, Rule>, feature_id: FeatureId) -> ParserError {
    let record = capability(feature_id).expect("parser feature IDs are generated capabilities");
    ParserError::UnsupportedFeature {
        feature_id,
        display_name: record.name,
        span: span(pair),
        hint: record.non_support_rationale,
    }
}

fn build_composite(pair: Pair<'_, Rule>) -> Result<Statement, ParserError> {
    let source_span = span(&pair);
    let mut children = pair.into_inner();
    let first = children
        .next()
        .ok_or_else(ParserError::empty_program)
        .and_then(|pair| query::build_query_pipeline(pair))?;
    let mut rest = Vec::new();

    while let Some(op_pair) = children.next() {
        let op = build_set_op(op_pair)?;
        let pipeline = children
            .next()
            .ok_or_else(ParserError::empty_program)
            .and_then(|pair| query::build_query_pipeline(pair))?;
        if focused(&first) != focused(&pipeline) {
            return Err(ParserError::syntax(
                "focused and ambient query arms cannot be mixed (ISO 14.2 SR4)",
                pipeline.span,
                None,
            ));
        }
        rest.push((op, pipeline));
    }

    Ok(Statement::Composite {
        first,
        rest: NonEmpty::try_from_vec(rest)
            .expect("grammar guarantees >= 1: composite_query set operator"),
        span: source_span,
    })
}

fn build_chained(pair: Pair<'_, Rule>) -> Result<Statement, ParserError> {
    let source_span = span(&pair);
    let blocks = pair
        .into_inner()
        .filter(|child| child.as_rule() == Rule::query_pipeline)
        .map(query::build_query_pipeline)
        .collect::<Result<Vec<_>, _>>()?;
    if blocks.is_empty() {
        return Err(ParserError::empty_program());
    }
    if blocks
        .iter()
        .any(|block| focused(block) != focused(&blocks[0]))
    {
        return Err(ParserError::syntax(
            "focused and ambient NEXT statements cannot be mixed (ISO 9.2 SR5)",
            source_span,
            None,
        ));
    }
    Ok(Statement::Chained {
        blocks,
        span: source_span,
    })
}

fn focused(query: &QueryPipeline) -> bool {
    matches!(
        query.working_scopes.first(),
        Some(crate::WorkingScopeClause::Use { .. })
    )
}

fn build_set_op(pair: Pair<'_, Rule>) -> Result<SetOp, ParserError> {
    let child = pair
        .into_inner()
        .next()
        .ok_or_else(|| ParserError::syntax("set operator is empty", SourceSpan::default(), None))?;
    let has_all = contains_word(child.as_str(), "ALL");
    match child.as_rule() {
        Rule::union_op if has_all => Ok(SetOp::UnionAll),
        Rule::union_op => Ok(SetOp::Union),
        Rule::intersect_op if has_all => Ok(SetOp::IntersectAll),
        Rule::intersect_op => Ok(SetOp::Intersect),
        Rule::except_op if has_all => Ok(SetOp::ExceptAll),
        Rule::except_op => Ok(SetOp::Except),
        Rule::otherwise_op => Ok(SetOp::Otherwise),
        _ => Err(unexpected_pair(child, "expected set operator")),
    }
}

/// Case-insensitive whole-word match against a SQL/GQL keyword.
///
/// Used to detect modifier keywords (e.g. `ALL` in `INTERSECT ALL`) without
/// false-matching identifiers that happen to contain the keyword as a
/// substring.
fn contains_word(text: &str, word: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    upper
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .any(|part| part == word)
}

use query::{build_exists_match_body_pipeline, build_filter, build_return_clause};

pub(super) fn first_child(pair: Pair<'_, Rule>) -> Result<Pair<'_, Rule>, ParserError> {
    let pair_span = span(&pair);
    pair.into_inner()
        .next()
        .ok_or_else(|| ParserError::syntax("expected child rule", pair_span, None))
}

fn first_non_eoi(pair: Pair<'_, Rule>) -> Result<Pair<'_, Rule>, ParserError> {
    let pair_span = span(&pair);
    pair.into_inner()
        .find(|child| child.as_rule() != Rule::EOI)
        .ok_or_else(|| ParserError::syntax("empty GQL program", pair_span, None))
}

pub(super) fn span(pair: &Pair<'_, Rule>) -> SourceSpan {
    SourceSpan::from_pest(pair.as_span())
}

/// Construct a database string, mapping the only remaining construction failure
/// (the `IL013` per-string byte cap) to a syntax error at `span`.
pub(super) fn db_string_from_str(
    value: &str,
    span: SourceSpan,
    kind: &'static str,
) -> Result<DbString, ParserError> {
    selene_core::db_string(value).map_err(|err| {
        ParserError::syntax(
            format!("could not construct database string for {kind}: {err}"),
            span,
            Some("string exceeds the maximum byte length".into()),
        )
    })
}

/// Construct a database string from owned text, preserving the parser-local
/// syntax-error mapping for the `IL013` byte cap.
pub(super) fn db_string_from_owned(
    value: String,
    span: SourceSpan,
    kind: &'static str,
) -> Result<DbString, ParserError> {
    DbString::from_string(value).map_err(|err| {
        ParserError::syntax(
            format!("could not construct database string for {kind}: {err}"),
            span,
            Some("string exceeds the maximum byte length".into()),
        )
    })
}

pub(super) fn db_string_pair(pair: Pair<'_, Rule>) -> Result<DbString, ParserError> {
    let source_span = span(&pair);
    let decoded = decode_ident_like(pair.as_str());
    match decoded {
        Cow::Borrowed(value) => db_string_from_str(value, source_span, "identifier"),
        Cow::Owned(value) => db_string_from_owned(value, source_span, "identifier"),
    }
}

/// Build a qualified name as a list of database-string segments.
///
/// Each grammar segment is constructed independently. Quoted segments containing
/// dots stay one segment, so `foo."bar.baz"` and `foo.bar.baz` produce
/// different paths.
pub(super) fn build_qualified_name(pair: Pair<'_, Rule>) -> Result<Vec<DbString>, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::qualified_name);
    let source_span = span(&pair);
    let mut segments = Vec::new();
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::ident | Rule::prop_ident => {
                let canonical = decode_ident_like(child.as_str());
                let segment = match canonical {
                    Cow::Borrowed(value) => {
                        db_string_from_str(value, source_span, "qualified-name segment")?
                    }
                    Cow::Owned(value) => {
                        db_string_from_owned(value, source_span, "qualified-name segment")?
                    }
                };
                segments.push(segment);
            }
            _ => return Err(unexpected_pair(child, "unexpected qualified-name child")),
        }
    }
    if segments.is_empty() {
        return Err(ParserError::syntax(
            "qualified name has no segments",
            source_span,
            None,
        ));
    }
    Ok(segments)
}

pub(super) fn db_string_param(pair: Pair<'_, Rule>) -> Result<DbString, ParserError> {
    let source_span = span(&pair);
    let text = pair.as_str().strip_prefix('$').unwrap_or(pair.as_str());
    db_string_from_str(text, source_span, "parameter")
}

pub(super) fn build_typed_param_ref(
    pair: Pair<'_, Rule>,
) -> Result<(DbString, Option<GqlType>, SourceSpan), ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::typed_param_ref);
    let source_span = span(&pair);
    let mut name = None;
    let mut param_span = None;
    let mut declared_type = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::param_ref => {
                param_span = Some(span(&child));
                name = Some(db_string_param(child)?);
            }
            Rule::type_name => declared_type = Some(expr::build_type_name(child)?),
            _ => return Err(unexpected_pair(child, "unexpected typed parameter child")),
        }
    }
    let name = name.ok_or_else(|| {
        ParserError::syntax(
            "typed parameter reference is missing name",
            source_span,
            None,
        )
    })?;
    let source_span = if declared_type.is_some() {
        source_span
    } else {
        param_span.unwrap_or(source_span)
    };
    Ok((name, declared_type, source_span))
}

/// Decode an identifier-like token into its canonical form.
///
/// Bare (unquoted) identifiers — the common case — are returned borrowed
/// (`Cow::Borrowed`) with zero allocation; only delimited identifiers that
/// must strip delimiters or unescape doubled delimiters allocate. Callers preserve that
/// borrowed/owned shape when constructing the validated database string.
pub(super) fn decode_ident_like(text: &str) -> Cow<'_, str> {
    if let Some(inner) = text.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Cow::Owned(inner.replace("\"\"", "\""))
    } else if let Some(inner) = text.strip_prefix('`').and_then(|s| s.strip_suffix('`')) {
        Cow::Owned(inner.replace("``", "`"))
    } else {
        Cow::Borrowed(text)
    }
}

/// Compare a raw keyword token sequence against an expected canonical form,
/// case- and whitespace-insensitively, without allocating.
///
/// `text` is the source slice (any case, arbitrary internal whitespace runs);
/// `expected` is an already-canonical sequence of upper-case keyword tokens
/// (e.g. `["SIGNED", "INTEGER"]` or `["NOT", "NULL"]`). The match holds iff the
/// whitespace-split tokens of `text` equal `expected` token-for-token under
/// ASCII-case-insensitive comparison. This preserves the multi-token /
/// whitespace-insensitive semantics of the previous
/// `to_ascii_uppercase().split_whitespace().collect().join(" ")` canonicalizer
/// while avoiding the per-call `Vec<&str>` + `String` allocation.
pub(super) fn keyword_tokens_eq(text: &str, expected: &[&str]) -> bool {
    let mut tokens = text.split_whitespace();
    for &want in expected {
        match tokens.next() {
            Some(token) if token.eq_ignore_ascii_case(want) => {}
            _ => return false,
        }
    }
    tokens.next().is_none()
}

/// Return `true` when `text`, with leading whitespace ignored, begins with
/// `keyword` case-insensitively (allocation-free prefix dispatch).
///
/// This mirrors the previous `uppercase + whitespace-normalize + starts_with`
/// behavior, where e.g. `LIST<INT8>` and `RECORD{ a :: INT }` (which carry no
/// space after the keyword) are detected by their leading keyword. The actual
/// element / field types are parsed separately from the pest children.
pub(super) fn keyword_starts_with(text: &str, keyword: &str) -> bool {
    text.trim_start()
        .get(..keyword.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(keyword))
}

pub(super) fn unexpected_pair(pair: Pair<'_, Rule>, message: &'static str) -> ParserError {
    ParserError::syntax(message, span(&pair), None)
}

pub(super) fn not_implemented(pair: &Pair<'_, Rule>, message: &'static str) -> ParserError {
    ParserError::not_implemented(
        message,
        span(pair),
        Some(
            "this construct is not yet supported; use `CALL selene.feature_status()` for feature status or `SHOW PROCEDURES` for registered procedures",
        ),
    )
}
