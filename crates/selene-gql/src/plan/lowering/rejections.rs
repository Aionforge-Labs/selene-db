//! PLAN-17 / PLAN-22 / PLAN-23: planner `NotImplemented` /
//! `WriteSetPatternMismatch` / `ProcedureMetadataMismatch` rejection-site
//! coverage.
//!
//! Most of the planner's defensive rejection sites are unreachable from real
//! GQL — the flagger (parse-time) or analyzer rejects the construct first
//! (e.g. variable-scope CALL flags GP03 at parse, `IN TRANSACTIONS` no longer
//! parses, empty / non-alternating / edge-without-target patterns are syntax
//! errors). The `feature` tag on `PlannerError::NotImplemented` is
//! documented "asserted by tests", but only a couple of tags were exact-match
//! pinned. To exercise the planner branches themselves, these tests parse a
//! valid base statement, analyze it, then surgically mutate the analyzed AST
//! into the rejected shape and drive the planner directly — making each
//! defensive guard live and pinning its exact tag.

use crate::{
    AnalyzedStatement, EmptyProcedureRegistry, JoinTree, MatchClause, MatchMode, MutationStatement,
    PatternElement, PipelineStatement, PlannerError, Statement, WriteKind, YieldColumn, YieldItem,
    analyze, parse, plan,
};
use selene_core::db_string;

fn analyzed(source: &str) -> AnalyzedStatement {
    let statement = parse(source).expect("base source parses");
    analyze(statement, &EmptyProcedureRegistry, None).expect("base source analyzes")
}

fn plan_err(analyzed: &AnalyzedStatement) -> PlannerError {
    plan(analyzed, &EmptyProcedureRegistry).expect_err("mutated AST must fail planning")
}

/// Apply `mutate` to the leading MATCH clause's pattern elements of a Query.
fn mutate_match_pattern(
    analyzed: &mut AnalyzedStatement,
    mutate: impl FnOnce(&mut Vec<PatternElement>),
) {
    analyzed.corrupt_for_test(|source, _| {
        let Statement::Query(query) = source else {
            panic!("expected query statement");
        };
        for statement in &mut query.statements {
            if let PipelineStatement::Match(clause) = statement {
                mutate(&mut clause.patterns[0].elements);
                return;
            }
        }
        panic!("no MATCH clause to mutate");
    });
}

/// Apply `mutate` to the leading MATCH clause of a Query.
fn mutate_match_clause(analyzed: &mut AnalyzedStatement, mutate: impl FnOnce(&mut MatchClause)) {
    analyzed.corrupt_for_test(|source, _| {
        let Statement::Query(query) = source else {
            panic!("expected query statement");
        };
        for statement in &mut query.statements {
            if let PipelineStatement::Match(clause) = statement {
                mutate(clause);
                return;
            }
        }
        panic!("no MATCH clause to mutate");
    });
}

/// Apply `mutate` to the leading inline `CALL { ... }` subquery of a Query.
fn mutate_call_subquery(
    analyzed: &mut AnalyzedStatement,
    mutate: impl FnOnce(&mut crate::InlineProcedureCall),
) {
    analyzed.corrupt_for_test(|source, _| {
        let Statement::Query(query) = source else {
            panic!("expected query statement");
        };
        for statement in &mut query.statements {
            if let PipelineStatement::CallSubquery(call) = statement {
                mutate(call);
                return;
            }
        }
        panic!("no CALL subquery to mutate");
    });
}

fn assert_not_implemented(error: PlannerError, expected_feature: &str) {
    let PlannerError::NotImplemented { feature, .. } = error else {
        panic!("expected NotImplemented(\"{expected_feature}\"), got {error:?}");
    };
    assert_eq!(feature, expected_feature);
}

// ---------------------------------------------------------------------------
// PLAN-17: pattern-lowering defensive guards.
// ---------------------------------------------------------------------------

#[test]
fn empty_graph_pattern_is_rejected() {
    let mut analyzed = analyzed("MATCH (n) RETURN n");
    mutate_match_pattern(&mut analyzed, Vec::clear);
    assert_not_implemented(plan_err(&analyzed), "empty graph pattern");
}

#[test]
fn non_alternating_graph_pattern_is_rejected() {
    let mut analyzed = analyzed("MATCH (a)-[:E]->(b) RETURN a");
    // Make element[1] a Node (a copy of the leading node) so the walk hits a
    // node where an edge is required.
    mutate_match_pattern(&mut analyzed, |elements| {
        let node = elements[0].clone();
        elements[1] = node;
    });
    assert_not_implemented(plan_err(&analyzed), "non-alternating graph pattern");
}

#[test]
fn edge_without_target_is_rejected() {
    let mut analyzed = analyzed("MATCH (a)-[:E]->(b) RETURN a");
    // Drop the trailing target node so the pattern ends on an edge.
    mutate_match_pattern(&mut analyzed, |elements| {
        elements.pop();
    });
    assert_not_implemented(plan_err(&analyzed), "edge without target");
}

#[test]
fn different_edges_match_mode_lowers_to_filter() {
    // 812: G002 (DIFFERENT EDGES) is runtime-supported (ISO §16.4 GR8(a)). The
    // `reject_unsupported_clause` backstop now lets it pass; lowering installs
    // the pattern-wide `MatchModeFilter` wrapper. This pins that the (formerly
    // rejecting) planner path now lowers the mode instead of erroring.
    let mut analyzed = analyzed("MATCH (a)-[:E]->(b) RETURN a");
    mutate_match_clause(&mut analyzed, |clause| {
        clause.match_mode = Some(MatchMode::DifferentEdges);
    });
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("DIFFERENT EDGES lowers");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    assert!(matches!(
        pattern.join_tree,
        JoinTree::MatchModeFilter { .. }
    ));
}

#[test]
fn repeatable_elements_match_mode_lowers_without_filter() {
    // 812: G003 (REPEATABLE ELEMENTS) is runtime-supported (ISO §16.4 GR8(b): BINDINGS =
    // INNER), so lowering installs NO match-mode wrapper. The backstop lets it
    // pass; the tree is a bare Expand identical to the no-prefix default.
    let mut analyzed = analyzed("MATCH (a)-[:E]->(b) RETURN a");
    mutate_match_clause(&mut analyzed, |clause| {
        clause.match_mode = Some(MatchMode::RepeatableElements);
    });
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("REPEATABLE ELEMENTS lowers");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    assert!(!matches!(
        pattern.join_tree,
        JoinTree::MatchModeFilter { .. }
    ));
    assert!(matches!(pattern.join_tree, JoinTree::Expand { .. }));
}

// PLAN-17 coverage boundary: the questioned-edge / path-selector / path-mode
// "without … node/edge binding" guards (match_clause.rs::lower_match_clause,
// path_search.rs, path_mode.rs) are NOT reachable by black-box AST mutation.
// `node_scan` / `edge_match` always allocate a hidden binding when no named
// binding exists (`binding.is_none().then(|| hidden.next())`), so
// `chain_tail_binding` over a lowered scan always returns `Some`. Triggering
// those `None` arms requires hand-constructing the internal `JoinTree`, which
// is not a public API. They remain pure defense-in-depth; this file pins every
// tag that a parsed-then-analyzed AST can actually reach (empty /
// non-alternating / edge-without-target / MATCH-mode here; correlated NEXT +
// leading OPTIONAL MATCH are pinned in exec_pipeline_chain.rs +
// plan_read_pipeline.rs). GP03 explicit variable-scope CALL is now SUPPORTED —
// it is bound in the analyzer (the body sees only the named imports), so the
// former planner backstop is gone; its positive + restriction tests live in
// call_subqueries.rs.

// ---------------------------------------------------------------------------
// PLAN-22: insert edge-endpoint resolution uses index.wrapping_sub(1).
// ---------------------------------------------------------------------------

#[test]
fn insert_pattern_starting_with_an_edge_is_a_write_set_pattern_mismatch() {
    // PLAN-22: edge endpoint resolution computes the left endpoint at
    // `index.wrapping_sub(1)`. The grammar forbids an INSERT pattern that starts
    // with an edge, so this defensive path (index 0 -> wrapping_sub(1) ==
    // usize::MAX -> elements.get(MAX) == None -> WriteSetPatternMismatch) has no
    // natural source. Build it by reordering a valid `INSERT (:A)-[:E]->(:B)` so
    // the edge sits at element index 0, keeping the write-set order in step.
    let mut analyzed = analyzed("INSERT (:A)-[:E]->(:B)");

    analyzed.corrupt_for_test(|source, semantic| {
        let Statement::Mutate(pipeline) = source else {
            panic!("expected mutation pipeline");
        };
        for statement in pipeline.statements.iter_mut() {
            if let MutationStatement::Insert(insert) = statement {
                let elements = &mut insert.patterns[0].elements;
                let edge_index = elements
                    .iter()
                    .position(|element| matches!(element, PatternElement::Edge(_)))
                    .expect("pattern has an edge");
                let edge = elements.remove(edge_index);
                elements.insert(0, edge);
            }
        }
        // Reorder the write-set entries so the InsertEdge entry is consumed first,
        // matching the new pattern-element walk order (the edge is now element 0).
        let write_set = semantic.write_set.as_mut().expect("insert has a write set");
        let edge_entry = write_set
            .entries
            .iter()
            .position(|entry| matches!(entry.kind, WriteKind::InsertEdge { .. }))
            .expect("write set has an InsertEdge entry");
        let edge = write_set.entries.remove(edge_entry);
        write_set.entries.insert(0, edge);
    });

    let error = plan_err(&analyzed);
    assert!(
        matches!(error, PlannerError::WriteSetPatternMismatch { .. }),
        "edge-at-index-0 must resolve its missing left endpoint to a \
         WriteSetPatternMismatch, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// PLAN-23: lower_call_subquery yield-resolution metadata mismatch.
// ---------------------------------------------------------------------------

#[test]
fn call_subquery_yield_of_unknown_column_is_a_metadata_mismatch() {
    // PLAN-23: when an inline CALL yields a column absent from its body schema,
    // `body_column` raises ProcedureMetadataMismatch with an EMPTY `procedure`
    // box. The empty box is the intentional inline-call convention (an inline
    // CALL has no named procedure to report, mirroring UnknownYieldColumn). The
    // grammar shape that triggers this can't be reached directly, so mutate the
    // analyzed CALL's yield to reference a non-existent column.
    let mut analyzed = analyzed("CALL { RETURN 1 AS x }");
    mutate_call_subquery(&mut analyzed, |call| {
        let span = call.span;
        call.yield_items = vec![YieldItem {
            column: YieldColumn::Named(
                db_string("missing_column").expect("string fits DB string cap"),
            ),
            alias: None,
            span,
        }];
    });

    let error = plan_err(&analyzed);
    let PlannerError::ProcedureMetadataMismatch {
        procedure, detail, ..
    } = error
    else {
        panic!("expected ProcedureMetadataMismatch, got {error:?}");
    };
    assert!(
        procedure.is_empty(),
        "inline CALL has no named procedure; the box must be empty by convention"
    );
    assert_eq!(
        detail,
        "CALL subquery yield column missing from body output schema"
    );
}
