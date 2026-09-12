//! F04-PR03 facade acceptance: joins and set operations end to end.
//!
//! These queries run through logical planning and the stable result
//! boundary (`Session::execute`): repeated-key joins keep multiplicity,
//! set versus multiset arms count deliberately differently, unmatched
//! outer rows survive with nulls, and `NEXT` blocks compose.

use selene_db::{CreatePolicy, Database, ExecutionOutcome, ObjectPath, SchemaPath, Value};

fn schema(name: &str) -> SchemaPath {
    SchemaPath::regular("selene", name).unwrap()
}

fn graph(schema: &str, name: &str) -> ObjectPath {
    ObjectPath::regular("selene", schema, name).unwrap()
}

fn fixture() -> (Database, ObjectPath) {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let path = graph("memory", "main");
    catalog
        .create_schema(&schema("memory"), CreatePolicy::Strict)
        .unwrap();
    catalog
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    (database, path)
}

fn rows(outcome: ExecutionOutcome) -> Vec<Vec<Value>> {
    let ExecutionOutcome::Rows { result, .. } = outcome else {
        panic!("expected rows, got {outcome:?}");
    };
    result
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

fn seed_people(session: &selene_db::Session) {
    for name in ["Ada", "Bob", "Ada"] {
        session
            .execute(&format!("INSERT (:Person {{ name: '{name}' }})"))
            .expect("insert succeeds");
    }
    session
        .execute("INSERT (:Robot { name: 'R2' })")
        .expect("insert succeeds");
}

#[test]
fn facade_shared_binding_join_keeps_one_row_per_person() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    seed_people(&session);

    // Same-name shared binding across comma clauses joins each person with
    // itself: three persons yield three bindings, never deduplicated away
    // nor fanned out.
    let found = rows(
        session
            .execute("MATCH (a:Person) MATCH (a:Person) RETURN a")
            .expect("query succeeds"),
    );
    assert_eq!(found.len(), 3);
}

#[test]
fn facade_cross_product_keeps_every_pair() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    seed_people(&session);

    let found = rows(
        session
            .execute("MATCH (a:Person), (b:Robot) RETURN a, b")
            .expect("query succeeds"),
    );
    assert_eq!(found.len(), 3, "three persons times one robot");
}

#[test]
fn facade_union_all_and_union_count_deliberately_different() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();

    let all = rows(
        session
            .execute("RETURN 1 AS n UNION ALL RETURN 1 AS n")
            .expect("query succeeds"),
    );
    assert_eq!(all.len(), 2);
    let distinct = rows(
        session
            .execute("RETURN 1 AS n UNION RETURN 1 AS n")
            .expect("query succeeds"),
    );
    assert_eq!(distinct.len(), 1);
    let intersect = rows(
        session
            .execute("RETURN 1 AS n INTERSECT RETURN 1 AS n")
            .expect("query succeeds"),
    );
    assert_eq!(intersect.len(), 1);
    let except = rows(
        session
            .execute("RETURN 1 AS n EXCEPT RETURN 1 AS n")
            .expect("query succeeds"),
    );
    assert!(except.is_empty());
}

#[test]
fn facade_unmatched_optional_rows_survive_with_nulls() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    seed_people(&session);

    // No KNOWS edges exist: every person survives with a null neighbor.
    let found = rows(
        session
            .execute("MATCH (a:Person) OPTIONAL MATCH (a)-[e:KNOWS]->(b) RETURN a, b")
            .expect("query succeeds"),
    );
    assert_eq!(found.len(), 3);
    assert!(
        found.iter().all(|row| row.len() == 2),
        "outer rows keep their width, got {found:?}"
    );
}

#[test]
fn facade_next_blocks_compose_after_matches() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    seed_people(&session);

    // A non-leading match over fresh bindings fans out per input row (three
    // persons times one robot), then NEXT discards into its own block.
    let found = rows(
        session
            .execute(
                "MATCH (a:Person) WITH a AS x MATCH (b:Robot) RETURN x, b NEXT RETURN 7 AS seven",
            )
            .expect("query succeeds"),
    );
    assert_eq!(found, vec![vec![Value::Int(7)]]);
}
