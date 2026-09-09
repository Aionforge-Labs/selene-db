//! End-to-end coverage for the relocated `selene.*` platform built-ins.
//!
//! These tests drive `CALL selene.*` through the concrete native
//! [`BuiltinProcedureRegistry`] (STEP 3 relocation, no procedure-pack
//! machinery), exercising plan-time lookup, tier-checked dispatch, and — for the
//! mutation-tier built-ins — the single mutation funnel (`SHOW INDEXES` reads
//! the committed `property_index`, which the procedure populated through
//! `MutationContext::mutator`). They are the parity guard that the relocated
//! built-ins behave identically to the pack era with the registry swapped behind
//! the `ProcedureRegistry` trait, plus coverage for new native platform
//! built-ins added after the pack teardown.

use selene_core::{DbString, GraphId, Value};
use selene_gql::{
    BindingTable, BuiltinProcedureRegistry, ProcedureRegistry, Session, StatementOutput,
};
use selene_graph::{SharedGraph, TypedIndexKind};
use selene_profile::{PROFILE_HASH, capabilities};

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn graph(id: u64) -> SharedGraph {
    SharedGraph::new(GraphId::new(id))
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

fn uint_column(table: &BindingTable, name: &str) -> Vec<u64> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::Uint(value)) => *value,
            other => panic!("expected uint in {name}, got {other:?}"),
        })
        .collect()
}

#[test]
fn show_procedures_lists_all_sixty_nine_procedures() {
    let graph = graph(330_001);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let table = execute_rows(&mut session, "SHOW PROCEDURES", &registry);
    let names = string_column(&table, "name");

    assert_eq!(
        table.row_count(),
        69,
        "19 algo procedures + 50 platform built-ins"
    );
    for expected in [
        "selene.property_index_stats",
        "selene.health",
        "selene.feature_status",
        "selene.verify",
        "selene.compaction_stats",
        "selene.create_index",
        "selene.drop_index",
        "selene.vector_search_nodes",
        "selene.vector_search_nodes_batch",
        "selene.vector_score_nodes",
        "selene.vector_score_nodes_batch",
        "selene.vector_score_neighbors",
        "selene.vector_score_neighbors_batch",
        "selene.vector_score_expanded_candidates",
        "selene.vector_score_expanded_candidates_batch",
        "selene.vector_search_nodes_ann",
        "selene.vector_search_nodes_ann_batch",
        "selene.vector_search_expanded_candidates_ann",
        "selene.vector_search_candidate_state_expanded_ann",
        "selene.vector_search_expanded_candidates_ann_batch",
        "selene.vector_score_candidate_state",
        "selene.vector_score_candidate_state_nodes",
        "selene.vector_score_candidate_state_expanded",
        "selene.vector_score_candidate_state_expanded_batch",
        "selene.vector_candidate_states",
        "selene.reachable_nodes",
        "selene.vector_index_stats",
        "selene.text_index_stats",
        "selene.json_contains_nodes",
        "selene.json_path_exists_nodes",
        "selene.json_path_contains_nodes",
        "selene.json_path_value_nodes",
        "selene.json_contains_candidate_nodes",
        "selene.json_path_exists_candidate_nodes",
        "selene.json_path_contains_candidate_nodes",
        "selene.json_path_value_candidate_nodes",
        "selene.rebuild_vector_indexes",
        "selene.rebuild_recommended_vector_indexes",
        "selene.compact",
        "selene.create_vector_index",
        "selene.drop_vector_index",
        "selene.create_text_index",
        "selene.drop_text_index",
        "selene.text_search_nodes",
        "selene.text_score_nodes",
        "selene.text_score_nodes_batch",
        "selene.text_score_candidate_state",
        "selene.text_score_candidate_state_nodes",
        "selene.text_score_candidate_state_expanded_batch",
        "selene.reciprocal_rank_fusion",
        "algo.pagerank",
    ] {
        assert!(
            names.contains(&expected.to_owned()),
            "SHOW PROCEDURES must list {expected}"
        );
    }
    // `pack_history` is NOT relocated — it must not appear in the native registry.
    assert!(
        !names.contains(&"selene.pack_history".to_owned()),
        "pack_history must not be relocated into the native registry"
    );
}

#[test]
fn health_reports_node_and_edge_counts() {
    let graph = graph(330_002);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session
        .execute_source("INSERT (a:N)-[:E]->(b:N)", &registry)
        .expect("seed inserts");

    let table = execute_rows(
        &mut session,
        "CALL selene.health() YIELD graph_id, node_count, edge_count, schema_bound",
        &registry,
    );
    assert_eq!(table.row_count(), 1);
    assert_eq!(uint_column(&table, "node_count"), vec![2]);
    assert_eq!(uint_column(&table, "edge_count"), vec![1]);
    assert_eq!(uint_column(&table, "graph_id"), vec![330_002]);
}

#[test]
fn feature_status_reports_generated_capability_records() {
    let graph = graph(330_003);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let table = execute_rows(
        &mut session,
        "CALL selene.feature_status() YIELD feature_id, status, rationale, feature_name, surface, profile_relation, claim_state, evidence_status, evidence_count, profile_hash",
        &registry,
    );
    let feature_ids = string_column(&table, "feature_id");
    let statuses = string_column(&table, "status");
    let rationales = string_column(&table, "rationale");
    let names = string_column(&table, "feature_name");
    let surfaces = string_column(&table, "surface");
    let relations = string_column(&table, "profile_relation");
    let claims = string_column(&table, "claim_state");
    let evidence_statuses = string_column(&table, "evidence_status");
    let evidence_counts = uint_column(&table, "evidence_count");
    let profile_hashes = string_column(&table, "profile_hash");

    assert_eq!(feature_ids.len(), 208);
    assert_eq!(
        feature_ids,
        capabilities()
            .iter()
            .map(|record| record.id.as_str().to_owned())
            .collect::<Vec<_>>()
    );
    let gp04 = feature_ids
        .iter()
        .position(|value| value == "GP04")
        .expect("GP04 row exists");
    assert_eq!(statuses[gp04], "supported");
    assert_eq!(rationales[gp04], "Named procedure calls");
    assert_eq!(names[gp04], "Named procedure calls");
    assert_eq!(surfaces[gp04], "iso");
    assert_eq!(relations[gp04], "direct");
    assert_eq!(claims[gp04], "implemented_unclaimed");
    assert_eq!(evidence_statuses[gp04], "incomplete");
    assert_eq!(evidence_counts[gp04], 0);

    for (feature_id, expected_name) in [
        ("GQ12", "ORDER BY and page statement: OFFSET clause"),
        ("GQ13", "ORDER BY and page statement: LIMIT clause"),
        ("GQ18", "Scalar subqueries"),
        ("GQ20", "Advanced linear composition with NEXT"),
    ] {
        let index = feature_ids
            .iter()
            .position(|value| value == feature_id)
            .unwrap_or_else(|| panic!("{feature_id} row exists"));
        assert_eq!(statuses[index], "supported");
        assert_eq!(rationales[index], expected_name);
    }

    for feature_id in [
        "GC03", "GE04", "GE05", "GG02", "GG20", "GG21", "GH02", "GV66", "GV67",
    ] {
        let index = feature_ids
            .iter()
            .position(|value| value == feature_id)
            .unwrap_or_else(|| panic!("{feature_id} row exists"));
        assert_eq!(statuses[index], "unsupported");
        assert!(!rationales[index].is_empty());
    }
    for feature_id in ["GS04", "GS05", "GS06"] {
        let index = feature_ids
            .iter()
            .position(|value| value == feature_id)
            .unwrap_or_else(|| panic!("{feature_id} row exists"));
        assert_eq!(statuses[index], "supported");
    }
    let gv65 = feature_ids
        .iter()
        .position(|value| value == "GV65")
        .expect("GV65 row exists");
    assert_eq!(statuses[gv65], "referenced");
    assert_eq!(relations[gv65], "implied");

    for feature_id in ["GG02", "GH02", "GV66"] {
        let index = feature_ids
            .iter()
            .position(|value| value == feature_id)
            .unwrap_or_else(|| panic!("{feature_id} row exists"));
        assert_eq!(statuses[index], "unsupported");
        assert!(!rationales[index].is_empty());
    }
    let gc04 = feature_ids
        .iter()
        .position(|value| value == "GC04")
        .unwrap();
    assert_eq!(relations[gc04], "direct");
    assert_eq!(statuses[gc04], "supported");

    let im_json = feature_ids
        .iter()
        .position(|value| value == "IM_JSON")
        .expect("IM_JSON row exists");
    assert_eq!(statuses[im_json], "supported");
    assert_eq!(surfaces[im_json], "extension");
    assert_eq!(relations[im_json], "extension");
    assert_eq!(claims[im_json], "not_applicable");

    assert!(profile_hashes.iter().all(|hash| hash == PROFILE_HASH));
    assert!(
        rationales
            .iter()
            .chain(names.iter())
            .all(|value| !value.contains("crates/") && !value.contains("tests/")),
        "procedure output must not expose internal evidence paths"
    );
}

#[test]
fn verify_reports_ok_for_a_consistent_graph() {
    let graph = graph(330_004);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session
        .execute_source("INSERT (a:N)-[:E]->(b:N)", &registry)
        .expect("seed inserts");

    let table = execute_rows(
        &mut session,
        "CALL selene.verify() YIELD check, status, detail",
        &registry,
    );
    let statuses = string_column(&table, "status");
    assert!(
        !statuses.is_empty(),
        "verify emits at least the shallow checks"
    );
    assert!(
        statuses.iter().all(|status| status == "ok"),
        "a freshly inserted graph must verify clean: {statuses:?}"
    );
}

#[test]
fn verify_reports_ok_for_mixed_parallel_edges_and_both_loop_kinds() {
    use selene_core::{
        EdgeDirectionality::{Directed, Undirected},
        LabelSet, PropertyMap,
    };
    let graph = graph(330_007);
    let mut tx = graph.begin_write();
    {
        let mut m = tx.mutator();
        let a = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        let b = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        for (first, second, kind) in [
            (a, b, Directed),
            (b, a, Directed),
            (a, b, Directed),
            (a, b, Undirected),
            (b, a, Undirected),
            (a, a, Directed),
            (a, a, Undirected),
        ] {
            m.create_mixed_edge(db_string("E"), first, second, kind, PropertyMap::new())
                .unwrap();
        }
    }
    tx.commit().unwrap();
    let before = graph.read();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    for (source, expected_rows) in [
        ("CALL selene.verify() YIELD check, status, detail", 4),
        ("CALL selene.verify(true) YIELD check, status, detail", 6),
    ] {
        let table = execute_rows(&mut session, source, &registry);
        assert_eq!(table.row_count(), expected_rows);
        assert!(
            string_column(&table, "status")
                .iter()
                .all(|status| status == "ok"),
            "healthy mixed graph failed verify: {:?}",
            string_column(&table, "detail")
        );
    }
    assert_eq!(graph.read().meta.generation, before.meta.generation);
    assert_eq!(graph.read().edge_count(), 7);
}

/// Deep verify re-derives each row's expected key through the same float key
/// constructors the index used, so collapsing `-0.0` onto `+0.0` has to leave
/// both sides agreeing. A collapse applied on only one side would surface here
/// as a corrupt index on data that is in fact consistent.
#[test]
fn verify_reports_ok_for_a_float_index_holding_both_signed_zeros() {
    let graph = graph(330_006);
    let registry = BuiltinProcedureRegistry::new();
    {
        let mut session = Session::new(&graph);
        session
            .execute_source(
                "INSERT (:Reading { level: -0.0e0 }) INSERT (:Reading { level: 0.0e0 })",
                &registry,
            )
            .expect("seed inserts");
    }
    graph
        .create_property_index(
            db_string("Reading"),
            db_string("level"),
            TypedIndexKind::F64,
        )
        .expect("float index builds over both signed zeros");

    let mut session = Session::new(&graph);
    let table = execute_rows(
        &mut session,
        "CALL selene.verify(true) YIELD check, status, detail",
        &registry,
    );
    let statuses = string_column(&table, "status");
    assert!(
        statuses.iter().all(|status| status == "ok"),
        "a float index keying both signed zeros is consistent: {statuses:?}"
    );
}

#[test]
fn verify_deep_runs_additional_checks() {
    let graph = graph(330_005);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session
        .execute_source("INSERT (a:N)-[:E]->(b:N)", &registry)
        .expect("seed inserts");

    let shallow = execute_rows(
        &mut session,
        "CALL selene.verify(false) YIELD check",
        &registry,
    );
    let deep = execute_rows(
        &mut session,
        "CALL selene.verify(true) YIELD check",
        &registry,
    );
    assert!(
        deep.row_count() > shallow.row_count(),
        "deep verify runs more checks than shallow"
    );
}

#[test]
fn create_index_then_show_indexes_confirms_funnel_commit() {
    let graph = graph(330_006);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    // The index DDL is performed by the mutation-tier built-in through the
    // `Mutator` funnel; SHOW INDEXES reads the COMMITTED graph's property index,
    // proving the create routed through the funnel and committed.
    session
        .execute_source(
            "CALL selene.create_index('Sensor', 'timestamp', 'i64')",
            &registry,
        )
        .expect("index creation executes");

    let table = execute_rows(&mut session, "SHOW INDEXES", &registry);
    assert_eq!(string_column(&table, "label"), vec!["Sensor"]);
    assert_eq!(string_column(&table, "property"), vec!["timestamp"]);
    assert_eq!(string_column(&table, "kind"), vec!["i64"]);
}

#[test]
fn create_index_accepts_boolean_kind_alias() {
    let graph = graph(330_026);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL selene.create_index('Sensor', 'active', 'boolean')",
            &registry,
        )
        .expect("boolean index creation executes");

    let table = execute_rows(&mut session, "SHOW INDEXES", &registry);
    assert_eq!(string_column(&table, "label"), vec!["Sensor"]);
    assert_eq!(string_column(&table, "property"), vec!["active"]);
    assert_eq!(string_column(&table, "kind"), vec!["bool"]);
}

#[test]
fn create_index_accepts_uint64_kind_alias() {
    let graph = graph(330_027);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL selene.create_index('Sensor', 'count', 'uint64')",
            &registry,
        )
        .expect("uint64 index creation executes");

    let table = execute_rows(&mut session, "SHOW INDEXES", &registry);
    assert_eq!(string_column(&table, "label"), vec!["Sensor"]);
    assert_eq!(string_column(&table, "property"), vec!["count"]);
    assert_eq!(string_column(&table, "kind"), vec!["u64"]);
}

#[test]
fn create_index_accepts_numeric_kind_aliases() {
    let graph = graph(330_028);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    for (property, kind) in [
        ("signed", "int128"),
        ("unsigned", "uint128"),
        ("amount", "decimal"),
        ("score", "float32"),
    ] {
        session
            .execute_source(
                &format!("CALL selene.create_index('Metric', '{property}', '{kind}')"),
                &registry,
            )
            .expect("exact numeric index creation executes");
    }

    let table = execute_rows(&mut session, "SHOW INDEXES", &registry);
    let properties = string_column(&table, "property");
    let kinds = string_column(&table, "kind");
    assert!(properties.contains(&"signed".to_owned()));
    assert!(properties.contains(&"unsigned".to_owned()));
    assert!(properties.contains(&"amount".to_owned()));
    assert!(properties.contains(&"score".to_owned()));
    assert!(kinds.contains(&"i128".to_owned()));
    assert!(kinds.contains(&"u128".to_owned()));
    assert!(kinds.contains(&"decimal".to_owned()));
    assert!(kinds.contains(&"f32".to_owned()));
}

#[test]
fn create_index_accepts_temporal_time_kind_aliases() {
    let graph = graph(330_029);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    for (property, kind) in [
        ("occurred_at", "zoned_datetime"),
        ("wall_time", "local_time"),
        ("clock_time", "zoned_time"),
        ("span", "duration"),
    ] {
        session
            .execute_source(
                &format!("CALL selene.create_index('Event', '{property}', '{kind}')"),
                &registry,
            )
            .expect("temporal index creation executes");
    }

    let table = execute_rows(&mut session, "SHOW INDEXES", &registry);
    let properties = string_column(&table, "property");
    let kinds = string_column(&table, "kind");
    assert!(properties.contains(&"occurred_at".to_owned()));
    assert!(properties.contains(&"wall_time".to_owned()));
    assert!(properties.contains(&"clock_time".to_owned()));
    assert!(properties.contains(&"span".to_owned()));
    assert!(kinds.contains(&"zoned_datetime".to_owned()));
    assert!(kinds.contains(&"local_time".to_owned()));
    assert!(kinds.contains(&"zoned_time".to_owned()));
    assert!(kinds.contains(&"duration".to_owned()));
}

#[test]
fn drop_index_removes_the_index_through_the_funnel() {
    let graph = graph(330_007);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL selene.create_index('Sensor', 'timestamp', 'i64')",
            &registry,
        )
        .expect("index creation executes");
    session
        .execute_source("CALL selene.drop_index('Sensor', 'timestamp')", &registry)
        .expect("index drop executes");

    let table = execute_rows(&mut session, "SHOW INDEXES", &registry);
    assert_eq!(table.row_count(), 0, "dropped index must not be listed");
}

#[test]
fn create_index_duplicate_is_an_invalid_argument_error() {
    let graph = graph(330_008);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session
        .execute_source(
            "CALL selene.create_index('Sensor', 'timestamp', 'i64')",
            &registry,
        )
        .expect("first index creation executes");
    let err = session
        .execute_source(
            "CALL selene.create_index('Sensor', 'timestamp', 'i64')",
            &registry,
        )
        .expect_err("duplicate index must error");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("already exists"),
        "duplicate index error should mention existence, got: {rendered}"
    );
}

#[test]
fn create_index_unknown_kind_is_rejected() {
    let graph = graph(330_009);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let err = session
        .execute_source(
            "CALL selene.create_index('Sensor', 'timestamp', 'not_a_kind')",
            &registry,
        )
        .expect_err("unknown index kind must error");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("unknown index kind"),
        "error should name the bad kind, got: {rendered}"
    );
}

#[test]
fn unknown_selene_builtin_is_rejected_at_plan_time() {
    let graph = graph(330_010);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let err = session
        .execute_source("CALL selene.does_not_exist()", &registry)
        .expect_err("unknown built-in must be rejected");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("does_not_exist") || rendered.to_lowercase().contains("procedure"),
        "unknown built-in error should name the procedure, got: {rendered}"
    );
}
