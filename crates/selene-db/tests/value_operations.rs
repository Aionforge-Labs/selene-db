//! Operation-specific runtime value contracts through the supported facade.

use selene_db::{
    CreatePolicy, Database, ExecutionOutcome, GeneralParameter, ObjectPath, Request, RequestParams,
    SchemaPath, Session, Type, Value,
};

fn session() -> Session {
    let database = Database::builder().build();
    let schema = SchemaPath::regular("selene", "operations").unwrap();
    let graph = ObjectPath::regular("selene", "operations", "main").unwrap();
    database
        .catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    database
        .catalog()
        .create_graph(&graph, None, CreatePolicy::Strict)
        .unwrap();
    database.session(&graph).unwrap()
}

fn run(
    session: &Session,
    source: &str,
    values: &[(&str, Value)],
) -> selene_db::Result<ExecutionOutcome> {
    let mut params = RequestParams::new();
    let mut source = source.to_owned();
    for (name, value) in values {
        let ty = Type::new(selene_db::TypeKind::Record(None), true).unwrap();
        let wrapped = Value::Record(Box::new(selene_db::Record::Open(vec![(
            selene_core::db_string("value").unwrap(),
            value.clone(),
        )])));
        params
            .insert(name, GeneralParameter::new(ty, wrapped).unwrap())
            .unwrap();
        source = source.replace(&format!("${name}"), &format!("${name}.value"));
    }
    session
        .execute_request(Request::with_params(source, params))
        .into_result()
}

#[test]
fn dynamic_incomparable_values_fail_equality_distinct_grouping_and_sorting() {
    let session = session();
    let values = [
        ("a", Value::Int(1)),
        ("b", Value::String(selene_core::db_string("one").unwrap())),
    ];
    for source in [
        "RETURN $a = $b",
        "RETURN $a <> $b",
        "RETURN $a < $b",
        "FOR x IN [$a, $b] RETURN DISTINCT x",
        "FOR x IN [$a, $b] RETURN x, count(*) GROUP BY x",
        "FOR x IN [$a, $b] RETURN count(DISTINCT x)",
        "FOR x IN [$a, $b] RETURN x ORDER BY x",
        "FOR x IN [$a, $b] RETURN x ORDER BY x LIMIT 1",
        "RETURN $a AS x UNION RETURN $b AS x",
        "RETURN $a AS x INTERSECT RETURN $b AS x",
        "RETURN $a AS x EXCEPT RETURN $b AS x",
    ] {
        let error = run(&session, source, &values).unwrap_err();
        assert_eq!(
            error.gqlstatus().unwrap().as_str(),
            "22G04",
            "{source}: {error}"
        );
    }
}

#[test]
fn nan_predicates_are_unknown_and_nested_sorting_places_nan_high() {
    let session = session();
    let values = [("a", Value::Float(f64::NAN)), ("b", Value::Int(1))];
    let ExecutionOutcome::Rows { result, .. } = run(
        &session,
        "RETURN $a = $b, $a <> $b, $a < $b, $a <= $b, $a > $b, $a >= $b",
        &values,
    )
    .unwrap() else {
        panic!("rows")
    };
    assert_eq!(result.rows()[0].values(), &[const { Value::Null }; 6]);
    let ExecutionOutcome::Rows { result, .. } = run(
        &session,
        "FOR x IN [[$a], [$b]] RETURN x ORDER BY x",
        &values,
    )
    .unwrap() else {
        panic!("rows")
    };
    assert_eq!(
        result.rows()[0].values(),
        &[Value::List(vec![Value::Int(1)])]
    );
}

#[test]
fn nested_null_sorting_is_total_but_predicates_remain_unknown() {
    let session = session();
    let ExecutionOutcome::Rows { result, .. } = session
        .execute("FOR x IN [[NULL], [2], [1]] RETURN x ORDER BY x")
        .unwrap()
    else {
        panic!("rows")
    };
    assert_eq!(
        result.rows()[0].values(),
        &[Value::List(vec![Value::Int(1)])]
    );
    assert_eq!(result.rows()[2].values(), &[Value::List(vec![Value::Null])]);
}

#[test]
fn recursive_comparability_checks_all_positions_and_record_field_names() {
    let session = session();
    let text = || Value::String(selene_core::db_string("x").unwrap());
    let record = |name: &str| {
        Value::Record(Box::new(selene_db::Record::Open(vec![(
            selene_core::db_string(name).unwrap(),
            Value::Int(1),
        )])))
    };
    for (a, b) in [
        (
            Value::List(vec![Value::Int(1), text()]),
            Value::List(vec![Value::Int(2), Value::Int(0)]),
        ),
        (record("a"), record("b")),
    ] {
        for source in [
            "RETURN $a = $b",
            "FOR x IN [$a, $b] RETURN DISTINCT x",
            "FOR x IN [$a, $b] RETURN x ORDER BY x",
        ] {
            let error = run(&session, source, &[("a", a.clone()), ("b", b.clone())]).unwrap_err();
            assert_eq!(error.gqlstatus().unwrap().as_str(), "22G04", "{source}");
        }
    }
}

#[test]
fn internal_analysis_types_are_not_facade_parameter_declarations() {
    for ty in [
        Type::DYNAMIC,
        Type::NULL,
        Type::EMPTY,
        Type::new(selene_db::TypeKind::Property, true).unwrap(),
        Type::list(Type::DYNAMIC, None).unwrap(),
    ] {
        let error = GeneralParameter::new(ty, Value::Null).unwrap_err();
        assert_eq!(error.gqlstatus().unwrap().as_str(), "22G03");
    }
}

#[test]
fn paths_group_and_sort_by_elements_while_equality_requires_ga09() {
    use selene_core::{EdgeDirection, EdgeId, NodeId};
    let session = session();
    session.execute("INSERT (a:A)-[:Loop]->(a) FINISH").unwrap();
    let node = session.node_reference(NodeId::new(1)).unwrap();
    let edge = session.edge_reference(EdgeId::new(1)).unwrap();
    let paths: Vec<_> = [EdgeDirection::Outgoing, EdgeDirection::Incoming]
        .into_iter()
        .map(|direction| {
            Value::Path(Box::new(
                session
                    .path_reference(
                        node,
                        vec![selene_db::ValuePathSegment::new(edge, direction, node)],
                    )
                    .unwrap(),
            ))
        })
        .collect();
    let values = [("a", paths[0].clone()), ("b", paths[1].clone())];
    assert_eq!(
        run(&session, "RETURN $a = $b", &values)
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "22G04"
    );
    let ExecutionOutcome::Rows { result, .. } = run(
        &session,
        "FOR x IN [$a, $b] RETURN DISTINCT x ORDER BY x",
        &values,
    )
    .unwrap() else {
        panic!("rows");
    };
    assert_eq!(
        result.row_count(),
        1,
        "direction is not an element in the path-element list"
    );
}
