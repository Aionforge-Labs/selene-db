//! Concrete mode, binding, telemetry, limit and compatibility regressions.

use super::oracle::{Fixture, canonical, list};
use super::*;
use crate::{
    AnalyzedStatement, EmptyProcedureRegistry, ImplDefinedCaps, analyze,
    lower_path_automata_with_defaults, parse,
};
use selene_core::{CancellationToken, NodeScanBudget, Value};
use selene_graph::SharedGraph;

pub(super) fn analyzed(source: &str) -> AnalyzedStatement {
    analyze(parse(source).unwrap(), &EmptyProcedureRegistry, None).unwrap()
}

pub(super) fn run(
    graph: &SharedGraph,
    source: &str,
    limits: PathExecutionLimits,
) -> Result<PathExecution, ExecutorError> {
    let analyzed = analyzed(source);
    let set = lower_path_automata_with_defaults(&analyzed).unwrap();
    let program = BoundedPathProgram::compile(&set.automata, &analyzed)?;
    let caps = ImplDefinedCaps::default();
    let tx = TxContext::read_only(
        graph.read(),
        &caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );
    program.execute(&tx, limits)
}

#[test]
fn simple_is_not_trail_and_closure_is_terminal_across_transitions() {
    let f = Fixture::new(2, &[(0, 1, false, true)]);
    for (mode, count) in [("WALK", 2), ("TRAIL", 0), ("SIMPLE", 2), ("ACYCLIC", 0)] {
        let result = run(
            &f.graph,
            &format!("MATCH {mode} (a)~[r{{2}}]~(a) RETURN r"),
            PathExecutionLimits::default(),
        )
        .unwrap();
        assert_eq!(result.table.row_count(), count, "{mode}");
    }
    for source in [
        "MATCH SIMPLE (a)~[r{2}]~(a)~[s]~(b) RETURN a",
        "MATCH DIFFERENT EDGES SIMPLE (a)~[r{2}]~(a) RETURN a",
    ] {
        assert_eq!(
            run(&f.graph, source, PathExecutionLimits::default())
                .unwrap()
                .table
                .row_count(),
            0
        );
    }
    assert_eq!(
        run(
            &f.graph,
            "MATCH SIMPLE (a)~[r{2}]~(a)~[s{0}]~(a) RETURN a",
            PathExecutionLimits::default()
        )
        .unwrap()
        .table
        .row_count(),
        2
    );
}

#[test]
fn questioned_zero_and_group_zero_have_different_exposures() {
    let f = Fixture::new(1, &[(0, 0, true, true)]);
    let q = run(
        &f.graph,
        "MATCH (a)-[r?]->(b) RETURN a",
        PathExecutionLimits::default(),
    )
    .unwrap();
    let g = run(
        &f.graph,
        "MATCH (a)-[r{0,1}]->(b) RETURN a",
        PathExecutionLimits::default(),
    )
    .unwrap();
    assert_eq!(
        q.table.rows()[0].values(),
        &[
            Value::NodeRef(f.nodes[0]),
            Value::Null,
            Value::NodeRef(f.nodes[0])
        ]
    );
    assert_eq!(
        g.table.rows()[0].values(),
        &[
            Value::NodeRef(f.nodes[0]),
            list(&[]),
            Value::NodeRef(f.nodes[0])
        ]
    );
    assert_eq!(q.table.rows()[1].values()[1], Value::EdgeRef(f.edges[0].0));
    assert_eq!(g.table.rows()[1].values()[1], list(&[f.edges[0].0]));
}

#[test]
fn repeated_variables_restore_history_and_bound_null_is_not_unbound() {
    let f = Fixture::new(1, &[(0, 0, true, true), (0, 0, true, true)]);
    for (source, count) in [
        ("MATCH (a)-[r]->(a)-[r]->(a) RETURN r", 2),
        ("MATCH (a)-[r?]->(a)-[r?]->(a) RETURN r", 3),
        ("MATCH (a)-[r{1,2}]->(a)-[r{1,2}]->(a) RETURN r", 6),
        ("MATCH TRAIL (a)-[r]->(a)-[r]->(a) RETURN r", 0),
    ] {
        assert_eq!(
            run(&f.graph, source, PathExecutionLimits::default())
                .unwrap()
                .table
                .row_count(),
            count,
            "{source}"
        );
    }
}

#[test]
fn match_mode_is_clause_wide_but_path_history_is_automaton_local() {
    let f = Fixture::new(1, &[(0, 0, true, true), (0, 0, true, true)]);
    for (match_mode, count) in [("", 4), ("REPEATABLE ELEMENTS", 4), ("DIFFERENT EDGES", 2)] {
        let source = format!("MATCH {match_mode} TRAIL (a)-[r]->(a), (a)-[s]->(a) RETURN a");
        assert_eq!(
            run(&f.graph, &source, PathExecutionLimits::default())
                .unwrap()
                .table
                .row_count(),
            count,
            "{source}"
        );
    }
    let a = analyzed("MATCH TRAIL (a)-[r]->(b) MATCH ACYCLIC (c)-[s]->(d) RETURN a");
    let set = lower_path_automata_with_defaults(&a).unwrap();
    assert!(matches!(
        BoundedPathProgram::compile(&set.automata, &a),
        Err(ExecutorError::ImplementationDefined { .. })
    ));
    for automaton in &set.automata {
        let program = BoundedPathProgram::compile(std::slice::from_ref(automaton), &a).unwrap();
        let caps = ImplDefinedCaps::default();
        let tx = TxContext::read_only(
            f.graph.read(),
            &caps,
            &EmptyProcedureRegistry,
            f.graph.index_providers(),
        );
        let result = program
            .execute(
                &tx,
                PathExecutionLimits {
                    observe: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(result.observations.iter().all(|o| o.mode == automaton.mode));
    }
}

#[test]
fn temporary_reduction_preserves_multiplicity_without_leaking_columns() {
    let f = Fixture::new(2, &[(0, 1, true, true), (0, 1, true, true)]);
    let result = run(
        &f.graph,
        "MATCH (a)-[{1}]->(b) RETURN a",
        PathExecutionLimits {
            observe: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.table.row_count(), 2);
    assert_eq!(result.table.schema().columns.len(), 2);
    assert_eq!(
        result.table.rows()[0].values(),
        result.table.rows()[1].values()
    );
    assert_ne!(result.observations[0].edge, result.observations[1].edge);
    assert!(
        result
            .table
            .schema()
            .columns
            .iter()
            .all(|c| c.hidden.is_none() && c.name.is_some())
    );
    let anonymous = run(
        &f.graph,
        "MATCH ()-[{1}]->() RETURN 1",
        PathExecutionLimits::default(),
    )
    .unwrap();
    assert_eq!(anonymous.table.row_count(), 2);
    assert!(anonymous.table.schema().columns.is_empty());
}

#[test]
fn hop_observations_include_locals_per_transition_and_cost_every_match() {
    let f = Fixture::new(1, &[(0, 0, true, true)]);
    let source = "MATCH WALK (a)-[r{1,2}]->(m)-[{1,2}]->(b) RETURN a";
    let result = run(
        &f.graph,
        source,
        PathExecutionLimits {
            observe: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.table.row_count(), 4);
    assert_eq!(result.table.schema().columns.len(), 4);
    assert!(
        result
            .table
            .schema()
            .columns
            .iter()
            .all(|c| c.hidden.is_none())
    );
    assert_eq!(result.stats.cheapest_projection.candidate_costs, 4);
    assert_eq!(result.stats.cheapest_projection.edge_cost_evaluations, 12);
    assert!(
        result.stats.hop_lengths[1..=4]
            .iter()
            .all(|count| *count > 0)
    );
    assert!(
        result
            .observations
            .iter()
            .any(|o| !o.temporaries.is_empty())
    );
    let a = analyzed(source);
    let r = a
        .scopes
        .declarations()
        .iter()
        .find(|d| d.name().as_str() == "r")
        .unwrap()
        .id();
    for observation in &result.observations {
        assert_eq!(observation.from, f.nodes[0]);
        assert_eq!(observation.to, f.nodes[0]);
        assert_eq!(observation.edge, f.edges[0].0);
        assert!((1..=2).contains(&observation.repetition));
        let Value::List(edges) = &observation
            .locals
            .iter()
            .find(|(id, _)| *id == r)
            .unwrap()
            .1
        else {
            panic!("r group local")
        };
        assert!((1..=2).contains(&edges.len()));
    }
    let ordinary = run(&f.graph, source, PathExecutionLimits::default()).unwrap();
    assert!(ordinary.observations.is_empty());
    assert_eq!(
        canonical(
            result
                .table
                .rows()
                .iter()
                .map(|r| r.values().to_vec())
                .collect()
        ),
        canonical(
            ordinary
                .table
                .rows()
                .iter()
                .map(|r| r.values().to_vec())
                .collect()
        )
    );
}

#[test]
fn each_execution_limit_fails_explicitly_never_truncates() {
    let f = Fixture::new(1, &[(0, 0, true, true)]);
    let source = "MATCH (a)-[r{0,2}]->(b) RETURN a";
    for (limits, detail) in [
        (
            PathExecutionLimits {
                max_hops: 1,
                ..Default::default()
            },
            "max_path_hops",
        ),
        (
            PathExecutionLimits {
                max_work: 0,
                ..Default::default()
            },
            "max_path_work",
        ),
        (
            PathExecutionLimits {
                max_rows: 1,
                ..Default::default()
            },
            "max_path_rows",
        ),
        (
            PathExecutionLimits {
                max_bytes: 1,
                ..Default::default()
            },
            "max_path_bytes",
        ),
        (
            PathExecutionLimits {
                observe: true,
                max_observations: 1,
                ..Default::default()
            },
            "max_path_observations",
        ),
    ] {
        let error = run(&f.graph, source, limits).unwrap_err();
        assert_eq!(error.gqlstatus().as_str(), "5GQL1");
        assert!(
            matches!(error, ExecutorError::ProgramLimitExceeded { detail: actual, .. } if actual == detail),
            "{error:?}"
        );
    }
    let measured = run(
        &f.graph,
        source,
        PathExecutionLimits {
            observe: true,
            ..Default::default()
        },
    )
    .unwrap();
    let exact = PathExecutionLimits {
        max_hops: 2,
        max_work: measured.stats.product_states + measured.stats.incidences,
        max_rows: 3,
        max_bytes: measured.stats.peak_bytes,
        max_observations: measured.observations.len(),
        observe: true,
    };
    assert_eq!(run(&f.graph, source, exact).unwrap().table.row_count(), 3);
}

#[test]
fn cancellation_deadline_and_scan_budget_share_batch_statuses() {
    let f = Fixture::new(2, &[(0, 1, true, true)]);
    let a = analyzed("MATCH (a)-[r{0,2}]->(b) RETURN a");
    let set = lower_path_automata_with_defaults(&a).unwrap();
    let program = BoundedPathProgram::compile(&set.automata, &a).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let budget = NodeScanBudget::new(0);
    let caps = ImplDefinedCaps::default();
    for (token, deadline, budget, code) in [
        (Some(&token), None, None, "5GQL2"),
        (
            None,
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1)),
            None,
            "5GQL3",
        ),
        (None, None, Some(&budget), "5GQL1"),
    ] {
        let tx = TxContext::read_only(
            f.graph.read(),
            &caps,
            &EmptyProcedureRegistry,
            f.graph.index_providers(),
        )
        .with_resource_limits(token, deadline, None, budget);
        assert_eq!(
            program
                .execute(&tx, PathExecutionLimits::default())
                .unwrap_err()
                .gqlstatus()
                .as_str(),
            code
        );
    }
}

#[test]
fn landed_execution_boundaries_accept_and_malformed_ir_still_rejects() {
    let f = Fixture::new(0, &[]);
    for source in [
        "MATCH TRAIL (a)-[r*]->(b) RETURN a",
        "MATCH ANY (a)-[r{1}]->(b) RETURN a",
        "MATCH ALL (a)-[r{1}]->(b) RETURN a",
        "MATCH p = (a)-[r{1}]->(b) RETURN a",
        "MATCH (a {x: 1})-[r{1}]->(b) RETURN a",
        "MATCH (a WHERE a.x = 1)-[r{1}]->(b) RETURN a",
        "MATCH (a)-[r{1} {x: 1}]->(b) RETURN a",
    ] {
        assert_eq!(
            run(&f.graph, source, PathExecutionLimits::default())
                .unwrap()
                .table
                .row_count(),
            0,
            "{source}"
        );
    }
    let a = analyzed("MATCH (a)-[r{1,2}]->(b) RETURN a");
    let mut set = lower_path_automata_with_defaults(&a).unwrap();
    let crate::PathSemanticElement::Edge(edge) = &mut set.automata[0].semantic.elements[1] else {
        unreachable!()
    };
    edge.quantifier = crate::EdgeQuantifierKind::Bounded { min: 3, max: 2 };
    let error = match BoundedPathProgram::compile(&set.automata, &a) {
        Err(e) => e,
        Ok(_) => panic!("invalid bounds accepted"),
    };
    assert!(
        matches!(error,ExecutorError::ImplementationDefined { detail } if detail.contains("unsatisfiable"))
    );
    let mut set = lower_path_automata_with_defaults(&a).unwrap();
    set.automata[0].transitions[1].to = crate::PathStateId(0);
    assert!(matches!(
        BoundedPathProgram::compile(&set.automata, &a),
        Err(ExecutorError::ImplementationDefined { .. })
    ));
}
