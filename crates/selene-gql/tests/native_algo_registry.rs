//! End-to-end coverage for the native [`BuiltinProcedureRegistry`].
//!
//! These tests drive `CALL algo.*` through the concrete native registry (no
//! procedure-pack machinery), exercising plan-time lookup, tier-checked
//! dispatch, the per-`GraphId` projection lifecycle, and a real algorithm run.
//! They are the STEP-2 parity guard: every `algo.*` name resolves and executes
//! identically to the pack era, with the registry swapped behind the
//! `ProcedureRegistry` trait.

use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Record, Value};
use selene_gql::{
    BindingTable, BuiltinProcedureRegistry, ProcedureRegistry, Session, StatementOutput,
};
use selene_graph::{SharedGraph, TypedIndexKind};
use smallvec::smallvec;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn graph(id: u64) -> SharedGraph {
    SharedGraph::new(GraphId::new(id))
}

fn seed_isolated_nodes(graph: &SharedGraph, count: usize) -> Vec<NodeId> {
    let mut txn = graph.begin_write();
    let label = db_string("N");
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        nodes.push(
            txn.mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .expect("test node creates"),
        );
    }
    txn.commit().expect("test graph commit");
    nodes
}

fn seed_sink_personalization_graph(graph: &SharedGraph) -> Vec<NodeId> {
    let mut txn = graph.begin_write();
    let label = db_string("N");
    let rel = db_string("R");
    let mut nodes = Vec::with_capacity(3);
    for _ in 0..3 {
        nodes.push(
            txn.mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .expect("test node creates"),
        );
    }
    txn.mutator()
        .create_edge(rel.clone(), nodes[0], nodes[1], PropertyMap::new())
        .expect("fact edge creates");
    txn.mutator()
        .create_edge(rel, nodes[2], nodes[1], PropertyMap::new())
        .expect("episode edge creates");
    txn.commit().expect("test graph commit");
    nodes
}

fn seed_labeled_pagerank_graph(graph: &SharedGraph) -> Vec<NodeId> {
    let mut txn = graph.begin_write();
    let fact = db_string("Fact");
    let entity = db_string("Entity");
    let about = db_string("ABOUT");
    let left = txn
        .mutator()
        .create_node(LabelSet::single(fact.clone()), PropertyMap::new())
        .expect("left fact creates");
    let center = txn
        .mutator()
        .create_node(LabelSet::single(entity), PropertyMap::new())
        .expect("entity creates");
    let right = txn
        .mutator()
        .create_node(LabelSet::single(fact), PropertyMap::new())
        .expect("right fact creates");
    txn.mutator()
        .create_edge(about.clone(), left, center, PropertyMap::new())
        .expect("left fact edge creates");
    txn.mutator()
        .create_edge(about, right, center, PropertyMap::new())
        .expect("right fact edge creates");
    txn.commit().expect("test graph commit");
    vec![left, center, right]
}

fn edge_props(property: &DbString, value: &str) -> PropertyMap {
    PropertyMap::from_pairs([(property.clone(), Value::String(db_string(value)))])
        .expect("test edge property map is valid")
}

fn seed_edge_filtered_pagerank_graph(graph: &SharedGraph) -> Vec<NodeId> {
    let mut txn = graph.begin_write();
    let doc = db_string("Doc");
    let commit = db_string("Commit");
    let rel = db_string("R");
    let at_commit = db_string("AT_COMMIT");
    let commit_sha = db_string("commit_sha");
    let mut mutator = txn.mutator();
    let commit_node = mutator
        .create_node(LabelSet::single(commit), PropertyMap::new())
        .expect("commit node creates");
    let mut docs = Vec::new();
    for _ in 0..3 {
        docs.push(
            mutator
                .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
                .expect("doc node creates"),
        );
    }
    mutator
        .create_edge(rel.clone(), docs[0], docs[1], PropertyMap::new())
        .expect("first rank edge creates");
    mutator
        .create_edge(rel.clone(), docs[1], docs[2], PropertyMap::new())
        .expect("second rank edge creates");
    mutator
        .create_edge(rel, docs[2], docs[0], PropertyMap::new())
        .expect("third rank edge creates");
    for (node, sha) in [(docs[0], "abc"), (docs[1], "def"), (docs[2], "abc")] {
        mutator
            .create_edge(
                at_commit.clone(),
                commit_node,
                node,
                edge_props(&commit_sha, sha),
            )
            .expect("commit edge creates");
    }
    mutator
        .create_edge_property_index(at_commit, commit_sha, TypedIndexKind::String)
        .expect("commit edge property index creates");
    txn.commit().expect("test graph commit");
    docs
}

fn rows(output: StatementOutput) -> BindingTable {
    match output {
        StatementOutput::Rows(table) => table,
        StatementOutput::Written(outcome) => outcome.rows.expect("written statement returned rows"),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn execute_rows(
    session: &mut Session<'_>,
    source: &str,
    registry: &dyn ProcedureRegistry,
) -> BindingTable {
    rows(
        session
            .execute_source(source, registry)
            .expect("statement executes"),
    )
}

fn float_column(table: &BindingTable, name: &str) -> Vec<f64> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::Float(value)) => *value,
            other => panic!("expected float in {name}, got {other:?}"),
        })
        .collect()
}

fn node_column(table: &BindingTable, name: &str) -> Vec<NodeId> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::NodeRef(value)) => *value,
            other => panic!("expected node ref in {name}, got {other:?}"),
        })
        .collect()
}

fn sorted_node_ids(mut nodes: Vec<NodeId>) -> Vec<NodeId> {
    nodes.sort_by_key(|node| node.get());
    nodes
}

fn string_column(table: &BindingTable, name: &str) -> Vec<String> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::String(value)) => value.as_str().to_owned(),
            other => panic!("expected string in {name}, got {other:?}"),
        })
        .collect()
}

/// Build a small directed triangle graph: (a)->(b)->(c)->(a).
fn seed_triangle(session: &mut Session<'_>, registry: &dyn ProcedureRegistry) {
    session
        .execute_source(
            "INSERT (a:N)-[:E]->(b:N), (b)-[:E]->(c:N), (c)-[:E]->(a)",
            registry,
        )
        .expect("seed graph inserts");
}

fn personalization_seed(node: NodeId, weight: f64) -> Value {
    Value::Record(Box::new(Record::Open(smallvec![
        (db_string("node_id"), Value::NodeRef(node)),
        (db_string("weight"), Value::Float(weight)),
    ])))
}

#[test]
fn show_procedures_lists_all_nineteen_algo_procedures() {
    let graph = graph(220_001);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let table = execute_rows(&mut session, "SHOW PROCEDURES", &registry);
    let names = string_column(&table, "name");

    // The registry also carries the 49 `selene.*` platform built-ins, so SHOW
    // PROCEDURES lists 69; all 19 algo names must still be present.
    assert_eq!(
        table.row_count(),
        69,
        "expected 19 algo procedures + 50 platform built-ins"
    );
    for expected in [
        "algo.projection_build",
        "algo.projection_get",
        "algo.projection_drop",
        "algo.projection_list",
        "algo.pagerank",
        "algo.betweenness",
        "algo.label_propagation",
        "algo.louvain",
        "algo.triangle_count",
        "algo.wcc",
        "algo.scc",
        "algo.wcc_count",
        "algo.scc_count",
        "algo.topological_sort",
        "algo.articulation_points",
        "algo.bridges",
        "algo.dijkstra",
        "algo.sssp",
        "algo.apsp",
    ] {
        assert!(
            names.contains(&expected.to_owned()),
            "SHOW PROCEDURES must list {expected}"
        );
    }
}

#[test]
fn pagerank_runs_end_to_end_over_a_built_projection() {
    let graph = graph(220_002);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    seed_triangle(&mut session, &registry);

    session
        .execute_source(
            "CALL algo.projection_build('p', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");

    let table = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', NULL, NULL, NULL, NULL) YIELD node_id, score",
        &registry,
    );

    // Three nodes in the triangle, each with a finite positive score summing to
    // ~1.0 (PageRank teleport-normalized over a symmetric cycle).
    let scores = float_column(&table, "score");
    assert_eq!(scores.len(), 3, "one score row per node");
    assert!(
        scores.iter().all(|s| s.is_finite() && *s > 0.0),
        "scores must be finite and positive: {scores:?}"
    );
    let total: f64 = scores.iter().sum();
    assert!(
        (total - 1.0).abs() < 1e-6,
        "pagerank scores should sum to ~1.0, got {total}"
    );
}

#[test]
fn pagerank_accepts_personalization_parameter() {
    let graph = graph(220_008);
    let nodes = seed_isolated_nodes(&graph, 3);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL algo.projection_build('p', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");
    session.bind_parameter(
        db_string("seeds"),
        Value::List(vec![personalization_seed(nodes[2], 1.0)]),
    );

    let table = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', 0.85D, 10, 0.0D, NULL, 'NATURAL', $seeds) YIELD node_id, score",
        &registry,
    );

    let result_nodes = node_column(&table, "node_id");
    let scores = float_column(&table, "score");
    assert_eq!(result_nodes[0], nodes[2]);
    assert!((scores[0] - 1.0).abs() < 1e-12);
    assert!((scores[1] - 0.0).abs() < 1e-12);
    assert!((scores[2] - 0.0).abs() < 1e-12);
}

#[test]
fn pagerank_filters_results_by_label_and_limit() {
    let graph = graph(220_011);
    let nodes = seed_labeled_pagerank_graph(&graph);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL algo.projection_build('p', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");

    let all_facts = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', 0.85D, 10, 0.0D, NULL, 'NATURAL', NULL, 'Fact', NULL) \
         YIELD node_id, score",
        &registry,
    );
    assert_eq!(node_column(&all_facts, "node_id"), vec![nodes[0], nodes[2]]);
    assert!(
        float_column(&all_facts, "score")
            .iter()
            .all(|score| *score > 0.0)
    );

    let top_fact = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', 0.85D, 10, 0.0D, NULL, 'NATURAL', NULL, 'Fact', 1) \
         YIELD node_id, score",
        &registry,
    );
    assert_eq!(node_column(&top_fact, "node_id"), vec![nodes[0]]);

    let zero = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', 0.85D, 10, 0.0D, NULL, 'NATURAL', NULL, 'Fact', 0) \
         YIELD node_id, score",
        &registry,
    );
    assert_eq!(zero.row_count(), 0);
}

#[test]
fn pagerank_result_nodes_intersects_before_limit() {
    let graph = graph(220_012);
    let nodes = seed_labeled_pagerank_graph(&graph);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL algo.projection_build('p', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");
    session.bind_parameter(
        db_string("result_nodes"),
        Value::List(vec![Value::NodeRef(nodes[2])]),
    );

    let restricted = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', 0.85D, 10, 0.0D, NULL, 'NATURAL', NULL, NULL, 1, $result_nodes) \
         YIELD node_id, score",
        &registry,
    );
    assert_eq!(node_column(&restricted, "node_id"), vec![nodes[2]]);
    assert!(float_column(&restricted, "score")[0] > 0.0);

    session.bind_parameter(db_string("result_nodes"), Value::List(Vec::new()));
    let empty = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', 0.85D, 10, 0.0D, NULL, 'NATURAL', NULL, NULL, 1, $result_nodes) \
         YIELD node_id, score",
        &registry,
    );
    assert_eq!(empty.row_count(), 0);
}

#[test]
fn pagerank_edge_filter_scopes_result_candidates() {
    let graph = graph(220_013);
    let nodes = seed_edge_filtered_pagerank_graph(&graph);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL algo.projection_build('p', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");
    session.bind_parameter(
        db_string("abc"),
        Value::List(vec![Value::String(db_string("abc"))]),
    );
    session.bind_parameter(
        db_string("only_last"),
        Value::List(vec![Value::NodeRef(nodes[2])]),
    );

    let edge_filtered = execute_rows(
        &mut session,
        "CALL algo.pagerank(
             'p', 0.85D, 20, 0.0D, NULL, 'NATURAL',
             NULL, 'Doc', NULL, NULL,
             'AT_COMMIT', 'commit_sha', $abc, 'target'
         ) YIELD node_id, score",
        &registry,
    );
    assert_eq!(
        sorted_node_ids(node_column(&edge_filtered, "node_id")),
        vec![nodes[0], nodes[2]]
    );

    let composed = execute_rows(
        &mut session,
        "CALL algo.pagerank(
             'p', 0.85D, 20, 0.0D, NULL, 'NATURAL',
             NULL, 'Doc', NULL, $only_last,
             'AT_COMMIT', 'commit_sha', $abc, 'target'
         ) YIELD node_id, score",
        &registry,
    );
    assert_eq!(node_column(&composed, "node_id"), vec![nodes[2]]);
    assert!(float_column(&composed, "score")[0] > 0.0);
}

#[test]
fn pagerank_undirected_orientation_spreads_personalized_sink_seed() {
    let graph = graph(220_010);
    let nodes = seed_sink_personalization_graph(&graph);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL algo.projection_build('p', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");
    session.bind_parameter(
        db_string("seeds"),
        Value::List(vec![personalization_seed(nodes[1], 1.0)]),
    );

    let natural = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', 0.85D, 1, 0.0D, NULL, 'NATURAL', $seeds) YIELD node_id, score",
        &registry,
    );
    let undirected = execute_rows(
        &mut session,
        "CALL algo.pagerank('p', 0.85D, 1, 0.0D, NULL, 'UNDIRECTED', $seeds) YIELD node_id, score",
        &registry,
    );
    let score_for = |table: &BindingTable, node| {
        node_column(table, "node_id")
            .into_iter()
            .zip(float_column(table, "score"))
            .find(|(candidate, _)| *candidate == node)
            .expect("node has score")
            .1
    };

    assert!((score_for(&natural, nodes[1]) - 1.0).abs() < 1e-12);
    assert!((score_for(&natural, nodes[0]) - 0.0).abs() < 1e-12);
    assert!((score_for(&natural, nodes[2]) - 0.0).abs() < 1e-12);
    assert!((score_for(&undirected, nodes[1]) - 0.15).abs() < 1e-12);
    assert!((score_for(&undirected, nodes[0]) - 0.425).abs() < 1e-12);
    assert!((score_for(&undirected, nodes[2]) - 0.425).abs() < 1e-12);
}

#[test]
fn pagerank_personalization_rejects_seed_outside_projection() {
    let graph = graph(220_009);
    seed_isolated_nodes(&graph, 2);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source("INSERT (:Outside)", &registry)
        .unwrap();
    session
        .execute_source(
            "CALL algo.projection_build('p', ['N'], NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");
    session.bind_parameter(
        db_string("seeds"),
        Value::List(vec![personalization_seed(NodeId::new(3), 1.0)]),
    );

    let err = session
        .execute_source(
            "CALL algo.pagerank('p', 0.85D, 10, 0.0D, NULL, 'NATURAL', $seeds) YIELD node_id, score",
            &registry,
        )
        .expect_err("out-of-projection seed rejected");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("not in projection") && rendered.contains("3"),
        "error should mention out-of-projection seed, got: {rendered}"
    );
}

#[test]
fn wcc_count_yields_single_component_for_connected_graph() {
    let graph = graph(220_003);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    seed_triangle(&mut session, &registry);

    session
        .execute_source(
            "CALL algo.projection_build('p', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");

    let table = execute_rows(
        &mut session,
        "CALL algo.wcc_count('p') YIELD count",
        &registry,
    );

    let index = table
        .column_index(db_string("count"))
        .expect("count column");
    let count = match table.rows()[0].values().get(index) {
        Some(Value::Uint(value)) => *value,
        other => panic!("expected uint count, got {other:?}"),
    };
    assert_eq!(
        count, 1,
        "a directed triangle is one weakly connected component"
    );
}

#[test]
fn projection_list_reports_built_projection() {
    let graph = graph(220_004);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    seed_triangle(&mut session, &registry);

    session
        .execute_source(
            "CALL algo.projection_build('proj_a', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");

    let table = execute_rows(
        &mut session,
        "CALL algo.projection_list() YIELD name, node_count, edge_count",
        &registry,
    );
    let names = string_column(&table, "name");
    assert_eq!(names, vec!["proj_a".to_owned()]);
}

#[test]
fn pagerank_on_missing_projection_is_an_invalid_argument_error() {
    let graph = graph(220_005);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    seed_triangle(&mut session, &registry);

    let err = session
        .execute_source(
            "CALL algo.pagerank('does_not_exist', NULL, NULL, NULL, NULL) YIELD node_id, score",
            &registry,
        )
        .expect_err("missing projection must error");
    let rendered = format!("{err:?}");
    assert!(
        rendered.to_lowercase().contains("projection")
            || rendered.to_lowercase().contains("does_not_exist"),
        "error should mention the missing projection, got: {rendered}"
    );
}

#[test]
fn unknown_algo_procedure_is_rejected_at_plan_time() {
    let graph = graph(220_006);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let err = session
        .execute_source("CALL algo.does_not_exist('p')", &registry)
        .expect_err("unknown procedure must be rejected");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("does_not_exist") || rendered.to_lowercase().contains("procedure"),
        "unknown procedure error should name the procedure, got: {rendered}"
    );
}

#[test]
fn forget_graph_clears_projection_state() {
    let graph = graph(220_007);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    seed_triangle(&mut session, &registry);

    session
        .execute_source(
            "CALL algo.projection_build('p', NULL, NULL, NULL)",
            &registry,
        )
        .expect("projection_build executes");

    // The catalog for this graph now exists; forgetting it returns true, and a
    // subsequent forget returns false (state was reclaimed).
    assert!(registry.forget_graph(GraphId::new(220_007)));
    assert!(!registry.forget_graph(GraphId::new(220_007)));

    // After reclamation, the projection is gone: list reports nothing.
    let table = execute_rows(
        &mut session,
        "CALL algo.projection_list() YIELD name",
        &registry,
    );
    assert_eq!(table.row_count(), 0, "projection state cleared on forget");
}
