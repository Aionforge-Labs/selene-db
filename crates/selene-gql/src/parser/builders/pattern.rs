//! Graph-pattern pair-to-AST builders.

use pest::iterators::Pair;

use crate::{
    ast::{
        EdgeDirection, EdgePattern, GraphPattern, LabelExpr, MatchClause, MatchMode, NodePattern,
        PathMode, PathSelector, PatternElement, Quantifier, SourceSpan, ValueExpr, Vec2OrMore,
    },
    error::ParserError,
};

use super::{Rule, db_string_pair, expr, first_child, keyword_tokens_eq, span, unexpected_pair};

pub(super) fn build_match_clause(pair: Pair<'_, Rule>) -> Result<MatchClause, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::match_stmt);
    let source_span = span(&pair);
    let mut optional = false;
    let mut selector = None;
    let mut match_mode = None;
    let mut path_mode = PathMode::Walk;
    let mut path_mode_explicit = false;
    let mut trailing_path_mode = false;
    // ISO §16.6 <path or paths> (Feature G014) is pure surface sugar (§1.2.4). It
    // reaches the AST from two grammar sites: nested inside `counted_shortest_tail`
    // (its ISO position for the counted SHORTEST forms) or the trailing `match_stmt`
    // slot (the ALL/ANY/<path mode prefix> forms). `path_or_paths` records presence
    // for the flagger (G014, recorded iff true); `trailing_path_or_paths` tracks the
    // trailing-slot site only, which the conformance guards below constrain.
    let mut path_or_paths = false;
    let mut trailing_path_or_paths = false;
    let mut patterns = Vec::new();
    let mut where_clause = None;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::match_kw => {}
            Rule::optional_modifier => optional = true,
            Rule::path_selector => {
                selector = Some(build_path_selector(&child)?);
                // A counted SHORTEST form may carry <path mode> and <path or
                // paths> in their ISO positions inside counted_shortest_tail.
                // Recover those surface bits because they still belong to the
                // MatchClause, not the selector enum.
                for nested in child.clone().into_inner().flatten() {
                    match nested.as_rule() {
                        Rule::path_or_paths => path_or_paths = true,
                        Rule::path_modifier => {
                            path_mode = build_path_mode(&nested)?;
                            path_mode_explicit = true;
                        }
                        _ => {}
                    }
                }
            }
            Rule::match_mode => match_mode = Some(build_match_mode(&child)?),
            Rule::path_modifier => {
                if path_mode_explicit {
                    return Err(ParserError::syntax(
                        "path mode may be specified only once in a MATCH prefix",
                        span(&child),
                        None,
                    ));
                }
                path_mode = build_path_mode(&child)?;
                path_mode_explicit = true;
                trailing_path_mode = true;
            }
            Rule::path_or_paths => {
                if path_or_paths {
                    return Err(ParserError::syntax(
                        "PATH/PATHS may be specified only once in a MATCH prefix",
                        span(&child),
                        None,
                    ));
                }
                path_or_paths = true;
                trailing_path_or_paths = true;
            }
            Rule::graph_pattern_list => patterns = build_graph_pattern_list(child)?,
            Rule::where_clause => where_clause = Some(expr_from_child(child)?),
            _ => return Err(unexpected_pair(child, "unexpected MATCH child")),
        }
    }

    if patterns.is_empty() {
        return Err(ParserError::syntax(
            "MATCH is missing a graph pattern",
            source_span,
            None,
        ));
    }

    // The conformance constraints below apply to the TRAILING <path or paths> slot
    // only. A counted-form <path or paths> (parsed inside counted_shortest_tail) is
    // already in its exact ISO position, so it is unconditionally legal.

    // ISO §16.6: <path or paths> never appears standalone — it is the optional
    // trailing token of a <path mode prefix> (which REQUIRES a <path mode>) or a
    // <path search prefix> (ALL / ANY / SHORTEST). A bare `MATCH PATHS (n)` with
    // neither a path selector nor an explicit path mode is therefore not conforming.
    if trailing_path_or_paths && selector.is_none() && !path_mode_explicit {
        return Err(ParserError::syntax(
            "PATH/PATHS must follow a path mode (WALK/TRAIL/SIMPLE/ACYCLIC) or a \
             path search prefix (ALL/ANY/SHORTEST) per ISO/IEC 39075:2024 §16.6",
            source_span,
            None,
        ));
    }

    // ISO §16.6 binds <path or paths> to the path-pattern prefix (`ALL [mode] [PATHS]`,
    // etc.); the §16.4 <match mode> (DIFFERENT EDGES / REPEATABLE ELEMENTS) is a
    // separate, graph-level construct and cannot sit between the prefix and
    // <path or paths>. selene's flattened order would otherwise accept
    // `ALL DIFFERENT EDGES PATHS` — reject it.
    if trailing_path_or_paths && match_mode.is_some() {
        return Err(ParserError::syntax(
            "PATH/PATHS may not be separated from the path prefix by a match mode \
             (DIFFERENT EDGES / REPEATABLE ELEMENTS) per ISO/IEC 39075:2024 §16.6",
            source_span,
            None,
        ));
    }

    // ISO §16.6 <counted shortest group search> places <path or paths> BEFORE the
    // GROUP/GROUPS discriminator (`SHORTEST [n] [mode] [path-or-paths] {GROUP|
    // GROUPS}`). The conforming `SHORTEST n TRAIL PATHS GROUPS` spelling is parsed
    // inside counted_shortest_tail; a TRAILING slot here means PATH/PATHS came AFTER
    // GROUP[S] (`SHORTEST n GROUPS PATHS`), which is the wrong ISO order — reject it.
    if trailing_path_or_paths && matches!(selector, Some(PathSelector::CountedShortestGroup { .. }))
    {
        return Err(ParserError::syntax(
            "PATH/PATHS must precede GROUP/GROUPS (write SHORTEST n PATHS GROUPS) per \
             ISO/IEC 39075:2024 §16.6",
            source_span,
            None,
        ));
    }

    // Same ordering rule for <path mode>: in counted group syntax, WALK/TRAIL/
    // SIMPLE/ACYCLIC appears before GROUP/GROUPS. The flattened top-level
    // path_modifier slot can otherwise accept `SHORTEST n GROUPS TRAIL`, so
    // reject that wrong-order spelling here.
    if trailing_path_mode && matches!(selector, Some(PathSelector::CountedShortestGroup { .. })) {
        return Err(ParserError::syntax(
            "path mode must precede GROUP/GROUPS (write SHORTEST n TRAIL GROUPS) per \
             ISO/IEC 39075:2024 §16.6",
            source_span,
            None,
        ));
    }

    Ok(MatchClause {
        optional,
        selector,
        match_mode,
        path_mode,
        path_mode_explicit,
        path_or_paths,
        patterns,
        where_clause,
        span: source_span,
    })
}

pub(super) fn build_match_clause_from_graph_pattern_list(
    pair: Pair<'_, Rule>,
    source_span: SourceSpan,
) -> Result<MatchClause, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::graph_pattern_list);
    let patterns = build_graph_pattern_list(pair)?;
    if patterns.is_empty() {
        return Err(ParserError::syntax(
            "EXISTS graph pattern is missing a graph pattern",
            source_span,
            None,
        ));
    }
    Ok(MatchClause {
        optional: false,
        selector: None,
        match_mode: None,
        path_mode: PathMode::Walk,
        path_mode_explicit: false,
        path_or_paths: false,
        patterns,
        where_clause: None,
        span: source_span,
    })
}

fn build_path_selector(pair: &Pair<'_, Rule>) -> Result<PathSelector, ParserError> {
    // Per ISO 39075:2024 §16.6: counted selector forms carry structured count
    // children. Dispatch on those children first so they never fall through to
    // the keyword-text match below.
    if let Some(tail) = pair
        .clone()
        .into_inner()
        .find(|child| child.as_rule() == Rule::counted_shortest_tail)
    {
        return build_counted_shortest(tail);
    }
    if let Some(count) = pair
        .clone()
        .into_inner()
        .find(|child| child.as_rule() == Rule::any_path_count)
    {
        return Ok(PathSelector::Any {
            paths: parse_positive_path_count(&count)?,
        });
    }

    // Match the selector keyword(s) token-wise (case- and whitespace-
    // insensitive) without allocating a normalized `String`. `ANY SHORTEST` /
    // `ALL SHORTEST` keep their two-token match; check them before the bare
    // single-token `ANY`/`ALL` so the longer spelling wins.
    let text = pair.as_str();
    if keyword_tokens_eq(text, &["ANY", "SHORTEST"]) {
        Ok(PathSelector::AnyShortest)
    } else if keyword_tokens_eq(text, &["ALL", "SHORTEST"]) {
        Ok(PathSelector::AllShortest)
    } else if keyword_tokens_eq(text, &["ANY"]) {
        Ok(PathSelector::Any { paths: 1 })
    } else if keyword_tokens_eq(text, &["ALL"]) {
        Ok(PathSelector::All)
    } else {
        Err(ParserError::syntax(
            "unknown path selector",
            span(pair),
            None,
        ))
    }
}

// Build the counted shortest selector from a `counted_shortest_tail` pair.
//
// Per ISO 39075:2024 §16.6 the tail is either a counted shortest path
// (`uint` plus optional path-mode / PATHS surface tokens) or a counted shortest
// group (`counted_group_kw` with optional leading count and optional path-mode /
// PATHS surface tokens):
//   - a `uint` with no GROUP keyword -> G019 counted shortest PATH (paths = N);
//   - a `counted_group_kw` (with or without a leading `uint`) -> G020 counted
//     shortest GROUP (groups = N, defaulting to 1 when no `uint` is written,
//     per §16.6 SR2b).
//
// §16.6 SR2bii requires a written literal count to be positive; a literal `0`
// (`SHORTEST 0` / `SHORTEST 0 GROUPS`) is a static violation rejected here with
// GQLSTATUS 22G0F. A literal too large for `u32` saturates to `u32::MAX` rather
// than erroring, because keep-up-to-N semantics make any count `>= |partition|`
// equivalent to keep-all.
fn build_counted_shortest(tail: Pair<'_, Rule>) -> Result<PathSelector, ParserError> {
    let tail_span = span(&tail);
    let mut count = None;
    let mut is_group = false;
    for child in tail.into_inner() {
        match child.as_rule() {
            Rule::uint => count = Some(parse_positive_path_count(&child)?),
            Rule::counted_group_kw => is_group = true,
            Rule::path_modifier => {}
            // ISO §16.6 <path or paths> (Feature G014) in its counted-form position.
            // It is pure surface sugar with no effect on the selector; the presence
            // bit is recovered by build_match_clause (which scans the path_selector
            // subtree), so it is ignored here.
            Rule::path_or_paths => {}
            _ => return Err(unexpected_pair(child, "unexpected counted-shortest child")),
        }
    }

    if is_group {
        // SHORTEST [N] GROUP[S]: N defaults to 1 per §16.6 SR2b.
        Ok(PathSelector::CountedShortestGroup {
            groups: count.unwrap_or(1),
        })
    } else {
        // SHORTEST N (no GROUP): the count is mandatory in this grammar branch.
        let paths = count.ok_or_else(|| {
            ParserError::syntax(
                "counted shortest path is missing its count",
                tail_span,
                None,
            )
        })?;
        Ok(PathSelector::CountedShortest { paths })
    }
}

// Parse an ISO path-selector count literal to a positive `u32`.
//
// Saturates an out-of-range literal to `u32::MAX` (keep-all). Rejects a literal
// `0` with GQLSTATUS 22G0F per ISO 39075:2024 §16.6 SR2bii / §22.4 GR7.
fn parse_positive_path_count(pair: &Pair<'_, Rule>) -> Result<u32, ParserError> {
    let value = pair.as_str().parse::<u32>().unwrap_or(u32::MAX);
    if value == 0 {
        return Err(ParserError::syntax_with_status(
            crate::error::GqlStatus::INVALID_NUMBER_OF_PATHS_OR_GROUPS,
            "path selector count must be a positive integer",
            span(pair),
            Some(
                "write a positive ANY/SHORTEST count; 0 is invalid per ISO 39075:2024 §16.6".into(),
            ),
        ));
    }
    Ok(value)
}

fn build_match_mode(pair: &Pair<'_, Rule>) -> Result<MatchMode, ParserError> {
    // The grammar factors <match mode> (ISO §16.4) as
    //   different_mode_kw  ~ edge_bindings_or_edges        (DIFFERENT EDGES, G002)
    //   repeatable_mode_kw ~ element_bindings_or_elements  (REPEATABLE ELEMENTS, G003)
    // where the leading mode keyword is a boundary-guarded atomic rule. Dispatch
    // on that first child *rule* rather than slicing `as_str()`: the `~` skips
    // implicit WHITESPACE *and* COMMENTs, so a comment can sit flush against the
    // keyword with no surrounding space (e.g. "DIFFERENT/*x*/EDGE"), which a raw
    // `split_whitespace()` would see as one un-delimited blob and wrongly reject.
    // Structural dispatch is immune to every whitespace/comment spelling. All
    // edge-/element-family synonyms collapse onto the two semantics here.
    match pair
        .clone()
        .into_inner()
        .next()
        .map(|inner| inner.as_rule())
    {
        Some(Rule::different_mode_kw) => Ok(MatchMode::DifferentEdges),
        Some(Rule::repeatable_mode_kw) => Ok(MatchMode::RepeatableElements),
        _ => Err(ParserError::syntax("unknown match mode", span(pair), None)),
    }
}

fn build_path_mode(pair: &Pair<'_, Rule>) -> Result<PathMode, ParserError> {
    let text = pair.as_str().to_ascii_uppercase();
    match text.as_str() {
        "WALK" => Ok(PathMode::Walk),
        "ACYCLIC" => Ok(PathMode::Acyclic),
        "SIMPLE" => Ok(PathMode::Simple),
        "TRAIL" => Ok(PathMode::Trail),
        _ => Err(ParserError::syntax("unknown path mode", span(pair), None)),
    }
}

fn build_graph_pattern_list(pair: Pair<'_, Rule>) -> Result<Vec<GraphPattern>, ParserError> {
    pair.into_inner()
        .filter(|child| child.as_rule() == Rule::graph_pattern)
        .map(|child| build_graph_pattern(child))
        .collect()
}

fn build_graph_pattern(pair: Pair<'_, Rule>) -> Result<GraphPattern, ParserError> {
    let source_span = span(&pair);
    let mut path_binding = None;
    let mut elements = Vec::new();

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::path_var_binding => {
                path_binding = Some(db_string_pair(first_child(child)?)?);
            }
            Rule::pattern_chain => elements = build_pattern_chain(child)?,
            _ => return Err(unexpected_pair(child, "unexpected graph-pattern child")),
        }
    }

    if elements.is_empty() {
        return Err(ParserError::syntax(
            "graph pattern is empty",
            source_span,
            None,
        ));
    }

    Ok(GraphPattern {
        path_binding,
        elements,
        span: source_span,
    })
}

fn build_pattern_chain(pair: Pair<'_, Rule>) -> Result<Vec<PatternElement>, ParserError> {
    let mut elements = Vec::new();
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::node_pattern => {
                elements.push(PatternElement::Node(build_node_pattern(child)?));
            }
            Rule::edge_pattern => {
                elements.push(PatternElement::Edge(build_edge_pattern(child)?));
            }
            _ => return Err(unexpected_pair(child, "unexpected pattern-chain child")),
        }
    }
    Ok(elements)
}

fn build_node_pattern(pair: Pair<'_, Rule>) -> Result<NodePattern, ParserError> {
    let source_span = span(&pair);
    let mut binding = None;
    let mut label_expr = None;
    let mut properties = Vec::new();
    let mut inline_where = None;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::node_var => binding = Some(db_string_pair(first_child(child)?)?),
            Rule::label_expr => label_expr = Some(build_label_expr(child)?),
            Rule::property_map => properties = build_property_map(child)?,
            Rule::inline_where => inline_where = Some(expr_from_child(child)?),
            _ => return Err(unexpected_pair(child, "unexpected node-pattern child")),
        }
    }

    Ok(NodePattern {
        binding,
        label_expr,
        properties,
        inline_where,
        span: source_span,
    })
}

fn build_edge_pattern(pair: Pair<'_, Rule>) -> Result<EdgePattern, ParserError> {
    let source_span = span(&pair);
    let child = first_child(pair)?;
    let direction = match child.as_rule() {
        Rule::edge_right | Rule::abbrev_right => EdgeDirection::Right,
        Rule::edge_left | Rule::abbrev_left => EdgeDirection::Left,
        Rule::edge_any | Rule::abbrev_any => EdgeDirection::Any,
        Rule::edge_undirected | Rule::abbrev_undirected => EdgeDirection::Undirected,
        Rule::edge_left_or_undirected | Rule::abbrev_left_or_undirected => {
            EdgeDirection::LeftOrUndirected
        }
        Rule::edge_undirected_or_right | Rule::abbrev_undirected_or_right => {
            EdgeDirection::UndirectedOrRight
        }
        Rule::edge_left_or_right | Rule::abbrev_left_or_right => EdgeDirection::LeftOrRight,
        _ => return Err(unexpected_pair(child, "expected edge pattern")),
    };

    let mut pattern = EdgePattern {
        binding: None,
        direction,
        abbreviated: matches!(
            child.as_rule(),
            Rule::abbrev_right
                | Rule::abbrev_left
                | Rule::abbrev_any
                | Rule::abbrev_undirected
                | Rule::abbrev_left_or_undirected
                | Rule::abbrev_undirected_or_right
                | Rule::abbrev_left_or_right
        ),
        label_expr: None,
        properties: Vec::new(),
        quantifier: None,
        inline_where: None,
        span: source_span,
    };

    for child in child.into_inner() {
        match child.as_rule() {
            Rule::edge_interior => apply_edge_interior(child, &mut pattern)?,
            Rule::quantifier => assign_quantifier(&mut pattern, child)?,
            _ => return Err(unexpected_pair(child, "unexpected edge-pattern child")),
        }
    }

    Ok(pattern)
}

fn assign_quantifier(pattern: &mut EdgePattern, pair: Pair<'_, Rule>) -> Result<(), ParserError> {
    if pattern.quantifier.is_some() {
        // Quantifiers can appear inside edge_interior or after the closing
        // bracket; the grammar does not forbid both forms in the same edge,
        // so the builder rejects the conflict explicitly. Silently keeping
        // the second occurrence would change traversal bounds without the
        // author noticing.
        return Err(ParserError::syntax(
            "edge pattern carries conflicting quantifiers",
            span(&pair),
            Some("specify the quantifier exactly once".into()),
        ));
    }
    pattern.quantifier = Some(build_quantifier(pair)?);
    Ok(())
}

fn apply_edge_interior(pair: Pair<'_, Rule>, pattern: &mut EdgePattern) -> Result<(), ParserError> {
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::edge_var => pattern.binding = Some(db_string_pair(first_child(child)?)?),
            Rule::label_expr => pattern.label_expr = Some(build_label_expr(child)?),
            Rule::property_map => pattern.properties = build_property_map(child)?,
            Rule::quantifier => assign_quantifier(pattern, child)?,
            Rule::inline_where => pattern.inline_where = Some(expr_from_child(child)?),
            _ => return Err(unexpected_pair(child, "unexpected edge-interior child")),
        }
    }
    Ok(())
}

pub(super) fn build_label_expr(pair: Pair<'_, Rule>) -> Result<LabelExpr, ParserError> {
    match pair.as_rule() {
        Rule::label_expr => build_label_expr(first_child(pair)?),
        Rule::label_or => {
            let parts = pair
                .into_inner()
                .filter(|child| child.as_rule() == Rule::label_and)
                .map(|child| build_label_expr(child))
                .collect::<Result<Vec<_>, _>>()?;
            collapse_label_parts(parts, LabelExpr::Disjunction)
        }
        Rule::label_and => {
            let parts = pair
                .into_inner()
                .filter(|child| child.as_rule() == Rule::label_not)
                .map(|child| build_label_expr(child))
                .collect::<Result<Vec<_>, _>>()?;
            collapse_label_parts(parts, LabelExpr::Conjunction)
        }
        Rule::label_not => {
            let negated = pair.as_str().starts_with('!');
            let child = first_child(pair)?;
            let value = build_label_expr(child)?;
            if negated {
                Ok(LabelExpr::Negation(Box::new(value)))
            } else {
                Ok(value)
            }
        }
        Rule::label_atom => build_label_expr(first_child(pair)?),
        Rule::label_wildcard => Ok(LabelExpr::Wildcard),
        Rule::ident => Ok(LabelExpr::Single(db_string_pair(pair)?)),
        _ => Err(unexpected_pair(pair, "expected label expression")),
    }
}

fn collapse_label_parts(
    mut parts: Vec<LabelExpr>,
    wrap: fn(Vec2OrMore<LabelExpr>) -> LabelExpr,
) -> Result<LabelExpr, ParserError> {
    match parts.len() {
        0 => Err(ParserError::syntax(
            "label expression is empty",
            SourceSpan::default(),
            None,
        )),
        1 => Ok(parts.remove(0)),
        _ => Ok(wrap(
            Vec2OrMore::try_from_vec(parts).expect("collapse_label_parts only wraps at len >= 2"),
        )),
    }
}

fn build_quantifier(pair: Pair<'_, Rule>) -> Result<Quantifier, ParserError> {
    let source_span = span(&pair);
    let text = pair.as_str();

    if text == "*" {
        return Ok(Quantifier::GraphPattern { min: 0, max: None });
    }
    if text == "+" {
        return Ok(Quantifier::GraphPattern { min: 1, max: None });
    }
    if text == "?" {
        return Ok(Quantifier::Questioned);
    }

    if let Some(range) = text.strip_prefix('*') {
        let (min_text, max_text) = range.split_once("..").ok_or_else(|| {
            ParserError::syntax("invalid variable-length quantifier", source_span, None)
        })?;
        let min = parse_u32(min_text, source_span)?;
        let max = if max_text.is_empty() {
            None
        } else {
            Some(parse_u32(max_text, source_span)?)
        };
        return validate_range(min, max, source_span);
    }

    let body = text
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
        .ok_or_else(|| ParserError::syntax("invalid quantifier", source_span, None))?;
    if let Some((min_text, max_text)) = body.split_once(',') {
        let min = if min_text.is_empty() {
            0
        } else {
            parse_u32(min_text, source_span)?
        };
        let max = if max_text.is_empty() {
            None
        } else {
            Some(parse_u32(max_text, source_span)?)
        };
        return validate_range(min, max, source_span);
    }

    let exact = parse_u32(body, source_span)?;
    Ok(Quantifier::GraphPattern {
        min: exact,
        max: Some(exact),
    })
}

fn validate_range(
    min: u32,
    max: Option<u32>,
    source_span: SourceSpan,
) -> Result<Quantifier, ParserError> {
    if let Some(max_value) = max
        && max_value < min
    {
        // Reject `*5..2`, `{5,2}`, etc. before they reach planning, where
        // an impossible bound would either be silently empty or violate
        // the planner's monotonicity assumptions.
        return Err(ParserError::syntax(
            "quantifier range has max below min",
            source_span,
            Some("ensure max >= min in `*min..max` or `{min,max}`".into()),
        ));
    }
    Ok(Quantifier::GraphPattern { min, max })
}

fn parse_u32(text: &str, source_span: SourceSpan) -> Result<u32, ParserError> {
    text.parse::<u32>().map_err(|error| {
        ParserError::syntax(
            format!("invalid quantifier bound: {error}"),
            source_span,
            None,
        )
    })
}

pub(super) fn build_property_map(
    pair: Pair<'_, Rule>,
) -> Result<Vec<(selene_core::DbString, ValueExpr)>, ParserError> {
    pair.into_inner()
        .filter(|child| child.as_rule() == Rule::property_pair)
        .map(|property| {
            let property_span = span(&property);
            let mut children = property.into_inner();
            let key = children
                .next()
                .ok_or_else(|| {
                    ParserError::syntax("property pair is missing key", property_span, None)
                })
                .and_then(|pair| db_string_pair(pair))?;
            let value = children
                .next()
                .ok_or_else(|| {
                    ParserError::syntax("property pair is missing value", property_span, None)
                })
                .and_then(|pair| expr::build_value_expr(pair))?;
            Ok((key, value))
        })
        .collect()
}

fn expr_from_child(pair: Pair<'_, Rule>) -> Result<ValueExpr, ParserError> {
    let source_span = span(&pair);
    pair.into_inner()
        .find(|child| child.as_rule() == Rule::expr)
        .ok_or_else(|| ParserError::syntax("clause is missing expression", source_span, None))
        .and_then(|pair| expr::build_value_expr(pair))
}
