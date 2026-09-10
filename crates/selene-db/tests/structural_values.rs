//! End-to-end structural parameter/result and stored-value admission contracts.

use selene_core::db_string;
use selene_db::Record;

#[test]
fn facade_record_equality_uses_names_and_preserves_scalar_representation() {
    let record = |fields: Vec<(&str, Value)>| {
        Value::Record(Box::new(Record::Open(
            fields
                .into_iter()
                .map(|(name, value)| (db_string(name).unwrap(), value))
                .collect(),
        )))
    };
    let left = record(vec![
        ("a", Value::Int(1)),
        (
            "b",
            Value::List(vec![record(vec![
                ("x", Value::Null),
                ("y", Value::Float(f64::NAN)),
            ])]),
        ),
    ]);
    let right = record(vec![
        (
            "b",
            Value::List(vec![record(vec![
                ("y", Value::Float(f64::NAN)),
                ("x", Value::Null),
            ])]),
        ),
        ("a", Value::Int(1)),
    ]);
    assert_eq!(left, right);
    assert_ne!(
        record(vec![("a", Value::Int(1))]),
        record(vec![("a", Value::Float(1.0))])
    );
}

#[test]
fn empty_result_retains_sort_keys_null_policy_and_preferred_columns() {
    use selene_db::{NullPlacement, SortDirection};
    let session = session();
    for suffix in ["", " LIMIT 2"] {
        let output = session.execute_request(Request::with_params(
            format!("MATCH (n:Missing) RETURN 3 AS last, $p AS first ORDER BY first DESC NULLS LAST, last ASC NULLS FIRST{suffix}"),
            params(Type::INT64, Value::Int(7)),
        )).into_result().unwrap();
        let ExecutionOutcome::Rows { result, .. } = output else {
            panic!("regular result");
        };
        assert!(result.rows().is_empty());
        assert_eq!(result.descriptor().preferred_columns(), &[0, 1]);
        let ordering = result.descriptor().ordering();
        assert_eq!(ordering.len(), 2);
        assert_eq!(ordering[0].column(), Some(1));
        assert_eq!(ordering[0].expression(), "`first`");
        assert_eq!(ordering[0].direction(), SortDirection::Descending);
        assert_eq!(ordering[0].nulls(), NullPlacement::Last);
        assert_eq!(ordering[1].column(), Some(0));
        assert_eq!(ordering[1].nulls(), NullPlacement::First);
    }
}

#[test]
fn omitted_result_empty_typed_table_and_null_row_are_distinct() {
    let session = session();
    let omitted = session.execute("CREATE GRAPH other ANY").unwrap();
    assert!(matches!(omitted, ExecutionOutcome::OmittedResult { .. }));
    let empty = session
        .execute("MATCH (n:Missing) RETURN CAST(NULL AS INTEGER) AS item")
        .unwrap();
    let ExecutionOutcome::Rows { result, .. } = empty else {
        panic!("empty table");
    };
    assert_eq!(result.row_count(), 0);
    assert_eq!(
        result.descriptor().fields()[0].declared_type(),
        &DeclaredType::Resolved(Type::INT64)
    );
    let null = session
        .execute("RETURN CAST(NULL AS INTEGER) AS item")
        .unwrap();
    let ExecutionOutcome::Rows { result, .. } = null else {
        panic!("null row");
    };
    assert_eq!(result.row_count(), 1);
    assert_eq!(result.rows()[0].values(), &[Value::Null]);
}
use selene_db::{
    CreatePolicy, Database, DeclaredType, ExecutionOutcome, GeneralParameter, ObjectPath, Request,
    RequestParams, SchemaPath, Type, Value,
};

fn session() -> selene_db::Session {
    let database = Database::builder().build();
    let schema = SchemaPath::regular("selene", "structural_values").unwrap();
    let graph = ObjectPath::regular("selene", "structural_values", "main").unwrap();
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

fn params(ty: Type, value: Value) -> RequestParams {
    let mut params = RequestParams::new();
    params
        .insert("p", GeneralParameter::new(ty, value).unwrap())
        .unwrap();
    params
}

#[test]
fn nested_parameter_descriptor_survives_nonempty_and_empty_results() {
    let session = session();
    let name = db_string("nullable_items").unwrap();
    let ty = Type::record([(name.clone(), Type::list(Type::INT64, Some(4)).unwrap())]).unwrap();
    let value = Value::Record(Box::new(Record::Open(
        [(name, Value::List(vec![Value::Null, Value::Int(7)]))]
            .into_iter()
            .collect(),
    )));
    for (source, count) in [
        ("RETURN $p AS item", 1),
        ("MATCH (n:Missing) RETURN $p AS item", 0),
    ] {
        let output = session
            .execute_request(Request::with_params(
                source,
                params(ty.clone(), value.clone()),
            ))
            .into_result()
            .unwrap();
        let ExecutionOutcome::Rows { result, .. } = output else {
            panic!("regular result");
        };
        assert_eq!(result.row_count(), count);
        assert_eq!(result.descriptor().fields()[0].name(), Some("item"));
        assert_eq!(
            result.descriptor().fields()[0].declared_type(),
            &DeclaredType::Resolved(ty.clone())
        );
        if count != 0 {
            assert_eq!(result.rows()[0].values(), std::slice::from_ref(&value));
        }
    }
}

#[test]
fn equivalent_inline_parameter_declarations_share_one_semantic_type() {
    let output = session()
        .execute_request(Request::with_params(
            "RETURN $p :: SMALLINT AS a, $p :: INT16 AS b",
            params(
                Type::from_scalar(selene_db::ScalarType::Int16).unwrap(),
                Value::Int(7),
            ),
        ))
        .into_result()
        .unwrap();
    let ExecutionOutcome::Rows { result, .. } = output else {
        panic!("regular result");
    };
    assert_eq!(result.rows()[0].values(), &[Value::Int(7), Value::Int(7)]);
    assert_eq!(
        result.descriptor().fields()[0].declared_type(),
        result.descriptor().fields()[1].declared_type()
    );
}

#[test]
fn nested_query_reference_can_be_returned_but_not_published_as_property() {
    let session = session();
    let ExecutionOutcome::Written {
        result: Some(result),
        ..
    } = session.execute("INSERT (n:Live) RETURN n").unwrap()
    else {
        panic!("write result");
    };
    let reference = result.rows()[0].values()[0].clone();
    let field = db_string("reference").unwrap();
    let ty = Type::list(Type::record([(field.clone(), Type::NODE)]).unwrap(), None).unwrap();
    let value = Value::List(vec![Value::Record(Box::new(Record::Open(
        [(field, reference)].into_iter().collect(),
    )))]);
    let returned = session
        .execute_request(Request::with_params(
            "RETURN $p AS copied",
            params(ty.clone(), value.clone()),
        ))
        .into_result()
        .unwrap();
    assert_eq!(returned.row_count(), Some(1));
    let failure = session
        .execute_request(Request::with_params(
            "INSERT (:ShouldRollBack), (:Payload { content: $p }) FINISH",
            params(ty, value),
        ))
        .into_result()
        .unwrap_err();
    assert_eq!(failure.gqlstatus().unwrap().as_str(), "22G03");
    assert_eq!(
        session.execute("MATCH (n) RETURN n").unwrap().row_count(),
        Some(1)
    );
}
