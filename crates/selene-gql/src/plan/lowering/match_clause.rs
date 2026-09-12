//! MATCH-clause assembly. Path semantics come only from the logical automata.

use super::{
    PathLowering,
    bindings::{HiddenAllocator, binding_defs},
    expr,
    optional_filters::split_optional_filters,
    path_program,
};
use crate::{
    GraphPattern, MatchClause, MatchMode, PathMode, PatternElement,
    analyze::{AnalyzedStatement, BindingId},
    plan::{BuildSide, FilterPredicate, JoinTree, PatternPlan, PlannerError},
};
use selene_core::DbString;
use std::collections::BTreeSet;

struct LoweredClause {
    tree: JoinTree,
    names: BTreeSet<DbString>,
    filters: Vec<FilterPredicate>,
}

/// Assemble the leading clauses without changing their join multiplicity.
pub(crate) fn lower_match_prefix(
    clauses: &[&MatchClause],
    analyzed: &AnalyzedStatement,
    context: PathLowering<'_>,
) -> Result<Option<PatternPlan>, PlannerError> {
    let mut filters = Vec::new();
    let mut binding_ids = BTreeSet::new();
    let mut hidden = HiddenAllocator::default();
    let mut current: Option<(JoinTree, BTreeSet<DbString>)> = None;
    for clause in clauses {
        let lowered = lower_match_clause(clause, analyzed, &mut binding_ids, &mut hidden, context)?;
        current = Some(match (current, clause.optional) {
            (None, false) => {
                filters.extend(lowered.filters);
                (lowered.tree, lowered.names)
            }
            (None, true) => (
                JoinTree::Outer {
                    left: Box::new(JoinTree::Unit),
                    right: Box::new(lowered.tree),
                    key: Vec::new(),
                    right_filters: lowered.filters,
                },
                lowered.names,
            ),
            (Some((left, left_names)), optional) => {
                let key = shared_names(&left_names, &lowered.names);
                let (right_filters, global_filters) = if optional {
                    split_optional_filters(lowered.filters, &left_names, analyzed)
                } else {
                    (Vec::new(), lowered.filters)
                };
                filters.extend(global_filters);
                let mut names = left_names;
                names.extend(lowered.names);
                let tree = if optional {
                    JoinTree::Outer {
                        left: Box::new(left),
                        right: Box::new(lowered.tree),
                        key,
                        right_filters,
                    }
                } else {
                    JoinTree::HashJoin {
                        left: Box::new(left),
                        right: Box::new(lowered.tree),
                        key,
                        build_side: BuildSide::Left,
                    }
                };
                (tree, names)
            }
        });
    }
    Ok(current.map(|(join_tree, _)| PatternPlan {
        bindings: binding_defs(analyzed, &binding_ids),
        join_tree,
        filters,
    }))
}

pub(super) fn lower_pipeline_match(
    clause: &MatchClause,
    analyzed: &AnalyzedStatement,
    left_names: &BTreeSet<DbString>,
    context: PathLowering<'_>,
) -> Result<(PatternPlan, Vec<FilterPredicate>), PlannerError> {
    let mut ids = BTreeSet::new();
    let lowered = lower_match_clause(
        clause,
        analyzed,
        &mut ids,
        &mut HiddenAllocator::default(),
        context,
    )?;
    let (filters, global) = if clause.optional {
        split_optional_filters(lowered.filters, left_names, analyzed)
    } else {
        (lowered.filters, Vec::new())
    };
    Ok((
        PatternPlan {
            bindings: binding_defs(analyzed, &ids),
            join_tree: lowered.tree,
            filters,
        },
        global,
    ))
}

fn lower_match_clause(
    clause: &MatchClause,
    analyzed: &AnalyzedStatement,
    ids: &mut BTreeSet<BindingId>,
    hidden: &mut HiddenAllocator,
    context: PathLowering<'_>,
) -> Result<LoweredClause, PlannerError> {
    let mut filters = Vec::new();
    // Primitive scans/expands retain optimizer access paths. Every path family
    // (including fixed-length restricted or named paths) uses one complete
    // clause program, so graph match mode and local selection never split apart.
    let (tree, names) = if needs_path_program(clause) {
        let program = path_program::lower(clause, analyzed, context)?;
        ids.extend(&program.bindings);
        let names = program
            .schema
            .columns
            .iter()
            .filter_map(|c| c.name.clone())
            .collect();
        (JoinTree::Paths(Box::new(program)), names)
    } else {
        let mut current: Option<(JoinTree, BTreeSet<DbString>)> = None;
        for pattern in &clause.patterns {
            let (tree, names) = lower_primitive(pattern, analyzed, &mut filters, ids, hidden)?;
            current = Some(match current {
                None => (tree, names),
                Some((left, mut left_names)) => {
                    let key = shared_names(&left_names, &names);
                    left_names.extend(names);
                    (
                        JoinTree::HashJoin {
                            left: Box::new(left),
                            right: Box::new(tree),
                            key,
                            build_side: BuildSide::Left,
                        },
                        left_names,
                    )
                }
            });
        }
        current.ok_or(PlannerError::NotImplemented {
            feature: "empty graph pattern",
            span: clause.span,
        })?
    };
    if let Some(condition) = &clause.where_clause {
        filters.push(expr::filter_predicate(condition, analyzed)?);
    }
    Ok(LoweredClause {
        tree,
        names,
        filters,
    })
}

fn needs_path_program(clause: &MatchClause) -> bool {
    // A positive selector over a node-only pattern has exactly one zero-hop
    // candidate per endpoint pair. It is an identity operation; retain the
    // existing indexed scan instead of discarding its access-path authority.
    clause.path_mode != PathMode::Walk
        || (clause.selector.is_some() && clause.patterns.iter().any(|p| p.elements.len() > 1))
        || clause.match_mode == Some(MatchMode::DifferentEdges)
        || clause.patterns.iter().any(|p| {
            p.path_binding.is_some()
                || p.elements
                    .iter()
                    .any(|e| matches!(e, PatternElement::Edge(edge) if edge.quantifier.is_some()))
        })
}

fn lower_primitive(
    pattern: &GraphPattern,
    analyzed: &AnalyzedStatement,
    filters: &mut Vec<FilterPredicate>,
    ids: &mut BTreeSet<BindingId>,
    hidden: &mut HiddenAllocator,
) -> Result<(JoinTree, BTreeSet<DbString>), PlannerError> {
    use super::bindings::{edge_binding, node_binding};
    use crate::{EdgeMatch, NodeOrEdgeScan, ScanAccess, ScanKind};
    let mut names = BTreeSet::new();
    let Some(PatternElement::Node(first)) = pattern.elements.first() else {
        return Err(PlannerError::NotImplemented {
            feature: "empty graph pattern",
            span: pattern.span,
        });
    };
    let mut left = node_binding(first, analyzed, &mut names, ids)?;
    let mut left_hidden = left.is_none().then(|| hidden.next());
    let properties = first
        .properties
        .iter()
        .map(|(key, value)| expr::property_predicate(left, key.clone(), value, analyzed))
        .collect::<Result<_, _>>()?;
    if let Some(condition) = &first.inline_where {
        filters.push(expr::filter_predicate(condition, analyzed)?);
    }
    let mut tree = JoinTree::Scan(NodeOrEdgeScan {
        binding: left,
        hidden_binding: left_hidden,
        kind: ScanKind::Node,
        label_predicate: first.label_expr.clone(),
        property_predicates: properties,
        access: ScanAccess::Linear,
        span: first.span,
    });
    for pair in pattern.elements[1..].chunks_exact(2) {
        let [PatternElement::Edge(edge), PatternElement::Node(node)] = pair else {
            return Err(PlannerError::NotImplemented {
                feature: "non-alternating graph pattern",
                span: pattern.span,
            });
        };
        let right = node_binding(node, analyzed, &mut names, ids)?;
        let right_hidden = right.is_none().then(|| hidden.next());
        let binding = edge_binding(edge, analyzed, &mut names, ids)?;
        let edge_hidden = binding.is_none().then(|| hidden.next());
        let properties = edge
            .properties
            .iter()
            .map(|(key, value)| expr::property_predicate(binding, key.clone(), value, analyzed))
            .collect::<Result<_, _>>()?;
        let right_properties = node
            .properties
            .iter()
            .map(|(key, value)| expr::property_predicate(right, key.clone(), value, analyzed))
            .collect::<Result<_, _>>()?;
        for condition in node.inline_where.iter().chain(edge.inline_where.iter()) {
            filters.push(expr::filter_predicate(condition, analyzed)?);
        }
        tree = JoinTree::Expand {
            child: Box::new(tree),
            direction: edge.direction,
            edge: EdgeMatch {
                binding,
                hidden_binding: edge_hidden,
                label_predicate: edge.label_expr.clone(),
                property_predicates: properties,
                left_binding: left,
                left_hidden_binding: left_hidden,
                right_binding: right,
                right_hidden_binding: right_hidden,
                right_label_predicate: node.label_expr.clone(),
                right_property_predicates: right_properties,
                access: ScanAccess::Linear,
                span: edge.span,
            },
        };
        left = right;
        left_hidden = right_hidden;
    }
    Ok((tree, names))
}

fn shared_names(left: &BTreeSet<DbString>, right: &BTreeSet<DbString>) -> Vec<DbString> {
    left.intersection(right).cloned().collect()
}
