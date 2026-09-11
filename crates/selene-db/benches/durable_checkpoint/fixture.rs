use selene_db::*;
pub const GRAPHS: usize = 3;
pub fn path(graph: usize) -> ObjectPath {
    ObjectPath::regular("selene", "bench", format!("graph_{graph}")).unwrap()
}
fn name(s: &str) -> PathSegment {
    PathSegment::regular(s).unwrap()
}

pub fn fixture(rows: usize, indexes: bool) -> (tempfile::TempDir, Database) {
    fixture_graphs(rows, indexes, GRAPHS)
}
pub fn fixture_graphs(rows: usize, indexes: bool, graphs: usize) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path()).unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "bench").unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    let ty = ObjectPath::regular("selene", "bench", "Shape").unwrap();
    let node = NodeTypeDefinition::new(name("Doc"), vec![name("Doc")])
        .unwrap()
        .with_property(
            PropertyDefinition::new(name("id"), Type::INT64.with_nullability(false))
                .unwrap()
                .unique(),
        )
        .with_property(
            PropertyDefinition::new(name("n"), Type::INT64)
                .unwrap()
                .with_default(Value::Int(0))
                .unwrap(),
        )
        .with_property(PropertyDefinition::new(name("text"), Type::STRING).unwrap())
        .with_property(PropertyDefinition::new(name("v"), Type::VECTOR).unwrap())
        .with_property(PropertyDefinition::new(name("payload"), Type::JSON).unwrap());
    db.catalog()
        .create_graph_type(
            &ty,
            GraphTypeDefinition::builder()
                .with_node_type(node)
                .build()
                .unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    for graph in 0..graphs {
        db.catalog()
            .create_graph(&path(graph), Some(&ty), CreatePolicy::Strict)
            .unwrap();
        let s = db.session(&path(graph)).unwrap();
        let input = (0..rows).map(|id| format!("(:Doc {{id: {id}, text: 'alpha memory {graph} {id}', v: CAST([1, {}] AS VECTOR), payload: CAST('{{\"graph\":{graph},\"id\":{id}}}' AS JSON)}})", id as f64 / rows as f64)).collect::<Vec<_>>().join(",");
        s.execute(&format!("INSERT {input}")).unwrap();
        if indexes {
            for query in [
                "CREATE INDEX by_id ON :Doc(id)",
                "CREATE INDEX pair ON :Doc(id, n)",
                "CALL selene.create_text_index('Doc', 'text')",
                "CALL selene.create_vector_index('Doc', 'v', 2, 'hnsw', NULL, 'cosine', 8, 32)",
            ] {
                s.execute(query).unwrap();
            }
        }
    }
    (dir, db)
}

pub fn verify(db: &Database, rows: usize, sum: i64) {
    verify_graphs(db, rows, sum, GRAPHS);
}
pub fn verify_graphs(db: &Database, rows: usize, sum: i64, graphs: usize) {
    let mut total = 0;
    for graph in 0..graphs {
        let s = db.session(&path(graph)).unwrap();
        let ExecutionOutcome::Rows { result, .. } = s
            .execute("MATCH (n:Doc) RETURN n.id, n.n ORDER BY n.id")
            .unwrap()
        else {
            panic!("rows");
        };
        assert_eq!(result.rows().len(), rows);
        for (i, row) in result.rows().iter().enumerate() {
            assert_eq!(row.values()[0], Value::Int(i as i64));
            let Value::Int(n) = row.values()[1] else {
                panic!("int");
            };
            total += n;
        }
    }
    assert_eq!(total, sum);
}
pub fn suffix(db: &Database, count: usize) {
    for i in 0..count {
        let s = db.session(&path(i % GRAPHS)).unwrap();
        s.execute("MATCH (n:Doc) WHERE n.id = 0 SET n.n = n.n + 1")
            .unwrap();
    }
}
pub fn storage(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum()
}
