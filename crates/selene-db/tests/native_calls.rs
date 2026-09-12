//! F04-PR06: the closed native inventory through facade selection and durability.

use selene_db::*;
use std::error::Error as _;

fn graph(db: &Database, name: &str) -> (ObjectPath, Session) {
    let path = ObjectPath::regular("selene", "native", name).unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "native").unwrap(),
            CreatePolicy::IfNotExists,
        )
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    (path, session)
}

fn rows(session: &Session, source: &str) -> Vec<Vec<Value>> {
    let ExecutionOutcome::Rows { result, .. } = session
        .execute(source)
        .unwrap_or_else(|error| panic!("{source}: {error:?}"))
    else {
        panic!("rows: {source}")
    };
    result
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

const BUILD: &str = "CALL algo.projection_build('p', ['N'], ['E'], NULL)";

#[test]
fn all_nineteen_algorithms_execute_through_catalog_selected_facade() {
    let db = Database::builder().build();
    let (_, session) = graph(&db, "dag");
    session
        .execute("INSERT (a:N {k: 1})-[:E]->(b:N {k: 2}), (b)-[:E]->(c:N {k: 3})")
        .unwrap();
    let before = db.catalog().snapshot();
    session.execute(BUILD).unwrap();
    let declarations = before.native_procedures();
    assert_eq!(
        declarations
            .iter()
            .filter(|d| d.name.display().starts_with("algo."))
            .count(),
        19
    );
    for (source, expected) in [
        ("CALL algo.projection_get('p') YIELD node_count", 1),
        ("CALL algo.projection_list() YIELD name", 1),
        (
            "CALL algo.pagerank('p', NULL, NULL, NULL, NULL) YIELD node_id, score",
            3,
        ),
        (
            "CALL algo.betweenness('p', NULL, NULL) YIELD node_id, score",
            3,
        ),
        (
            "CALL algo.label_propagation('p', NULL) YIELD node_id, community",
            3,
        ),
        (
            "CALL algo.louvain('p', NULL) YIELD node_id, community, level",
            3,
        ),
        (
            "CALL algo.triangle_count('p', NULL) YIELD node_id, triangle_count",
            3,
        ),
        ("CALL algo.wcc('p') YIELD node_id, component_id", 3),
        ("CALL algo.scc('p') YIELD node_id, component_id", 3),
        ("CALL algo.wcc_count('p') YIELD count", 1),
        ("CALL algo.scc_count('p') YIELD count", 1),
        (
            "CALL algo.topological_sort('p') YIELD node_id, topo_position",
            3,
        ),
        ("CALL algo.articulation_points('p') YIELD node_id", 1),
        ("CALL algo.bridges('p') YIELD from_node, to_node", 2),
        (
            "MATCH (a:N {k: 1}), (b:N {k: 3}) CALL algo.dijkstra('p', a, b) YIELD path AS route RETURN route",
            1,
        ),
        (
            "MATCH (a:N {k: 1}) CALL algo.sssp('p', a) YIELD target_node, cost RETURN target_node",
            3,
        ),
    ] {
        assert_eq!(rows(&session, source).len(), expected, "{source}");
    }
    assert!(
        !rows(
            &session,
            "CALL algo.apsp('p', 10, NULL) YIELD source_node, target_node, cost"
        )
        .is_empty()
    );
    session.execute("CALL algo.projection_drop('p')").unwrap();
    assert!(rows(&session, "CALL algo.projection_list() YIELD name").is_empty());
    assert!(
        before.shares_state_with(&db.catalog().snapshot()),
        "query algorithms cannot publish data/catalog writes"
    );
}

#[test]
fn graph_selection_and_deleted_stable_ids_survive_reopen_with_declarations() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path()).unwrap();
    let (path, first) = graph(&db, "red");
    let (_, second) = graph(&db, "blue");
    first
        .execute("INSERT (:N {k: 0}), (a:N {k: 1})-[:E]->(b:N {k: 2})")
        .unwrap();
    second.execute("INSERT (:N {k: 11}), (:N {k: 12})").unwrap();
    first.execute(BUILD).unwrap();
    first
        .execute(&format!("USE /native/blue {BUILD} RETURN 1 AS built"))
        .unwrap();
    first.execute("MATCH (n:N {k: 0}) DELETE n").unwrap();
    let expected = rows(&first, "MATCH (n:N) RETURN n ORDER BY n.k");
    assert_eq!(
        rows(
            &first,
            "CALL algo.wcc('p') YIELD node_id RETURN node_id ORDER BY node_id.k"
        ),
        expected
    );
    assert_eq!(
        rows(
            &first,
            "USE /native/blue CALL algo.wcc('p') YIELD node_id RETURN node_id.k AS k ORDER BY k"
        ),
        vec![vec![Value::Int(11)], vec![Value::Int(12)]]
    );
    let declarations = db.catalog().snapshot().native_procedures();
    db.checkpoint().unwrap();
    drop(first);
    drop(second);
    drop(db);
    let db = Database::open(directory.path()).unwrap();
    assert_eq!(db.catalog().snapshot().native_procedures(), declarations);
    let session = db.session(&path).unwrap();
    // Executable declarations reattach; ephemeral CSR caches deliberately do not persist.
    assert!(rows(&session, "CALL algo.projection_list() YIELD name").is_empty());
    session.execute(BUILD).unwrap();
    assert_eq!(
        rows(
            &session,
            "CALL algo.wcc('p') YIELD node_id RETURN node_id.k AS k ORDER BY k"
        ),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
}

#[test]
fn native_error_keeps_cause_and_aborts_prior_staged_writes() {
    let db = Database::builder().build();
    let (path, session) = graph(&db, "errors");
    let observer = db.session(&path).unwrap();
    let before = db.catalog().snapshot();
    session.execute("START TRANSACTION").unwrap();
    session.execute("INSERT (:N)").unwrap();
    let error = session
        .execute("CALL algo.wcc('missing') YIELD node_id")
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22G03");
    let mut cause = error.source();
    let mut depth = 0;
    while let Some(source) = cause {
        depth += 1;
        cause = source.source();
    }
    assert!(
        depth >= 3,
        "facade -> executor -> procedure -> native cause"
    );
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::Failed
    );
    assert!(session.execute("COMMIT").is_err());
    assert!(rows(&observer, "MATCH (n:N) RETURN n").is_empty());
    assert!(before.shares_state_with(&db.catalog().snapshot()));
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::RolledBack
    );
}
