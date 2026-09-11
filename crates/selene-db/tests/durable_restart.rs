//! Public facade-only format-2 consumer restart and eager index evidence.

use selene_db::{
    CreatePolicy, Database, DatabaseBuilder, DatabaseConfig, EdgeTypeDefinition, ErrorKind,
    ExecutionOutcome, GraphTypeDefinition, NodeTypeDefinition, ObjectPath, PathSegment,
    PropertyDefinition, SchemaPath, Session, StorageErrorKind, TransactionAccessMode, Type, Value,
};

fn path(schema: &str, graph: &str) -> ObjectPath {
    ObjectPath::regular("selene", schema, graph).unwrap()
}
fn rows(session: &Session, query: &str) -> Vec<Vec<Value>> {
    let ExecutionOutcome::Rows { result, .. } = session.execute(query).unwrap() else {
        panic!("rows: {query}");
    };
    result
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}
fn create_graph(database: &Database, schema: &str, name: &str) -> Session {
    database
        .catalog()
        .create_schema(
            &SchemaPath::regular("selene", schema).unwrap(),
            CreatePolicy::IfNotExists,
        )
        .unwrap();
    let path = path(schema, name);
    database
        .catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    database.session(&path).unwrap()
}

fn name(text: &str) -> PathSegment {
    PathSegment::regular(text).unwrap()
}
fn literal(session: &Session, source: &str) -> Value {
    rows(session, &format!("RETURN {source}"))[0][0].clone()
}
fn item_schema(session: &Session) -> GraphTypeDefinition {
    let Value::String(flag) = literal(session, "'flag'") else {
        panic!("string");
    };
    let node = NodeTypeDefinition::new(name("Item"), vec![name("Item")])
        .unwrap()
        .with_property(
            PropertyDefinition::new(name("serial"), Type::STRING)
                .unwrap()
                .unique(),
        )
        .with_property(PropertyDefinition::new(name("rank"), Type::INT64).unwrap())
        .with_property(PropertyDefinition::new(name("text"), Type::STRING).unwrap())
        .with_property(
            PropertyDefinition::new(name("embedding"), Type::VECTOR)
                .unwrap()
                .with_default(literal(session, "CAST([1, 0] AS VECTOR)"))
                .unwrap(),
        )
        .with_property(
            PropertyDefinition::new(name("payload"), Type::JSON)
                .unwrap()
                .with_default(literal(session, "CAST('{\"kind\":\"episode\"}' AS JSON)"))
                .unwrap(),
        )
        .with_property(
            PropertyDefinition::new(name("tags"), Type::list(Type::STRING, None).unwrap())
                .unwrap()
                .with_default(literal(session, "['x']"))
                .unwrap(),
        )
        .with_property(
            PropertyDefinition::new(
                name("config"),
                Type::record([(flag, Type::BOOLEAN)]).unwrap(),
            )
            .unwrap()
            .with_default(literal(session, "RECORD{flag: true}"))
            .unwrap(),
        );
    GraphTypeDefinition::builder()
        .with_node_type(node)
        .with_edge_type(
            EdgeTypeDefinition::new(name("LINK"), name("LINK"), name("Item"), name("Item"))
                .with_property(PropertyDefinition::new(name("weight"), Type::INT64).unwrap()),
        )
        .build()
        .unwrap()
}

#[test]
fn consumer_reopens_exact_catalog_native_values_mixed_edges_and_fresh_references() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::create(directory.path()).unwrap();
    let setup = create_graph(&database, "memory", "episodes");
    let ty = path("memory", "ItemShape");
    database
        .catalog()
        .create_graph_type(&ty, item_schema(&setup), CreatePolicy::Strict)
        .unwrap();
    drop(setup);
    database
        .catalog()
        .create_graph(
            &path("memory", "episodes"),
            Some(&ty),
            CreatePolicy::OrReplace,
        )
        .unwrap();
    let session = database.session(&path("memory", "episodes")).unwrap();
    let other = create_graph(&database, "archive", "history");
    other
        .execute("CREATE GRAPH TYPE /archive/Shape { NODE TYPE Entry () }")
        .unwrap();
    other
        .execute("CREATE GRAPH /archive/bound_graph TYPED /archive/Shape")
        .unwrap();
    let typed = database.session(&path("archive", "bound_graph")).unwrap();
    typed.execute("CREATE NODE TYPE :Unused ()").unwrap();
    typed.execute("INSERT (:Entry)").unwrap();
    assert_eq!(
        typed
            .execute("INSERT (:Unused)")
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "G2000"
    );
    session
        .execute("CREATE INDEX item_serial ON :Item(serial)")
        .unwrap();
    session
        .execute("CREATE INDEX item_rank ON :Item(rank)")
        .unwrap();
    session
        .execute("CREATE INDEX item_pair ON :Item(serial, rank)")
        .unwrap();
    session
        .execute("CALL selene.create_text_index('Item', 'text')")
        .unwrap();
    session.execute("CALL selene.create_vector_index('Item', 'embedding', 2, 'hnsw', NULL, 'cosine', 8, 32)").unwrap();
    session.execute("INSERT (:Item {serial: 'A', rank: 1, text: 'alpha memory'})-[:LINK {weight: 5}]->(:Item {serial: 'B', rank: 2, text: 'beta memory', embedding: CAST([0, 1] AS VECTOR)})").unwrap();
    session.execute("MATCH (a:Item), (b:Item) WHERE a.serial = 'A' AND b.serial = 'B' INSERT (a)~[:LINK {weight: 7}]~(b)").unwrap();
    session
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    session
        .execute("INSERT (:Item {serial: 'C', rank: 3, text: 'gamma'})")
        .unwrap();
    session
        .execute("MATCH (n:Item) WHERE n.serial = 'A' SET n.rank = 10")
        .unwrap();
    session.commit_transaction().unwrap();
    session
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    session
        .execute("INSERT (:Item {serial: 'rolled-back', rank: 99})")
        .unwrap();
    session.rollback_transaction().unwrap();
    other.execute("INSERT (:History {n: 41})").unwrap();
    // A later committed graph generation captures already-burned element floors.
    session
        .execute("MATCH (n:Item) WHERE n.serial = 'B' SET n.rank = 20")
        .unwrap();
    let old_id = database.id();
    let old_node = session.node_reference(selene_db::NodeId::new(1)).unwrap();
    let old_edge = session.edge_reference(selene_db::EdgeId::new(1)).unwrap();
    let old_graph = session.graph_reference().unwrap();
    let checkpoint = database.checkpoint().unwrap();
    session
        .execute("MATCH (n:Item) WHERE n.serial = 'C' DELETE n")
        .unwrap();
    session
        .execute("CALL selene.drop_text_index('Item', 'text')")
        .unwrap();
    session
        .execute("CALL selene.create_text_index('Item', 'text')")
        .unwrap();
    session
        .execute("MATCH (n:Item) WHERE n.serial = 'B' SET n.text = 'delta memory'")
        .unwrap();
    let queries = [
        "MATCH (n:Item) RETURN n.serial, n.rank, json_stringify(n.payload), n.tags, n.config ORDER BY n.serial",
        "MATCH ()-[e:LINK]->() RETURN e.weight ORDER BY e.weight",
        "MATCH ()~[e:LINK]~() RETURN e.weight ORDER BY e.weight",
        "CALL selene.vector_search_nodes('Item', 'embedding', CAST([1, 0] AS VECTOR), 2, 'cosine') YIELD node_id, distance RETURN distance",
        "CALL selene.vector_search_nodes_ann('Item', 'embedding', CAST([1, 0] AS VECTOR), 2, 'cosine', 32) YIELD node_id, distance RETURN distance",
        "CALL selene.text_search_nodes('Item', 'text', 'alpha', 2) YIELD node_id, score RETURN score",
    ];
    let expected: Vec<_> = queries.iter().map(|query| rows(&session, query)).collect();
    assert_eq!(expected[0].len(), 2);
    assert_eq!(expected[0][0][1], Value::Int(10));
    assert_eq!(expected[0][1][1], Value::Int(20));
    // Independently stated expected defaults, not only live/reopen agreement.
    assert_eq!(
        expected[0][0][2],
        literal(&other, "'{\"kind\":\"episode\"}'")
    );
    assert_eq!(expected[0][0][3], literal(&other, "['x']"));
    assert_eq!(expected[0][0][4], literal(&other, "RECORD{flag: true}"));
    assert_eq!(
        expected[3],
        vec![vec![Value::Float(0.0)], vec![Value::Float(1.0)]]
    );
    assert_eq!(expected[3], expected[4]);
    let catalog = database.catalog().snapshot().logical_catalog().unwrap();
    let declarations = database
        .catalog()
        .snapshot()
        .declarations(&path("memory", "episodes"))
        .unwrap();
    let durable = database.durable_status().unwrap();
    assert!(durable.position.sequence > checkpoint.position.sequence);
    drop(typed);
    drop(other);
    drop(session);
    drop(database);
    let database = Database::open(directory.path()).unwrap();
    assert_ne!(database.id(), old_id);
    assert_eq!(database.durable_status().unwrap(), durable);
    assert_eq!(
        database.catalog().snapshot().logical_catalog().unwrap(),
        catalog
    );
    assert_eq!(
        database
            .catalog()
            .snapshot()
            .declarations(&path("memory", "episodes"))
            .unwrap(),
        declarations
    );
    assert!(database.recovery_info().unwrap().rebuilt_indexes >= 5);
    let session = database.session(&path("memory", "episodes")).unwrap();
    for (query, expected) in queries.iter().zip(expected) {
        assert_eq!(rows(&session, query), expected, "{query}");
    }
    assert_eq!(
        session.resolve_node_reference(old_node).unwrap_err().kind(),
        ErrorKind::RuntimeInvalidReference
    );
    assert_eq!(
        session.resolve_edge_reference(old_edge).unwrap_err().kind(),
        ErrorKind::RuntimeInvalidReference
    );
    assert_eq!(
        database
            .resolve_graph_reference(old_graph)
            .unwrap_err()
            .kind(),
        ErrorKind::RuntimeInvalidReference
    );
    let fresh = session.node_reference(selene_db::NodeId::new(1)).unwrap();
    assert_eq!(session.resolve_node_reference(fresh).unwrap().get(), 1);
    assert_eq!(
        session
            .execute("INSERT (:Item {serial: 'A', rank: 30})")
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "G2000"
    );
    let typed = database.session(&path("archive", "bound_graph")).unwrap();
    assert_eq!(
        typed
            .execute("INSERT (:Unused)")
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "G2000"
    );
    assert_eq!(
        rows(
            &database.session(&path("archive", "history")).unwrap(),
            "MATCH (n) RETURN n.n"
        ),
        vec![vec![Value::Int(41)]]
    );
    session
        .execute("INSERT (:Item {serial: 'D', rank: 40})")
        .unwrap();
    assert!(session.node_reference(selene_db::NodeId::new(3)).is_err());
    assert!(session.node_reference(selene_db::NodeId::new(4)).is_err());
    assert!(session.node_reference(selene_db::NodeId::new(5)).is_err()); // rejected duplicate burned it
    assert!(session.node_reference(selene_db::NodeId::new(6)).is_ok());
    drop(typed);
    drop(session);
    drop(database);
    let database = Database::open(directory.path()).unwrap();
    let session = database.session(&path("memory", "episodes")).unwrap();
    assert_eq!(
        rows(
            &session,
            "MATCH (n:Item) WHERE n.serial = 'D' RETURN n.rank"
        ),
        vec![vec![Value::Int(40)]]
    );
    database.checkpoint().unwrap();
}

#[test]
fn memory_build_remains_infallible_and_missing_open_never_initializes() {
    let database = DatabaseBuilder::from_config(DatabaseConfig::default()).build();
    assert!(database.durable_status().is_none());
    assert_eq!(
        database.checkpoint().unwrap_err().kind,
        StorageErrorKind::InMemory
    );
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        Database::open(directory.path()).err().unwrap().kind,
        StorageErrorKind::NotInitialized
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    let database = Database::create(directory.path()).unwrap();
    drop(database);
    let current = std::fs::read(directory.path().join("CURRENT")).unwrap();
    assert!(Database::create(directory.path()).is_err());
    assert_eq!(
        std::fs::read(directory.path().join("CURRENT")).unwrap(),
        current
    );
}
