//! Detached transactions must never recycle an escaped reference identity.

use selene_db::{
    CreatePolicy, Database, ExecutionOutcome, GeneralParameter, ObjectPath, Request, RequestParams,
    SchemaPath, Session, Type, Value,
};

fn sessions() -> (Session, Session) {
    let db = Database::builder().build();
    let schema = SchemaPath::regular("selene", "identity").unwrap();
    let path = ObjectPath::regular("selene", "identity", "graph").unwrap();
    db.catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    (db.session(&path).unwrap(), db.session(&path).unwrap())
}

fn pair(session: &Session) -> Vec<Value> {
    let ExecutionOutcome::Written {
        result: Some(table),
        ..
    } = session
        .execute("INSERT (a:Item {v: 1})-[e:LINK {v: 2}]->(b:Item {v: 3}) RETURN a, e")
        .unwrap()
    else {
        panic!("write rows")
    };
    table.rows()[0].values().to_vec()
}

fn access(session: &Session, value: Value, source: &str) -> selene_db::Result<ExecutionOutcome> {
    let ty = match &value {
        Value::NodeRef(_) => Type::NODE,
        Value::EdgeRef(_) => Type::EDGE,
        _ => panic!("reference"),
    };
    let mut params = RequestParams::new();
    params
        .insert("r", GeneralParameter::new(ty, value).unwrap())
        .unwrap();
    session
        .execute_request(Request::with_params(source, params))
        .into_result()
}

fn assert_invalidated(session: &Session, references: &[Value]) {
    for value in references {
        assert_eq!(
            access(session, value.clone(), "RETURN $r")
                .unwrap()
                .row_count(),
            Some(1)
        );
        let error = access(session, value.clone(), "RETURN $r.v").unwrap_err();
        assert_eq!(error.gqlstatus().unwrap().as_str(), "22G11");
    }
}

#[test]
fn rollback_burns_node_and_edge_identities() {
    let (session, observer) = sessions();
    session.execute("START TRANSACTION").unwrap();
    let escaped = pair(&session);
    session.execute("ROLLBACK").unwrap();
    assert_eq!(
        observer.execute("MATCH (n) RETURN n").unwrap().row_count(),
        Some(0)
    );
    let replacements = pair(&session);
    assert_ne!(escaped[0], replacements[0]);
    assert_ne!(escaped[1], replacements[1]);
    assert_invalidated(&observer, &escaped);
}

#[test]
fn failed_statement_burns_earlier_escaped_identities() {
    let (session, observer) = sessions();
    session.execute("START TRANSACTION").unwrap();
    let escaped = pair(&session);
    assert!(session.execute("RETURN 1 / 0").is_err());
    session.rollback_transaction().unwrap();
    let replacements = pair(&observer);
    assert_ne!(escaped[0], replacements[0]);
    assert_ne!(escaped[1], replacements[1]);
    assert_invalidated(&session, &escaped);
}

#[test]
fn concurrent_detached_transactions_share_one_non_reusing_authority() {
    let (first, second) = sessions();
    first.execute("START TRANSACTION").unwrap();
    second.execute("START TRANSACTION").unwrap();
    let committed = pair(&first);
    let conflicted = pair(&second);
    assert_ne!(committed[0], conflicted[0]);
    assert_ne!(committed[1], conflicted[1]);
    first.execute("COMMIT").unwrap();
    assert!(second.execute("COMMIT").is_err());
    let later = pair(&first);
    for index in 0..2 {
        assert_ne!(later[index], committed[index]);
        assert_ne!(later[index], conflicted[index]);
    }
    assert_invalidated(&first, &conflicted);
}
