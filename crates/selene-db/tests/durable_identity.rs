//! Stable published identity floors and retained-directory public ownership.
use selene_db::*;

fn object(name: &str) -> ObjectPath {
    ObjectPath::regular("selene", "memory", name).unwrap()
}
fn schema() -> SchemaPath {
    SchemaPath::regular("selene", "memory").unwrap()
}

#[test]
fn consumer_process_restart_witness() {
    const KEY: &str = "SELENE_PR05_RESTART_WITNESS_DIRECTORY";
    if let Some(path) = std::env::var_os(KEY) {
        let db = Database::open(path).unwrap();
        let s = db.session(&object("restart")).unwrap();
        assert_eq!(
            s.execute("MATCH (n:Item) RETURN n").unwrap().row_count(),
            Some(2)
        );
        s.execute("INSERT (:Item {n: 3})").unwrap();
        db.checkpoint().unwrap();
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path()).unwrap();
    db.catalog()
        .create_schema(&schema(), CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&object("restart"), None, CreatePolicy::Strict)
        .unwrap();
    let s = db.session(&object("restart")).unwrap();
    s.execute("INSERT (:Item {n: 1})").unwrap();
    db.checkpoint().unwrap();
    s.execute("INSERT (:Item {n: 2})").unwrap();
    let identity = db.durable_status().unwrap().position;
    drop(s);
    drop(db);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "consumer_process_restart_witness", "--nocapture"])
        .env(KEY, dir.path())
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "child stdout={} stderr={}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let db = Database::open(dir.path()).unwrap();
    let after = db.durable_status().unwrap().position;
    assert_eq!((identity.store, identity.epoch), (after.store, after.epoch));
    assert_eq!(after.sequence, identity.sequence + 1);
    assert_eq!(
        db.session(&object("restart"))
            .unwrap()
            .execute("MATCH (n:Item) RETURN n")
            .unwrap()
            .row_count(),
        Some(3)
    );
}
fn native() -> DeclarationDefinition {
    DeclarationDefinition::Native(NativeDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Inactive),
        binding: NativeBinding::Projection(NativeProjection {
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        }),
    })
}
fn definition() -> GraphTypeDefinition {
    let name = PathSegment::regular("Item").unwrap();
    GraphTypeDefinition::builder()
        .with_node_type(NodeTypeDefinition::new(name.clone(), vec![name]).unwrap())
        .build()
        .unwrap()
}

#[test]
fn deleted_catalog_and_element_ids_stay_consumed_after_checkpoint_and_wal_only_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path()).unwrap();
    db.catalog()
        .create_schema(&schema(), CreatePolicy::Strict)
        .unwrap();
    let mut graph_floor = 0;
    let mut type_floor = 0;
    let mut native_floor = 0;
    let mut schema_floor = 0;
    for checkpoint in [true, false, true] {
        let schema = SchemaPath::regular("selene", "deleted").unwrap();
        db.catalog()
            .create_schema(&schema, CreatePolicy::Strict)
            .unwrap();
        let id = db
            .catalog()
            .snapshot()
            .resolve_schema(&schema)
            .unwrap()
            .id
            .get();
        assert!(id > schema_floor);
        schema_floor = id;
        db.catalog()
            .drop_schema(&schema, DropPolicy::Strict)
            .unwrap();
        db.catalog()
            .create_graph_type(&object("Shape"), definition(), CreatePolicy::Strict)
            .unwrap();
        let id = db
            .catalog()
            .snapshot()
            .resolve_graph_type(&object("Shape"))
            .unwrap()
            .id
            .get();
        assert!(id > type_floor);
        type_floor = id;
        db.catalog()
            .create_graph(&object("g"), None, CreatePolicy::Strict)
            .unwrap();
        let id = db
            .catalog()
            .snapshot()
            .resolve_graph(&object("g"))
            .unwrap()
            .id
            .get();
        assert!(id > graph_floor);
        graph_floor = id;
        let name = PathSegment::regular("inactive").unwrap();
        db.catalog()
            .declare(&object("g"), &name, native(), CreatePolicy::Strict)
            .unwrap();
        let DeclarationId::Native(id) =
            db.catalog().snapshot().declarations(&object("g")).unwrap()[0].id
        else {
            panic!("native");
        };
        assert!(id > native_floor);
        native_floor = id;
        db.catalog()
            .drop_declaration(&object("g"), &name, DropPolicy::Strict)
            .unwrap();
        db.catalog()
            .drop_graph(&object("g"), DropPolicy::Strict)
            .unwrap();
        db.catalog()
            .drop_graph_type(&object("Shape"), DropPolicy::Strict)
            .unwrap();
        if checkpoint {
            db.checkpoint().unwrap();
        }
        drop(db);
        db = Database::open(dir.path()).unwrap();
    }
    db.catalog()
        .create_graph(&object("elements"), None, CreatePolicy::Strict)
        .unwrap();
    let mut node_floor = 0;
    let mut edge_floor = 0;
    for checkpoint in [true, false, true] {
        let s = db.session(&object("elements")).unwrap();
        s.execute("INSERT (:Item)-[:LINK]->(:Item)").unwrap();
        let ExecutionOutcome::Rows { result, .. } =
            s.execute("MATCH (n)-[e]->() RETURN n, e").unwrap()
        else {
            panic!("rows");
        };
        let [Value::NodeRef(n), Value::EdgeRef(e)] = result.rows()[0].values() else {
            panic!("refs");
        };
        assert!(n.node_id().get() > node_floor);
        node_floor = n.node_id().get();
        assert!(e.edge_id().get() > edge_floor);
        edge_floor = e.edge_id().get();
        s.execute("MATCH ()-[e]->() DELETE e").unwrap();
        s.execute("MATCH (n) DELETE n").unwrap();
        if checkpoint {
            db.checkpoint().unwrap();
        }
        drop(s);
        drop(db);
        db = Database::open(dir.path()).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn caller_directory_handle_remains_authority_after_path_and_symlink_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    let moved = temp.path().join("moved");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&original).unwrap();
    std::os::unix::fs::symlink(&original, &alias).unwrap();
    let retained = DatabaseDirectory::from_file(
        std::fs::File::open(&original).unwrap(),
        temp.path().join("not-authority"),
    )
    .unwrap();
    let db = Database::create_in(&retained).unwrap();
    assert!(Database::open(&alias).is_err());
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    db.catalog()
        .create_schema(&schema(), CreatePolicy::Strict)
        .unwrap();
    db.checkpoint().unwrap();
    assert!(!original.join("CURRENT").exists());
    drop(db);
    let db = Database::open_in(&retained).unwrap();
    assert!(db.catalog().snapshot().resolve_schema(&schema()).is_ok());
    assert_eq!(
        Database::open(&alias).err().unwrap().kind,
        StorageErrorKind::NotInitialized
    );
}
