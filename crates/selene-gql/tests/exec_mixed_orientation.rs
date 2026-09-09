//! Independent mixed-edge orientation regressions (ISO §16.7, §19.8, §19.10).

#![cfg(feature = "test-harness")]

mod exec_common;

use exec_common::{column_values, db_string, planned};
use selene_core::{EdgeDirectionality, GraphId, LabelSet, PropertyMap, Value};
use selene_gql::{
    Binding, BindingTable, EmptyProcedureRegistry, TxContext, execute_pattern, execute_pipeline,
};
use selene_graph::SharedGraph;
use selene_testing::mixed_orientation::{MixedOrientationFixture, ORIENTATIONS};

fn undirected_fixture() -> (SharedGraph, selene_core::NodeId, selene_core::EdgeId) {
    let graph = SharedGraph::new(GraphId::new(904));
    let mut tx = graph.begin_write();
    let mut m = tx.mutator();
    let a = m
        .create_node(LabelSet::single(db_string("A")), PropertyMap::new())
        .unwrap();
    let b = m
        .create_node(LabelSet::single(db_string("B")), PropertyMap::new())
        .unwrap();
    let edge = m
        .create_mixed_edge(
            db_string("E"),
            b,
            a,
            EdgeDirectionality::Undirected,
            PropertyMap::new(),
        )
        .unwrap();
    tx.commit().unwrap();
    (graph, a, edge)
}

#[test]
fn any_orientation_includes_intrinsic_undirected_edge() {
    let (graph, _, edge) = undirected_fixture();
    let plan = planned("MATCH (a:A)-[e]-(b:B) RETURN e");
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    )
    .with_plan_metadata(&plan.expr_ids, &plan.subqueries);
    let input = execute_pattern(plan.pattern_plan.as_ref().unwrap(), &ctx).unwrap();
    let table = execute_pipeline(&plan.pipeline, input, &mut ctx).unwrap();
    assert_eq!(column_values(&table, "e"), vec![Value::EdgeRef(edge)]);
}

#[test]
fn intrinsic_undirected_predicates_do_not_use_canonical_source() {
    let (graph, node, edge) = undirected_fixture();
    let plan = planned(
        "MATCH (a)-[e]->() RETURN e IS DIRECTED AS d, a IS SOURCE OF e AS s, a IS DESTINATION OF e AS t",
    );
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    )
    .with_plan_metadata(&plan.expr_ids, &plan.subqueries);
    // Inject typed bindings to test predicates independently of the expansion bug.
    let schema = execute_pattern(plan.pattern_plan.as_ref().unwrap(), &ctx)
        .unwrap()
        .schema()
        .clone();
    let row = Binding::new(schema.columns.iter().map(
        |c| match c.name.as_ref().map(|n| n.as_str()) {
            Some("a") => Value::NodeRef(node),
            Some("e") => Value::EdgeRef(edge),
            _ => Value::Null,
        },
    ));
    let table = execute_pipeline(
        &plan.pipeline,
        BindingTable::new(schema, vec![row]),
        &mut ctx,
    )
    .unwrap();
    for name in ["d", "s", "t"] {
        assert_eq!(
            column_values(&table, name),
            vec![Value::Bool(false)],
            "{name}"
        );
    }
}

#[test]
fn all_seven_full_and_abbreviated_forms_parse() {
    for edge in [
        "->", "<-", "~", "<~", "~>", "<->", "-", "-[]->", "<-[]-", "~[]~", "<~[]~", "~[]~>",
        "<-[]->", "-[]-",
    ] {
        assert!(
            selene_gql::parse(&format!("MATCH (a){edge}(b) RETURN a")).is_ok(),
            "{edge}"
        );
    }
}

fn query(graph: &SharedGraph, source: &str, indexed: bool) -> BindingTable {
    let mut session = selene_gql::Session::new(graph);
    if !indexed {
        session = session.without_index_selection();
    }
    match session
        .execute_source(source, &EmptyProcedureRegistry)
        .unwrap()
    {
        selene_gql::StatementOutput::Rows(rows) => rows,
        other => panic!("expected rows: {other:?}"),
    }
}

#[test]
fn independent_table_matches_all_forms_loops_parallel_and_reversed_creation() {
    let f = MixedOrientationFixture::build();
    for case in &ORIENTATIONS {
        for (label, expected) in [("A", case.from_a), ("B", case.from_b), ("C", &[][..])] {
            let source = format!("MATCH (a:{label}){}(b) RETURN e ORDER BY e.key", case.full);
            let rows = query(&f.graph, &source, false);
            assert_eq!(
                column_values(&rows, "e"),
                expected
                    .iter()
                    .map(|&i| Value::EdgeRef(f.edges[i]))
                    .collect::<Vec<_>>(),
                "{source}"
            );
            // Abbreviated patterns have no user edge variable, but still
            // preserve the multiplicity of distinct edge identities.
            let source = format!("MATCH (a:{label}){}(b) RETURN a, b", case.abbreviated);
            let rows = query(&f.graph, &source, false);
            let mut actual = column_values(&rows, "b")
                .into_iter()
                .map(|v| match v {
                    Value::NodeRef(n) => n,
                    _ => panic!(),
                })
                .collect::<Vec<_>>();
            let mut wanted = expected
                .iter()
                .map(|&i| {
                    if i == 4 || i == 5 {
                        f.nodes[0]
                    } else if label == "A" {
                        f.nodes[1]
                    } else {
                        f.nodes[0]
                    }
                })
                .collect::<Vec<_>>();
            actual.sort();
            wanted.sort();
            assert_eq!(actual, wanted, "{source}");

            // Explicit DIFFERENT EDGES retains hidden edge columns, allowing
            // the test harness to inspect identity without changing the default
            // mode or inventing an abbreviated edge-variable syntax.
            let p = planned(&format!(
                "MATCH DIFFERENT EDGES (a:{label}){}(b) RETURN a",
                case.abbreviated
            ));
            let ctx = TxContext::read_only(
                f.graph.read(),
                &p.impl_defined_caps,
                &EmptyProcedureRegistry,
                f.graph.index_providers(),
            )
            .with_plan_metadata(&p.expr_ids, &p.subqueries);
            let bindings = execute_pattern(p.pattern_plan.as_ref().unwrap(), &ctx).unwrap();
            let mut hidden = bindings
                .rows()
                .iter()
                .map(|row| {
                    row.values()
                        .iter()
                        .find_map(|v| match v {
                            Value::EdgeRef(e) => Some(*e),
                            _ => None,
                        })
                        .expect("hidden edge identity")
                })
                .collect::<Vec<_>>();
            hidden.sort();
            assert_eq!(
                hidden,
                expected.iter().map(|&i| f.edges[i]).collect::<Vec<_>>(),
                "abbreviated {} from {label}",
                case.abbreviated
            );
        }
    }
}

#[test]
fn selective_index_and_adjacency_agree_with_stable_id_oracle() {
    let f = MixedOrientationFixture::build();
    f.graph
        .create_edge_property_index(
            db_string("E"),
            db_string("key"),
            selene_graph::TypedIndexKind::I64,
        )
        .unwrap();
    for case in &ORIENTATIONS {
        for key in 0..f.edges.len() {
            let edge = case.full.replace("[e]", "[e:E]");
            // Three child rows vs one candidate forces the indexed-edge branch.
            let source = format!("MATCH (a:N){edge}(b:N) WHERE e.key = {key} RETURN a, e, b");
            let mut expected = Vec::new();
            for (start, eligible) in [(0, case.from_a), (1, case.from_b)] {
                for &i in eligible {
                    if i == key {
                        let end = if i == 4 || i == 5 { 0 } else { 1 - start };
                        expected.push((f.nodes[start], f.edges[i], f.nodes[end]));
                    }
                }
            }
            expected.sort();
            for indexed in [false, true] {
                let table = query(&f.graph, &source, indexed);
                let mut actual = table
                    .rows()
                    .iter()
                    .map(|row| match row.values() {
                        [Value::NodeRef(a), Value::EdgeRef(e), Value::NodeRef(b)] => (*a, *e, *b),
                        other => panic!("{other:?}"),
                    })
                    .collect::<Vec<_>>();
                actual.sort();
                assert_eq!(actual, expected, "indexed={indexed} {source}");
            }
            let explain = query(&f.graph, &format!("EXPLAIN {source}"), true);
            assert!(
                matches!(&explain.rows()[0].values()[0], Value::String(s) if s.as_str().contains("TypedIndexRange")),
                "{source}"
            );
        }
    }
}

#[test]
fn questioned_repeat_paths_and_duplicate_input_rows_preserve_identity() {
    let f = MixedOrientationFixture::build();
    let rows = query(
        &f.graph,
        "MATCH (a:B)~[e]~?(b) RETURN e IS DIRECTED AS d, a IS SOURCE OF e AS s, e IS NOT DIRECTED AS nd",
        false,
    );
    assert_eq!(rows.row_count(), 3);
    for name in ["d", "s"] {
        let values = column_values(&rows, name);
        assert_eq!(values.iter().filter(|v| **v == Value::Null).count(), 1);
        assert_eq!(
            values.iter().filter(|v| **v == Value::Bool(false)).count(),
            2
        );
    }
    assert_eq!(
        column_values(&rows, "nd")
            .iter()
            .filter(|v| **v == Value::Bool(true))
            .count(),
        2
    );
    let rows = query(&f.graph, "MATCH (a:B)~[e]~{1,1}(b:A) RETURN e", false);
    assert_eq!(rows.row_count(), 2);
    for row in rows.rows() {
        assert!(
            matches!(&row.values()[0], Value::List(es) if es.len() == 1 && [Value::EdgeRef(f.edges[2]), Value::EdgeRef(f.edges[3])].contains(&es[0]))
        );
    }
    let rows = query(
        &f.graph,
        "MATCH (a:B)~[e]~(b:A) RETURN PATH[a,e,b] AS p",
        false,
    );
    for row in rows.rows() {
        assert!(
            matches!(&row.values()[0], Value::Path(p) if p.segments[0].direction == selene_core::EdgeDirection::Undirected)
        );
    }
    let rows = query(
        &f.graph,
        "FOR x IN [1,1] MATCH (a:B)~[e]~(b:A) RETURN e",
        false,
    );
    assert_eq!(rows.row_count(), 4, "dedup must not cross input rows");
}

#[test]
fn insert_rejects_union_and_abbreviated_forms() {
    for edge in [
        "-[:E]-", "<-[:E]->", "<~[:E]~", "~[:E]~>", "->", "<-", "~", "<~", "~>", "<->", "-",
    ] {
        assert!(
            selene_gql::parse(&format!("INSERT (:A){edge}(:B)")).is_err(),
            "{edge}"
        );
    }
}

#[test]
fn questioned_and_repeat_use_the_same_orientation_union_table() {
    let f = MixedOrientationFixture::build();
    for case in &ORIENTATIONS {
        for suffix in ["?", "{1,1}"] {
            let source = format!("MATCH (a:A){}{suffix}(b) RETURN e", case.full);
            let rows = query(&f.graph, &source, false);
            let mut actual = Vec::new();
            let mut skipped = 0;
            for value in column_values(&rows, "e") {
                match value {
                    Value::Null => skipped += 1,
                    Value::EdgeRef(e) => actual.push(e),
                    Value::List(values) => {
                        assert_eq!(values.len(), 1);
                        let Value::EdgeRef(e) = values[0] else {
                            panic!()
                        };
                        actual.push(e);
                    }
                    other => panic!("{other:?}"),
                }
            }
            actual.sort();
            assert_eq!(
                actual,
                case.from_a.iter().map(|&i| f.edges[i]).collect::<Vec<_>>(),
                "{source}"
            );
            assert_eq!(skipped, usize::from(suffix == "?"));
        }
    }
    for (mode, count) in [("WALK", 4), ("TRAIL", 2), ("SIMPLE", 4), ("ACYCLIC", 0)] {
        let source = format!("MATCH {mode} (a:B)~[e]~{{2,2}}(b:B) RETURN e");
        assert_eq!(
            query(&f.graph, &source, false).row_count(),
            count,
            "{source}"
        );
    }
}

#[test]
fn syntax_provenance_selects_only_the_applicable_orientation_features() {
    use selene_profile::FeatureId;
    for (index, case) in ORIENTATIONS.iter().enumerate() {
        for (edge, abbreviated) in [(case.full, false), (case.abbreviated, true)] {
            let statement = selene_gql::parse(&format!("MATCH (a){edge}(b) RETURN a")).unwrap();
            let features = selene_gql::feature_walk(&statement)
                .into_iter()
                .map(|u| u.feature_id)
                .collect::<Vec<_>>();
            let complete = (2..=5).contains(&index);
            assert_eq!(
                features.contains(&FeatureId::G043),
                !abbreviated && complete,
                "{edge}"
            );
            assert_eq!(
                features.contains(&FeatureId::G044),
                abbreviated && !complete,
                "{edge}"
            );
            assert_eq!(
                features.contains(&FeatureId::G045),
                abbreviated && complete,
                "{edge}"
            );
            assert_eq!(
                features.contains(&FeatureId::GH02),
                (2..=4).contains(&index),
                "{edge}"
            );
            let formatted = selene_gql::ast::format_read_statement(&statement).unwrap();
            let reparsed = selene_gql::parse(&formatted).unwrap();
            assert!(
                selene_gql::ast::structurally_eq(&statement, &reparsed),
                "{edge}"
            );
        }
    }
}

#[test]
fn closed_mixed_insert_validates_both_endpoint_orders_without_canonical_source() {
    let graph = SharedGraph::builder(GraphId::new(906))
        .bound_to(selene_testing::person_company_graph_type())
        .unwrap()
        .build()
        .unwrap();
    let mut session = selene_gql::Session::new(&graph);
    for source in [
        "INSERT (:Person {name:'A'})~[:WORKS_AT {since:2026}]~(:Company {name:'B'}) FINISH",
        "INSERT (:Company {name:'C'})~[:WORKS_AT {since:2026}]~(:Person {name:'D'}) FINISH",
    ] {
        session
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap();
    }
    assert_eq!(graph.read().edge_count(), 2);
    for source in [
        "INSERT (:Person {name:'A'})~[:WORKS_AT]~(:Person {name:'B'}) FINISH",
        "INSERT (:Company {name:'A'})~[:WORKS_AT {since:'wrong'}]~(:Person {name:'B'}) FINISH",
    ] {
        assert!(
            session
                .execute_source(source, &EmptyProcedureRegistry)
                .is_err()
        );
        assert_eq!(graph.read().node_count(), 4);
        assert_eq!(graph.read().edge_count(), 2);
    }
}

#[test]
fn gql_algorithm_projection_consumer_sees_intrinsic_connectivity_and_logical_count() {
    let (graph, _, _) = undirected_fixture();
    let registry = selene_gql::BuiltinProcedureRegistry::new();
    let mut session = selene_gql::Session::new(&graph);
    session
        .execute_source(
            "CALL algo.projection_build('mixed', NULL, NULL, NULL)",
            &registry,
        )
        .unwrap();
    for (source, name) in [
        (
            "CALL algo.projection_get('mixed') YIELD edge_count",
            "edge_count",
        ),
        ("CALL algo.scc_count('mixed') YIELD count", "count"),
    ] {
        let selene_gql::StatementOutput::Rows(rows) =
            session.execute_source(source, &registry).unwrap()
        else {
            panic!()
        };
        assert_eq!(column_values(&rows, name), vec![Value::Uint(1)]);
    }
}
