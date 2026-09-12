//! F05-PR04: source-derived path semantics through the public request boundary.

use selene_db::{
    CreatePolicy, Database, ExecutionOutcome, ObjectPath, RegularResult, Request, RequestOutcome,
    SchemaPath, Session, Value,
};

fn session() -> Session {
    let db = Database::builder().build();
    let schema = SchemaPath::regular("selene", "path_batches").unwrap();
    let graph = ObjectPath::regular("selene", "path_batches", "g").unwrap();
    db.catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&graph, None, CreatePolicy::Strict)
        .unwrap();
    db.session(&graph).unwrap()
}

fn rows(s: &Session, source: &str) -> RegularResult {
    let ExecutionOutcome::Rows { result, .. } = s
        .execute(source)
        .unwrap_or_else(|e| panic!("{source}: {e:?}"))
    else {
        panic!("rows: {source}")
    };
    result
}

#[test]
fn questioned_and_zero_one_group_have_equal_traversals_but_distinct_exposure() {
    let s = session();
    s.execute("INSERT (a:A {n:0})-[:E]->(b:B {n:1}) FINISH")
        .unwrap();
    let questioned = rows(&s, "MATCH p = (a:A)-[r?]->(b) RETURN r, p ORDER BY b.n");
    let group = rows(&s, "MATCH p = (a:A)-[r{0,1}]->(b) RETURN r, p ORDER BY b.n");
    assert_eq!(questioned.row_count(), 2);
    assert_eq!(group.row_count(), 2);
    assert_eq!(questioned.rows()[0].values()[0], Value::Null);
    assert!(matches!(&group.rows()[0].values()[0], Value::List(v) if v.is_empty()));
    assert!(matches!(
        &questioned.rows()[1].values()[0],
        Value::EdgeRef(_)
    ));
    assert!(matches!(&group.rows()[1].values()[0], Value::List(v) if v.len() == 1));
    for (q, g) in questioned.rows().iter().zip(group.rows()) {
        assert_eq!(q.values()[1], g.values()[1]);
    }
    assert_ne!(
        questioned.descriptor().fields()[0].declared_type(),
        group.descriptor().fields()[0].declared_type()
    );
}

#[test]
fn multi_batch_results_preserve_duplicate_input_correlation_and_group_values() {
    let s = session();
    s.execute("INSERT (a:A {n:0})-[:E]->(b:B {n:1}), (a)-[:E]->(b) FINISH")
        .unwrap();
    let inputs = (0..1100)
        .map(|i| (i % 2).to_string())
        .collect::<Vec<_>>()
        .join(",");
    let result = rows(
        &s,
        &format!(
            "FOR wanted IN [{inputs}] MATCH p = (a:A)-[r{{0,1}}]->(b WHERE b.n = wanted) RETURN wanted, r, p"
        ),
    );
    assert_eq!(result.row_count(), 1650);
    let mut counts = [0; 2];
    for row in result.rows() {
        let [Value::Int(wanted), Value::List(group), Value::Path(path)] = row.values() else {
            panic!("typed correlated row")
        };
        counts[*wanted as usize] += 1;
        assert_eq!(group.len(), *wanted as usize);
        assert_eq!(path.segments().len(), group.len());
    }
    assert_eq!(counts, [550, 1100]);
}

#[test]
fn endpoint_partition_and_local_predicates_precede_selection_but_clause_filter_follows() {
    let s = session();
    s.execute("INSERT (a:A)-[:E]->(b:B), (a)-[:E]->(c:C)-[:E]->(b), (b)-[:E]->(d:D) FINISH")
        .unwrap();
    let result = rows(
        &s,
        "MATCH ALL SHORTEST p = (a:A)-[r{1,3}]->(b) RETURN size(r) AS hops ORDER BY hops",
    );
    assert_eq!(
        result
            .rows()
            .iter()
            .map(|r| r.values()[0].clone())
            .collect::<Vec<_>>(),
        vec![Value::Int(1), Value::Int(1), Value::Int(2)]
    );
    for source in [
        "MATCH ALL SHORTEST p = (a:A)-[r{1,2} WHERE size(r) = 2]->(b:B) RETURN p",
        "MATCH ALL SHORTEST p = (a:A)-[r{1,2}]->(b:B WHERE size(r) = 2) RETURN p",
        "MATCH ALL SHORTEST p = (a:A)-[r{1,2}]->(b:B WHERE EXISTS { MATCH (b) WHERE size(r) = 2 }) RETURN p",
    ] {
        let result = rows(&s, source);
        assert_eq!(result.row_count(), 1, "{source}");
        let Value::Path(path) = &result.rows()[0].values()[0] else {
            panic!("path")
        };
        assert_eq!(path.segments().len(), 2);
    }
    assert_eq!(
        rows(
            &s,
            "MATCH ALL SHORTEST (a:A)-[r{1,2}]->(b:B) WHERE size(r) = 2 RETURN r"
        )
        .row_count(),
        0
    );
}

#[test]
fn path_resource_error_is_failed_request_even_with_limit_and_prevents_mutation() {
    let s = session();
    s.execute("INSERT (a:A)-[:E]->(a) FINISH").unwrap();
    // An unreachable length condition prevents completion of this cyclic open
    // WALK. The request must report its resource boundary, not return LIMIT rows.
    for limit in [0, 1] {
        let source = format!(
            "MATCH ALL SHORTEST (a:A)-[r+ WHERE size(r) > 100]->(b) RETURN r LIMIT {limit}"
        );
        let outcome = s.execute_request(Request::new(source));
        assert!(
            matches!(outcome, RequestOutcome::Failed { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            outcome
                .into_result()
                .unwrap_err()
                .gqlstatus()
                .unwrap()
                .as_str(),
            "5GQL1"
        );
    }
    let failed = s.execute(
        "MATCH ALL SHORTEST (a:A)-[r+ WHERE size(r) > 100]->(b) SET a.touched = true FINISH",
    );
    assert_eq!(failed.unwrap_err().gqlstatus().unwrap().as_str(), "5GQL1");
    assert_eq!(
        rows(&s, "MATCH (n WHERE n.touched = true) RETURN n").row_count(),
        0
    );
}

#[test]
fn completion_certificate_does_not_suppress_earlier_predicate_errors() {
    let s = session();
    s.execute("INSERT (a:A)-[:E]->(a) FINISH").unwrap();
    let error = s
        .execute("MATCH ALL SHORTEST (a:A {n: 1 / 0, absent: 0})-[r+]->(b) RETURN r")
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22012");
}

#[test]
fn mixed_orientation_modes_match_modes_and_zero_edges_share_the_request_path() {
    let s = session();
    s.execute("INSERT (a:A)~[:U]~(b:B), (a)-[:D]->(a) FINISH")
        .unwrap();
    for (mode, count) in [("WALK", 3), ("TRAIL", 1), ("SIMPLE", 1), ("ACYCLIC", 0)] {
        let result = rows(&s, &format!("MATCH {mode} (a:A)-[r{{2}}]-(b) RETURN r"));
        // WALK: U,U / D,D / D,U. TRAIL: D,U. SIMPLE: U,U closure.
        assert_eq!(result.row_count(), count, "{mode}");
    }
    assert_eq!(
        rows(
            &s,
            "MATCH DIFFERENT EDGES (a:A)-[r{1}]->(b), (a)-[t{1}]->(c) RETURN r, t"
        )
        .row_count(),
        0
    );
    assert_eq!(
        rows(
            &s,
            "MATCH REPEATABLE ELEMENTS (a:A)-[r{1}]->(b), (a)-[t{1}]->(c) RETURN r, t"
        )
        .row_count(),
        1
    );
    let zero = rows(&s, "MATCH p = (a:A)~[r{0}]~(b) RETURN p");
    let Value::Path(path) = &zero.rows()[0].values()[0] else {
        panic!("zero path")
    };
    assert!(path.segments().is_empty());
    let mixed = rows(&s, "MATCH p = (b:B)~[r{1}]~(a:A)<-[d{1}]-(a) RETURN p");
    let Value::Path(path) = &mixed.rows()[0].values()[0] else {
        panic!("mixed path")
    };
    assert_eq!(
        path.segments()[0].direction(),
        selene_core::EdgeDirection::Undirected
    );
    assert_eq!(
        path.segments()[1].direction(),
        selene_core::EdgeDirection::Incoming
    );
}

#[test]
fn paths_compose_with_join_filter_projection_native_call_and_staged_mutations() {
    let s = session();
    s.execute("INSERT (a:A {n:1})-[:E]->(b:B {n:2}), (b)-[:E]->(c:C {n:3}) FINISH")
        .unwrap();
    let result = rows(
        &s,
        "MATCH (a:A), (x:B) FILTER a.n < x.n MATCH ALL SHORTEST p = (a)-[r{1,2}]->(b) CALL selene.health() YIELD node_count RETURN x.n AS x, size(r) AS hops, node_count ORDER BY hops",
    );
    assert_eq!(result.row_count(), 2);
    assert_eq!(result.rows()[0].values()[0], Value::Int(2));
    assert_eq!(result.rows()[0].values()[1], Value::Int(1));
    assert_eq!(result.rows()[1].values()[1], Value::Int(2));
    s.execute("START TRANSACTION").unwrap();
    s.execute("INSERT (:D) FINISH").unwrap();
    s.execute("MATCH (a:A)-[r{1,2}]->(b) SET b.seen = true FINISH")
        .unwrap();
    assert_eq!(
        rows(&s, "MATCH (b WHERE b.seen = true) RETURN b").row_count(),
        2
    );
    s.execute("ROLLBACK").unwrap();
    assert_eq!(
        rows(&s, "MATCH (b WHERE b.seen = true) RETURN b").row_count(),
        0
    );
    assert_eq!(rows(&s, "MATCH (d:D) RETURN d").row_count(), 0);
}

#[test]
fn selective_families_keep_parallel_identity_and_distinct_length_group_counts() {
    let s = session();
    s.execute("INSERT (a:A)-[:E]->(b:B), (a)-[:E]->(b), (a)-[:E]->(c:C)-[:E]->(b) FINISH")
        .unwrap();
    for (prefix, lengths) in [
        ("ALL PATHS", vec![1, 1, 2]),
        ("ANY", vec![1]),
        ("ANY 2 PATHS", vec![1, 1]),
        ("ANY SHORTEST", vec![1]),
        ("ALL SHORTEST", vec![1, 1]),
        ("SHORTEST 2 PATHS", vec![1, 1]),
        ("SHORTEST 2 GROUPS", vec![1, 1, 2]),
    ] {
        let result = rows(
            &s,
            &format!("MATCH {prefix} p = (a:A)-[r{{1,2}}]->(b:B) RETURN p"),
        );
        let mut actual = result
            .rows()
            .iter()
            .map(|r| {
                let Value::Path(p) = &r.values()[0] else {
                    panic!("selected path")
                };
                p.segments().len()
            })
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, lengths, "{prefix}");
        for (i, left) in result.rows().iter().enumerate() {
            for right in &result.rows()[i + 1..] {
                assert_ne!(left.values()[0], right.values()[0]);
            }
        }
    }
}

#[test]
fn path_subqueries_in_mutation_values_use_logical_metadata_and_the_staged_snapshot() {
    let s = session();
    s.execute("INSERT (a:A)-[:E]->(b:B)-[:E]->(c:C) FINISH")
        .unwrap();
    s.execute(
        "INSERT (:Summary {n: VALUE { MATCH (a:A)-[r{1,2}]->(b) RETURN count(*) AS n }}) FINISH",
    )
    .unwrap();
    assert_eq!(
        rows(&s, "MATCH (n:Summary) RETURN n.n").rows()[0].values()[0],
        Value::Int(2)
    );
    s.execute("MATCH (n:Summary) SET n.n = VALUE { MATCH (a:A)-[r{2}]->(b) RETURN count(*) AS amount } FINISH")
        .unwrap();
    assert_eq!(
        rows(&s, "MATCH (n:Summary) RETURN n.n").rows()[0].values()[0],
        Value::Int(1)
    );
}
