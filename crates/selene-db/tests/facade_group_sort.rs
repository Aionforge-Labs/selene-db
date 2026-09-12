//! F04-PR04 facade acceptance: grouping, aggregation, and sorting end to end.
//!
//! These queries run through logical planning and the stable result
//! boundary (`Session::execute`): empty inputs yield their three distinct
//! required results, null keys group together, ordering places nulls per
//! policy, deduplication keeps first occurrences, and incompatible
//! comparisons fail with `22G04` instead of an invented order.

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
    for (name, age) in [("Ada", 30), ("Bob", 20), ("Cyd", 30)] {
        session
            .execute(&format!(
                "INSERT (:Person {{ name: '{name}', age: {age} }})"
            ))
            .expect("insert succeeds");
    }
    session
        .execute("INSERT (:Robot { name: 'R2' })")
        .expect("insert succeeds");
}

#[test]
fn facade_empty_inputs_yield_three_distinct_results() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();

    // Empty ungrouped aggregation yields one row with the specified
    // per-function results.
    let found = rows(
        session
            .execute("MATCH (n:Missing) RETURN count(*) AS c, sum(n.age) AS s, avg(n.age) AS a")
            .expect("query succeeds"),
    );
    assert_eq!(
        found,
        vec![vec![Value::Int(0), Value::Int(0), Value::Null]],
        "empty ungrouped: count zero, sum zero, avg null"
    );
    // Empty grouped input yields zero rows but keeps both descriptors.
    let outcome = session
        .execute("MATCH (n:Missing) RETURN n.age AS age, count(*) AS c GROUP BY n.age")
        .expect("query succeeds");
    let ExecutionOutcome::Rows { result, .. } = outcome else {
        panic!("expected rows");
    };
    assert_eq!(result.row_count(), 0, "empty grouped yields no rows");
    assert_eq!(result.descriptor().fields().len(), 2);
    assert_eq!(result.descriptor().preferred_columns(), &[0, 1]);
    // Empty ordering keeps its descriptor and null policy.
    let outcome = session
        .execute("MATCH (n:Missing) RETURN n.name AS name ORDER BY name")
        .expect("query succeeds");
    let ExecutionOutcome::Rows { result, .. } = outcome else {
        panic!("expected rows");
    };
    assert_eq!(result.row_count(), 0);
    assert_eq!(result.descriptor().preferred_columns(), &[0]);
    assert_eq!(result.descriptor().ordering().len(), 1);
}

#[test]
fn facade_grouped_counts_collect_nulls_together() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    seed_people(&session);

    // Two persons share age 30; the ageless robot forms the single null
    // group rather than vanishing or splitting.
    let found = rows(
        session
            .execute("MATCH (n) RETURN n.age AS age, count(*) AS c GROUP BY n.age")
            .expect("query succeeds"),
    );
    assert_eq!(found.len(), 3, "ages 20 and 30 plus one null group");
    let mut thirty = 0;
    let mut twenty = 0;
    let mut nulls = 0;
    for row in &found {
        match &row[..] {
            [Value::Int(30), Value::Int(2)] => thirty += 1,
            [Value::Int(20), Value::Int(1)] => twenty += 1,
            [Value::Null, Value::Int(1)] => nulls += 1,
            other => panic!("unexpected group {other:?}"),
        }
    }
    assert_eq!((thirty, twenty, nulls), (1, 1, 1));
    // Ungrouped totals agree: four nodes, three known ages.
    let found = rows(
        session
            .execute("MATCH (n) RETURN count(*) AS c, count(n.age) AS known")
            .expect("query succeeds"),
    );
    assert_eq!(
        found,
        vec![vec![Value::Int(4), Value::Int(3)]],
        "COUNT(*) counts rows, COUNT(age) skips the missing age"
    );
}

#[test]
fn facade_ordering_places_nulls_and_pages_windows() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    seed_people(&session);

    // Ascending age defaults nulls last: Bob(20), Ada(30), Cyd(30), R2.
    // The tied pair asserts no order (multiset), the null placement is exact.
    let found = rows(
        session
            .execute("MATCH (n) RETURN n.name AS name ORDER BY n.age")
            .expect("query succeeds"),
    );
    assert_eq!(found.len(), 4);
    assert_eq!(found[0], vec![name_value("Bob")]);
    assert_eq!(found[3], vec![name_value("R2")], "null age sorts last");
    let mut middle = vec![found[1].clone(), found[2].clone()];
    middle.sort_by(|lhs, rhs| format!("{lhs:?}").cmp(&format!("{rhs:?}")));
    assert_eq!(
        middle,
        vec![vec![name_value("Ada")], vec![name_value("Cyd")]]
    );
    // Explicit NULLS FIRST plus a page window composes through the same path.
    let found = rows(
        session
            .execute("MATCH (n) RETURN n.name AS name ORDER BY n.age NULLS FIRST LIMIT 2")
            .expect("query succeeds"),
    );
    assert_eq!(found[0], vec![name_value("R2")], "nulls first");
    assert_eq!(found.len(), 2);
    // Descending names follow binary collation end to end.
    let found = rows(
        session
            .execute("MATCH (n) RETURN n.name AS name ORDER BY name DESC LIMIT 2")
            .expect("query succeeds"),
    );
    assert_eq!(
        found,
        vec![vec![name_value("R2")], vec![name_value("Cyd")]],
        "binary collation descends R2 past Cyd"
    );
}

#[test]
fn facade_distinct_dedups_ages() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    seed_people(&session);

    let found = rows(
        session
            .execute("MATCH (n:Person) RETURN DISTINCT n.age AS age ORDER BY age")
            .expect("query succeeds"),
    );
    assert_eq!(
        found,
        vec![vec![Value::Int(20)], vec![Value::Int(30)]],
        "duplicate age 30 dedups to one row"
    );
}

#[test]
fn facade_incompatible_ordering_fails_typed() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    session
        .execute("INSERT (:Thing { age: 1 })")
        .expect("insert succeeds");
    session
        .execute("INSERT (:Thing { age: 'old' })")
        .expect("insert succeeds");

    let error = session
        .execute("MATCH (n:Thing) RETURN n.age AS age ORDER BY age")
        .expect_err("mixed int/string ordering must fail");
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22G04");
    let error = session
        .execute("MATCH (n:Thing) RETURN n.age AS age, count(*) AS c GROUP BY n.age")
        .expect_err("mixed int/string grouping must fail");
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22G04");
}

fn name_value(name: &str) -> Value {
    Value::String(selene_core::db_string(name).unwrap())
}
