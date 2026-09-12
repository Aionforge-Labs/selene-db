//! Separate triggers for inherited caps, syntax containment and per-hop budgets.

use super::*;
use super::{
    oracle::Fixture,
    tests::{analyzed, run},
};
use crate::{EmptyProcedureRegistry, ImplDefinedCaps, lower_path_automata_with_defaults};

#[test]
fn fixed_and_quantified_primitives_use_the_same_caps_and_budget_contract() {
    let fixture = Fixture::new(2, &[(0, 1, true, true)]);
    for source in [
        "MATCH (a)-[r]->(b) RETURN a",
        "MATCH (a)-[r{1}]->(b) RETURN a",
    ] {
        let a = analyzed(source);
        let set = lower_path_automata_with_defaults(&a).unwrap();
        let program = BoundedPathProgram::compile(&set.automata, &a).unwrap();
        let caps = ImplDefinedCaps {
            max_quantifier: 0,
            ..Default::default()
        };
        let tx = TxContext::read_only(
            fixture.graph.read(),
            &caps,
            &EmptyProcedureRegistry,
            fixture.graph.index_providers(),
        );
        assert!(matches!(
            program.execute(&tx, PathExecutionLimits::default()),
            Err(ExecutorError::ProgramLimitExceeded {
                detail: "max_path_hops",
                ..
            })
        ));
        let caps = ImplDefinedCaps::default();
        let tx = TxContext::read_only(
            fixture.graph.read(),
            &caps,
            &EmptyProcedureRegistry,
            fixture.graph.index_providers(),
        )
        .with_resource_limits(None, None, Some(0), None);
        assert!(matches!(
            program.execute(&tx, PathExecutionLimits::default()),
            Err(ExecutorError::RowCapExceeded { cap: 0, .. })
        ));
        for limits in [
            PathExecutionLimits {
                max_rows: 0,
                ..Default::default()
            },
            PathExecutionLimits {
                max_bytes: 0,
                ..Default::default()
            },
            PathExecutionLimits {
                max_work: 2,
                ..Default::default()
            },
        ] {
            assert!(matches!(
                run(&fixture.graph, source, limits),
                Err(ExecutorError::ProgramLimitExceeded { .. })
            ));
        }
    }
}

#[test]
fn unsupported_group_syntax_has_no_fallback() {
    for source in [
        "MATCH ((a)-[r]->(b)){1,2} RETURN a",
        "MATCH ((a)-[r]->(b)|(a)-[s]->(b)) RETURN a",
        "MATCH (a)-[r]->(b)|(a)-[s]->(b) RETURN a",
    ] {
        assert!(crate::parse(source).is_err(), "{source}");
    }
}

#[test]
fn empty_and_isolated_graphs_keep_zero_length_and_empty_schema_contracts() {
    for n in [0, 1, 3] {
        let f = Fixture::new(n, &[]);
        let zero = run(
            &f.graph,
            "MATCH ()-[{0}]->() RETURN 1",
            PathExecutionLimits::default(),
        )
        .unwrap();
        assert_eq!(zero.table.row_count(), n);
        assert!(zero.table.schema().columns.is_empty());
        let single = run(
            &f.graph,
            "MATCH (a)-[r]->(b) RETURN a",
            PathExecutionLimits::default(),
        )
        .unwrap();
        assert_eq!(single.table.row_count(), 0);
        assert_eq!(single.table.schema().columns.len(), 3);
    }
}

#[test]
fn labels_filter_every_hop_and_endpoint_without_reducing_parallel_multiplicity() {
    let f = Fixture::new(
        3,
        &[
            (0, 1, true, true),
            (0, 1, true, true),
            (1, 2, true, false),
            (2, 0, true, true),
        ],
    );
    for (source, expected) in [
        ("MATCH (a:Root)-[r:K{2}]->(b) RETURN a", 0),
        ("MATCH (a:Root)-[r:K|L{2}]->(b:N) RETURN a", 2),
        ("MATCH (a:Root)-[r:K|L{3}]->(b:Root) RETURN a", 2),
        ("MATCH (a:Root)-[r:K|L{3}]->(b:N) RETURN a", 0),
        ("MATCH (a:Root|N)-[r:K|L{0}]->(b) RETURN a", 3),
    ] {
        assert_eq!(
            run(&f.graph, source, PathExecutionLimits::default())
                .unwrap()
                .table
                .row_count(),
            expected,
            "{source}"
        );
    }
}

#[test]
fn malformed_ir_overflow_and_missing_states_do_not_execute() {
    let a = analyzed("MATCH (a)-[r{1}]->(m)-[s{1}]->(b) RETURN a");
    let set = lower_path_automata_with_defaults(&a).unwrap();
    let mut missing = set.clone();
    missing.automata[0].states.pop();
    assert!(matches!(
        BoundedPathProgram::compile(&missing.automata, &a),
        Err(ExecutorError::ImplementationDefined { .. })
    ));
    let mut overflow = set.clone();
    for i in [1, 3] {
        let crate::PathSemanticElement::Edge(edge) = &mut overflow.automata[0].semantic.elements[i]
        else {
            unreachable!()
        };
        edge.quantifier = crate::EdgeQuantifierKind::Bounded {
            min: 1,
            max: u32::MAX,
        };
        overflow.automata[0].transitions[i].kind = crate::TransitionKind::QuantifiedEdge {
            test: (i / 2) as u32,
            min: 1,
            max: Some(u32::MAX),
        };
    }
    assert!(matches!(
        BoundedPathProgram::compile(&overflow.automata, &a),
        Err(ExecutorError::ImplementationDefined {
            detail: "bounded path length overflow"
        })
    ));
}
