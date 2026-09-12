//! Predicate failures, complete ties, observability privacy and resource release.

use super::{
    oracle::Fixture,
    tests::{analyzed, run},
    *,
};
use crate::lower_path_automata_with_defaults;
use selene_core::{CancellationToken, Value, db_string};
use std::cell::Cell;

#[test]
fn property_conditions_apply_to_every_hop_and_null_is_not_true() {
    let f = Fixture::new(
        3,
        &[(0, 2, true, true), (0, 1, true, true), (1, 2, true, true)],
    );
    let mut tx = f.graph.begin_write();
    for edge in &f.edges[1..] {
        tx.mutator()
            .update_edge(
                edge.0,
                selene_core::PropertyDiff::new([(db_string("ok").unwrap(), Value::Bool(true))], [])
                    .unwrap(),
            )
            .unwrap();
    }
    tx.commit().unwrap();
    let result = run(
        &f.graph,
        "MATCH ALL SHORTEST p = (a:Root)-[r{1,2} {ok: TRUE}]->(b) RETURN p",
        Default::default(),
    )
    .unwrap();
    let lengths: Vec<_> = result
        .table
        .rows()
        .iter()
        .map(|r| match &r.values()[0] {
            Value::Path(p) => p.segments.len(),
            _ => panic!("path"),
        })
        .collect();
    assert_eq!(lengths, [1, 2]);
    assert_eq!(
        run(
            &f.graph,
            "MATCH ALL SHORTEST (a:Root)-[r{1,2} WHERE a.absent = 1]->(b) RETURN r",
            Default::default()
        )
        .unwrap()
        .table
        .row_count(),
        0
    );
    assert!(
        run(
            &f.graph,
            "MATCH ALL SHORTEST (a:Root)-[r{1,2} WHERE 1 / 0 = 1]->(b) RETURN r",
            Default::default()
        )
        .is_err()
    );
}

#[test]
fn cancellation_during_tie_qualification_discards_rows_and_releases_batch_budget() {
    let f = Fixture::new(2, &[(0, 1, true, true); 32]);
    let a = analyzed("MATCH ALL SHORTEST p = (a:Root)-[r]->(b) RETURN p");
    let set = lower_path_automata_with_defaults(&a).unwrap();
    let program = BoundedPathProgram::compile(&set.automata, &a).unwrap();
    let token = CancellationToken::new();
    let calls = Cell::new(0);
    let qualify = |_: &state::SearchState, _: &Value, _: conditions::Phase| {
        calls.set(calls.get() + 1);
        if calls.get() == 12 {
            token.cancel();
        }
        Ok(true)
    };
    let snapshot = f.graph.read();
    let mut ctx = BatchExecutionContext::borrowed(
        &snapshot,
        super::super::batch::budget::BatchCancel::new(Some(&token), None, None),
        MemoryBudget::unlimited(),
    );
    let mut source =
        ProductPathOperator::new(&program, Default::default(), BatchPolicy::default_policy());
    source.qualify = Some(&qualify);
    let error = trace_operator_to_table(&mut source, &mut ctx).unwrap_err();
    assert_eq!(error.gqlstatus().as_str(), "5GQL2");
    assert!(calls.get() >= 12);
    assert_eq!(ctx.budget_used(), 0);
    assert!(ctx.is_closed());
}

#[test]
fn complete_unbounded_ties_and_groups_include_the_entire_boundary_layer() {
    let f = Fixture::new(1, &[(0, 0, true, true); 3]);
    for (prefix, count) in [
        ("ALL SHORTEST", 3),
        ("SHORTEST 2", 2),
        ("SHORTEST 2 GROUPS", 12),
    ] {
        let result = run(
            &f.graph,
            &format!("MATCH {prefix} p = (a)-[r+]->(b) RETURN p"),
            PathExecutionLimits {
                max_hops: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.table.row_count(), count);
    }
    let result = run(
        &f.graph,
        "MATCH ALL SHORTEST p = (a)-[r+ WHERE size(r) = 2]->(b) RETURN p",
        PathExecutionLimits {
            max_hops: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.table.row_count(), 9);
}

#[test]
fn adjacent_quantifiers_keep_duplicate_bindings_for_the_same_selected_path() {
    let f = Fixture::new(2, &[(0, 1, true, true)]);
    let result = run(
        &f.graph,
        "MATCH ALL SHORTEST p = (a:Root)-[r{0,1}]->(m)-[s{0,1}]->(b:N) RETURN p",
        Default::default(),
    )
    .unwrap();
    assert_eq!(result.table.row_count(), 2);
    assert_eq!(
        result.table.rows()[0].values()[0],
        result.table.rows()[1].values()[0]
    );
    assert_ne!(result.table.rows()[0], result.table.rows()[1]);
}

#[test]
fn observation_debug_redacts_payloads() {
    let f = Fixture::new(1, &[(0, 0, true, true)]);
    let mut result = run(
        &f.graph,
        "MATCH (a)-[r]->(b) RETURN r",
        PathExecutionLimits {
            observe: true,
            ..Default::default()
        },
    )
    .unwrap();
    result.observations[0].locals[0].1 = Value::String(db_string("private-query-payload").unwrap());
    result.observations[0].temporaries.push((
        0,
        0,
        Value::String(db_string("private-temporary-payload").unwrap()),
    ));
    let debug = format!("{:?}", result.observations);
    assert!(!debug.contains("private-"));
    assert!(debug.contains("local_count"));
}
