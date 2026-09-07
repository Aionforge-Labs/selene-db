//! Indexed reads must return exactly the rows an unindexed read returns.
//!
//! GQL equality is cross-variant: `3` equals `3.0`. A typed property index keys
//! one variant, so on an open graph a column can hold rows the index cannot
//! key. Those rows are absent from the index but present to a scan, and the
//! optimizer picks between the two on the literal's type — so the same
//! predicate written two ways could disagree on identical data.
//!
//! An index is supposed to make a query faster, never different. These tests
//! pin that for variant drift and for signed zero, where every row is keyable
//! and the divergence came instead from float key construction.

use selene_core::{DbString, GraphId, Value, db_string as core_db_string};
use selene_gql::{EmptyProcedureRegistry, Session, StatementOutput};
use selene_graph::{SharedGraph, TypedIndexKind};

fn db_string(value: &str) -> DbString {
    core_db_string(value).expect("test string fits DB string cap")
}

fn run(session: &mut Session<'_>, source: &str) -> selene_gql::BindingTable {
    match session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect("statement executes")
    {
        StatementOutput::Rows(table) => table,
        other => panic!("expected rows from {source}, got {other:?}"),
    }
}

fn exec(session: &mut Session<'_>, source: &str) {
    session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect("statement executes");
}

/// Row count for a `RETURN n` style query.
fn count(session: &mut Session<'_>, source: &str) -> usize {
    run(session, source).rows().len()
}

/// An open graph holding one Int and one Float row that are GQL-equal, with an
/// I64 index registered before the Float row lands.
///
/// The index is created while only the Int row exists, because index creation
/// is strict and would reject the mismatched value outright. That ordering is
/// exactly how a real deployment arrives here: index a clean column, then write
/// a value of another numeric type.
fn drifted_graph(id: u64) -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(id));
    {
        let mut session = Session::new(&graph);
        exec(&mut session, "INSERT (:Reading { level: 3 })");
    }
    graph
        .create_property_index(
            db_string("Reading"),
            db_string("level"),
            TypedIndexKind::I64,
        )
        .expect("index builds over the clean column");
    {
        let mut session = Session::new(&graph);
        exec(&mut session, "INSERT (:Reading { level: 3.0e0 })");
    }
    graph
}

#[test]
fn integer_and_float_spellings_of_one_predicate_agree() {
    let graph = drifted_graph(90_101);
    let mut session = Session::new(&graph);

    let via_int_literal = count(&mut session, "MATCH (n:Reading) WHERE n.level = 3 RETURN n");
    let via_float_literal = count(
        &mut session,
        "MATCH (n:Reading) WHERE n.level = 3.0e0 RETURN n",
    );

    assert_eq!(
        via_int_literal, via_float_literal,
        "the same predicate written two ways must return the same rows; the \
         optimizer picks an index scan for one spelling and a linear scan for \
         the other, and an index that omits rows makes them disagree"
    );
    assert_eq!(
        via_int_literal, 2,
        "both stored values are equal to 3 under GQL cross-variant equality"
    );
}

#[test]
fn indexed_label_matches_unindexed_label_on_identical_data() {
    let indexed = drifted_graph(90_102);

    let unindexed = SharedGraph::new(GraphId::new(90_103));
    {
        let mut session = Session::new(&unindexed);
        exec(&mut session, "INSERT (:Reading { level: 3 })");
        exec(&mut session, "INSERT (:Reading { level: 3.0e0 })");
    }

    let query = "MATCH (n:Reading) WHERE n.level = 3 RETURN n";
    let mut indexed_session = Session::new(&indexed);
    let mut unindexed_session = Session::new(&unindexed);

    assert_eq!(
        count(&mut indexed_session, query),
        count(&mut unindexed_session, query),
        "registering an index must not change which rows a query returns"
    );
}

#[test]
fn range_predicates_agree_across_the_same_drift() {
    let graph = drifted_graph(90_104);
    let mut session = Session::new(&graph);

    assert_eq!(
        count(&mut session, "MATCH (n:Reading) WHERE n.level > 2 RETURN n"),
        2,
        "a range predicate must also see the row the index cannot key"
    );
}

#[test]
fn a_clean_index_still_answers_and_stays_exact() {
    let graph = SharedGraph::new(GraphId::new(90_105));
    {
        let mut session = Session::new(&graph);
        exec(&mut session, "INSERT (:Reading { level: 3 })");
        exec(&mut session, "INSERT (:Reading { level: 4 })");
    }
    graph
        .create_property_index(
            db_string("Reading"),
            db_string("level"),
            TypedIndexKind::I64,
        )
        .expect("index builds");

    let snapshot = graph.read();
    let reading = db_string("Reading");
    let level = db_string("level");
    assert!(
        snapshot
            .node_candidates_with_property_eq(&reading, &level, &Value::Int(3))
            .unwrap()
            .is_some(),
        "an index covering every row must keep answering probes"
    );

    let mut session = Session::new(&graph);
    assert_eq!(
        count(&mut session, "MATCH (n:Reading) WHERE n.level = 3 RETURN n"),
        1
    );
}

#[test]
fn repairing_the_drifted_row_re_enables_the_index() {
    let graph = drifted_graph(90_106);
    let reading = db_string("Reading");
    let level = db_string("level");

    assert!(
        graph
            .read()
            .node_candidates_with_property_eq(&reading, &level, &Value::Int(3))
            .unwrap()
            .is_none(),
        "while a row is unkeyable the index declines so callers scan"
    );

    // Both stored values are GQL-equal, so the unkeyable one cannot be singled
    // out by predicate. Clear the label and rebuild it cleanly instead, which
    // is what an operator repairing the column would do.
    {
        let mut session = Session::new(&graph);
        exec(&mut session, "MATCH (n:Reading) DETACH DELETE n");
    }
    {
        let mut session = Session::new(&graph);
        exec(&mut session, "INSERT (:Reading { level: 7 })");
    }
    assert!(
        graph
            .read()
            .node_candidates_with_property_eq(&reading, &level, &Value::Int(7))
            .unwrap()
            .is_some(),
        "once every unkeyable row is gone the index answers again"
    );
}

/// A graph whose indexed column drifted to a value of *another comparability
/// family*: an `I64` index over a column that later took a `STRING`.
///
/// The suite above drifts within the numeric family, where the omitted row can
/// genuinely be matched by an index-keyed predicate. This is the other case —
/// ISO/IEC 39075:2024 §4.16.5.2 makes numbers the only family that compares
/// across variants, so no `I64`-keyed *equality* probe could ever have matched
/// this row.
fn cross_family_drifted_graph(id: u64, indexed: bool) -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(id));
    {
        let mut session = Session::new(&graph);
        exec(&mut session, "INSERT (:Reading { level: 3 })");
    }
    if indexed {
        graph
            .create_property_index(
                db_string("Reading"),
                db_string("level"),
                TypedIndexKind::I64,
            )
            .expect("index builds over the clean column");
    }
    {
        let mut session = Session::new(&graph);
        exec(&mut session, "INSERT (:Reading { level: 'high' })");
    }
    graph
}

/// The GQLSTATUS a statement fails with.
fn status(graph: &SharedGraph, source: &str) -> String {
    Session::new(graph)
        .execute_source(source, &EmptyProcedureRegistry)
        .expect_err("statement errors")
        .gqlstatus()
        .as_str()
        .to_owned()
}

#[test]
fn equality_over_cross_family_drift_agrees_with_a_scan() {
    let indexed = cross_family_drifted_graph(90_120, true);
    let unindexed = cross_family_drifted_graph(90_121, false);

    let query = "MATCH (n:Reading) WHERE n.level = 3 RETURN n";
    let indexed_rows = count(&mut Session::new(&indexed), query);

    assert_eq!(indexed_rows, count(&mut Session::new(&unindexed), query));
    assert_eq!(
        indexed_rows, 1,
        "`'high' = 3` is a definite false, so only the Int row matches"
    );
}

/// The reason index-drift classification stays an over-approximation instead of
/// narrowing to the numeric family.
///
/// Equality above shows the tempting half: the drifted row cannot be matched by
/// an `I64`-keyed probe, so letting the index keep answering would return the
/// same rows. Ordering is the half that forbids it. A non-comparable pair is a
/// `22G04` data exception, not a false, so the drifted row is not merely
/// invisible to a range predicate — it makes the whole statement fail. An index
/// that survived the drift would let `range_index_scan` fire, and its declined
/// range probe falls back to `linear_rows_filtered_by_resolved_bounds`, which
/// treats an incomparable row as a plain non-match. The statement would stop
/// raising and start succeeding.
///
/// See `selene_graph::property_index`'s `counts_as_drift` for the full finding.
#[test]
fn a_range_predicate_over_cross_family_drift_still_raises() {
    let indexed = cross_family_drifted_graph(90_122, true);
    let unindexed = cross_family_drifted_graph(90_123, false);

    let query = "MATCH (n:Reading) WHERE n.level > 2 RETURN n";

    assert_eq!(
        status(&indexed, query),
        "22G04",
        "an index must not turn a values-not-comparable failure into rows"
    );
    assert_eq!(
        status(&unindexed, query),
        "22G04",
        "the scan this must stay identical to raises the same status"
    );
}

/// The prefix probe's version of the same carve-out, held latent.
///
/// `STARTS WITH` against a non-string is a `22G03` data exception, so a `STRING`
/// index carrying one `Int` row would flip the same way as the range case above
/// if it kept answering. It cannot today: no optimizer rule routes `STARTS
/// WITH` to `TypedIndex::lookup_prefix`, so the predicate always reaches the
/// evaluator and always raises whatever the drift classifier decides. Narrowing
/// `counts_as_drift` leaves this test passing for that reason alone — it pins
/// the status a future prefix-scan rule has to preserve, not the classifier.
#[test]
fn starts_with_over_cross_family_drift_still_raises() {
    let build = |id: u64, indexed: bool| {
        let graph = SharedGraph::new(GraphId::new(id));
        {
            let mut session = Session::new(&graph);
            exec(&mut session, "INSERT (:Doc { name: 'alpha' })");
        }
        if indexed {
            graph
                .create_property_index(db_string("Doc"), db_string("name"), TypedIndexKind::String)
                .expect("index builds over the clean column");
        }
        {
            let mut session = Session::new(&graph);
            exec(&mut session, "INSERT (:Doc { name: 5 })");
        }
        graph
    };

    let query = "MATCH (n:Doc) WHERE n.name STARTS WITH 'a' RETURN n";
    assert_eq!(status(&build(90_124, true), query), "22G03");
    assert_eq!(status(&build(90_125, false), query), "22G03");
}

/// A graph holding one `-0.0` row and one `0.0` row, with an F64 index over the
/// column. Both rows are keyable, so this is not a completeness problem: the
/// index answers, and the question is only whether it answers the same rows a
/// scan would.
fn signed_zero_graph(id: u64, indexed: bool) -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(id));
    {
        let mut session = Session::new(&graph);
        exec(&mut session, "INSERT (:Reading { level: -0.0e0 })");
        exec(&mut session, "INSERT (:Reading { level: 0.0e0 })");
    }
    if indexed {
        graph
            .create_property_index(
                db_string("Reading"),
                db_string("level"),
                TypedIndexKind::F64,
            )
            .expect("index builds; neither signed zero is unkeyable");
    }
    graph
}

#[test]
fn signed_zero_equality_agrees_between_index_and_scan() {
    let indexed = signed_zero_graph(90_110, true);
    let unindexed = signed_zero_graph(90_111, false);

    for predicate in ["= 0.0e0", "= -0.0e0"] {
        let query = format!("MATCH (n:Reading) WHERE n.level {predicate} RETURN n");
        let indexed_rows = count(&mut Session::new(&indexed), &query);

        assert_eq!(
            indexed_rows,
            count(&mut Session::new(&unindexed), &query),
            "registering a float index must not change which rows `{predicate}` returns"
        );
        assert_eq!(
            indexed_rows, 2,
            "-0.0 and 0.0 are equal under GQL, so `{predicate}` matches both rows"
        );
    }
}

#[test]
fn signed_zero_ranges_agree_between_index_and_scan() {
    let indexed = signed_zero_graph(90_112, true);
    let unindexed = signed_zero_graph(90_113, false);

    // `0.0 > -0.0` is false under GQL, so a bound written with either sign
    // excludes both rows. Ordering the keys by total_cmp would have let the
    // index return the `0.0` row here.
    for (predicate, expected) in [
        ("> -0.0e0", 0),
        ("> 0.0e0", 0),
        (">= -0.0e0", 2),
        (">= 0.0e0", 2),
        ("< 0.0e0", 0),
        ("<= -0.0e0", 2),
    ] {
        let query = format!("MATCH (n:Reading) WHERE n.level {predicate} RETURN n");
        let indexed_rows = count(&mut Session::new(&indexed), &query);

        assert_eq!(
            indexed_rows,
            count(&mut Session::new(&unindexed), &query),
            "index and scan must agree on `{predicate}`"
        );
        assert_eq!(
            indexed_rows, expected,
            "`{predicate}` selects {expected} rows"
        );
    }
}

#[test]
fn updating_between_signed_zeros_leaves_the_index_answering() {
    let graph = signed_zero_graph(90_114, true);

    {
        let mut session = Session::new(&graph);
        exec(
            &mut session,
            "MATCH (n:Reading) WHERE n.level = 0.0e0 SET n.level = -0.0e0",
        );
    }

    let reading = db_string("Reading");
    let level = db_string("level");
    assert!(
        graph
            .read()
            .node_candidates_with_property_eq(&reading, &level, &Value::Float(0.0))
            .unwrap()
            .is_some(),
        "an update that only flips the sign of zero must not demote the index"
    );
    assert_eq!(
        count(
            &mut Session::new(&graph),
            "MATCH (n:Reading) WHERE n.level = 0.0e0 RETURN n"
        ),
        2,
        "both rows still key to the same zero after the update"
    );
}
