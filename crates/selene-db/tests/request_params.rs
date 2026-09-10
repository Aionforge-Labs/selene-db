//! Facade request parameter construction and merge coverage.

use std::collections::BTreeMap;

use proptest::prelude::*;
use selene_core::{JsonValue, VectorValue, db_string};
use selene_db::{
    CreatePolicy, Database, ErrorKind, GeneralParameter, ObjectPath, Request, RequestOutcome,
    RequestParams, ScalarType, SchemaPath, Type, TypeKind,
};
use selene_db::{Record, Value};
use selene_gql::{GqlType, RecordType, normalize_value_type};

type IntegerSnapshot = Vec<(String, i64)>;

struct OverlayObservation {
    merged: IntegerSnapshot,
    inherited: IntegerSnapshot,
    session_count: u64,
}

fn parameter(declared_type: GqlType, value: Value) -> GeneralParameter {
    GeneralParameter::new(normalize_value_type(&declared_type).unwrap(), value).unwrap()
}

fn session() -> selene_db::Session {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let schema = SchemaPath::regular("selene", "request_params").unwrap();
    let graph = ObjectPath::regular("selene", "request_params", "main").unwrap();
    catalog
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    catalog
        .create_graph(&graph, None, CreatePolicy::Strict)
        .unwrap();
    database.session(&graph).unwrap()
}

fn ordered_entries(values: &BTreeMap<u8, i16>, ranks: &[u8]) -> Vec<(u8, i16)> {
    let mut entries = values
        .iter()
        .map(|(&key, &value)| (key, value))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(key, _)| (ranks[usize::from(*key)], *key));
    entries
}

fn integer_snapshot(outcome: &RequestOutcome) -> IntegerSnapshot {
    outcome
        .context()
        .parameters()
        .iter()
        .map(|(name, parameter)| {
            let Value::Int(value) = parameter.value() else {
                panic!("generated parameter must remain an integer");
            };
            (name.to_owned(), *value)
        })
        .collect()
}

fn execute_overlay(
    session_entries: &[(u8, i16)],
    request_entries: &[(u8, i16)],
) -> OverlayObservation {
    let session = session();
    for &(key, value) in session_entries {
        session
            .set_parameter(
                &format!("p{key}"),
                parameter(GqlType::Integer, Value::Int(i64::from(value))),
            )
            .unwrap();
    }
    let mut request = RequestParams::new();
    for &(key, value) in request_entries {
        request
            .insert(
                &format!("p{key}"),
                parameter(GqlType::Integer, Value::Int(i64::from(value))),
            )
            .unwrap();
    }

    let merged =
        integer_snapshot(&session.execute_request(Request::with_params("RETURN 1", request)));
    let inherited = integer_snapshot(&session.execute_request(Request::new("RETURN 1")));
    OverlayObservation {
        merged,
        inherited,
        session_count: session.context().parameters().len(),
    }
}

#[test]
fn names_match_parser_spelling_and_duplicates_are_exact() {
    let mut params = RequestParams::new();
    for name in ["value", "_2", "RETURN", "Δείγμα", "变量2"] {
        params
            .insert(name, parameter(GqlType::Integer, Value::Int(1)))
            .unwrap();
    }
    assert_eq!(params.len(), 5);

    let duplicate = params
        .insert("value", parameter(GqlType::Integer, Value::Int(2)))
        .unwrap_err();
    assert_eq!(duplicate.kind(), ErrorKind::DuplicateParameter);

    params
        .insert("Value", parameter(GqlType::Integer, Value::Int(3)))
        .unwrap();
    for name in ["", "$value", "2value", "has-dash", "with space"] {
        let error = params
            .insert(name, parameter(GqlType::Integer, Value::Int(1)))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidParameterName, "{name:?}");
    }
}

#[test]
fn declared_types_use_runtime_matcher_for_representative_values() {
    assert!(GeneralParameter::new(Type::INT64, Value::Null).is_ok());
    let null_error =
        GeneralParameter::new(Type::INT64.with_nullability(false), Value::Null).unwrap_err();
    assert_eq!(null_error.gqlstatus().unwrap().as_str(), "22G03");

    assert!(
        GeneralParameter::new(
            Type::from_scalar(ScalarType::Int8).unwrap(),
            Value::Int(127)
        )
        .is_ok()
    );
    assert_eq!(
        GeneralParameter::new(
            Type::from_scalar(ScalarType::Int8).unwrap(),
            Value::Int(128)
        )
        .unwrap_err()
        .gqlstatus()
        .unwrap()
        .as_str(),
        "22G03"
    );

    let list_type = Type::list(Type::INT64, None).unwrap();
    assert!(GeneralParameter::new(list_type.clone(), Value::List(vec![Value::Int(1)])).is_ok());
    assert!(
        GeneralParameter::new(
            list_type,
            Value::List(vec![Value::String(db_string("wrong").unwrap())])
        )
        .is_err()
    );

    let record = Value::Record(Box::new(Record::Open(
        [(db_string("answer").unwrap(), Value::Int(42))]
            .into_iter()
            .collect(),
    )));
    assert!(
        GeneralParameter::new(Type::new(TypeKind::Record(None), true).unwrap(), record).is_ok()
    );
    assert!(
        GeneralParameter::new(
            Type::VECTOR,
            Value::Vector(VectorValue::new(vec![1.0, 2.0]).unwrap())
        )
        .is_ok()
    );
    assert!(
        GeneralParameter::new(
            Type::JSON,
            Value::Json(JsonValue::parse_str(r#"{"kind":"request"}"#).unwrap())
        )
        .is_ok()
    );
}

#[test]
fn request_overlay_is_sorted_request_wins_and_session_state_is_unchanged() {
    let session = session();
    assert_eq!(session.context().parameters().len(), 0);
    assert!(
        session
            .set_parameter("shared", parameter(GqlType::Integer, Value::Int(1)))
            .unwrap()
            .is_none()
    );
    session
        .set_parameter("zeta", parameter(GqlType::Integer, Value::Int(9)))
        .unwrap();
    assert_eq!(session.context().parameters().len(), 2);

    let replaced = session
        .set_parameter("shared", parameter(GqlType::Integer, Value::Int(2)))
        .unwrap()
        .unwrap();
    assert_eq!(replaced.value(), &Value::Int(1));

    let mut request_params = RequestParams::new();
    request_params
        .insert("alpha", parameter(GqlType::Integer, Value::Int(3)))
        .unwrap();
    request_params
        .insert("shared", parameter(GqlType::Integer, Value::Int(4)))
        .unwrap();
    let outcome = session.execute_request(Request::with_params("RETURN $shared", request_params));
    let RequestOutcome::Succeeded { context, .. } = outcome else {
        panic!("request succeeds");
    };
    assert_eq!(
        context
            .parameters()
            .iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["alpha", "shared", "zeta"]
    );
    assert_eq!(
        context.parameters().get("shared").unwrap().value(),
        &Value::Int(4)
    );
    assert!(
        session.context().current_request().is_none(),
        "completed context is retained by the outcome, not the active slot"
    );
    let inherited = session.execute_request(Request::new("RETURN $shared"));
    assert_eq!(
        inherited
            .context()
            .parameters()
            .get("shared")
            .unwrap()
            .value(),
        &Value::Int(2)
    );

    assert_eq!(
        session.remove_parameter("shared").unwrap().unwrap().value(),
        &Value::Int(2)
    );
    assert_eq!(session.context().parameters().len(), 1);
    assert!(session.remove_parameter("missing").unwrap().is_none());
}

#[test]
fn structural_parameter_descriptors_ignore_synonyms_and_field_order() {
    let a = db_string("a").unwrap();
    let b = db_string("b").unwrap();
    let value = Value::Record(Box::new(Record::Open(
        [
            (a.clone(), Value::Int(7)),
            (b.clone(), Value::List(vec![Value::Null])),
        ]
        .into_iter()
        .collect(),
    )));
    let left = parameter(
        GqlType::Record(RecordType::Closed(vec![
            (a.clone(), GqlType::SmallInt),
            (b.clone(), GqlType::List(Box::new(GqlType::Integer))),
        ])),
        value.clone(),
    );
    let right = parameter(
        GqlType::Record(RecordType::Closed(vec![
            (b, GqlType::List(Box::new(GqlType::Int64))),
            (a, GqlType::Int16),
        ])),
        value,
    );
    assert_eq!(left.declared_type(), right.declared_type());
    let mut params = RequestParams::new();
    params.insert("p", left.clone()).unwrap();
    let output = session().execute_request(Request::with_params("RETURN $p AS item", params));
    let selene_db::ExecutionOutcome::Rows { result, .. } = output.execution().unwrap() else {
        panic!("typed parameter produces a regular result");
    };
    assert_eq!(result.rows()[0].values(), &[left.value().clone()]);
    assert_eq!(
        result.descriptor().fields()[0].declared_type(),
        &selene_db::DeclaredType::Resolved(left.declared_type().clone())
    );
}

#[test]
fn insertion_order_does_not_change_parameter_snapshot() {
    let make = |entries: [(&str, i64); 3]| {
        let mut params = RequestParams::new();
        for (name, value) in entries {
            params
                .insert(name, parameter(GqlType::Integer, Value::Int(value)))
                .unwrap();
        }
        params
    };

    assert_eq!(
        make([("c", 3), ("a", 1), ("b", 2)]),
        make([("b", 2), ("c", 3), ("a", 1)])
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn generated_overlays_are_ordered_request_wins_and_leave_session_unchanged(
        session_values in prop::collection::btree_map(0_u8..12, any::<i16>(), 0..9),
        request_values in prop::collection::btree_map(0_u8..12, any::<i16>(), 0..9),
        session_ranks in prop::collection::vec(any::<u8>(), 12),
        request_ranks in prop::collection::vec(any::<u8>(), 12),
    ) {
        let session_first = ordered_entries(&session_values, &session_ranks);
        let request_first = ordered_entries(&request_values, &request_ranks);
        let mut session_second = session_first.clone();
        let mut request_second = request_first.clone();
        session_second.reverse();
        request_second.reverse();

        let expected_session = session_values
            .iter()
            .map(|(&key, &value)| (format!("p{key}"), i64::from(value)))
            .collect::<BTreeMap<_, _>>();
        let mut expected_merged = expected_session.clone();
        expected_merged.extend(
            request_values
                .iter()
                .map(|(&key, &value)| (format!("p{key}"), i64::from(value))),
        );
        let expected_session = expected_session.into_iter().collect::<Vec<_>>();
        let expected_merged = expected_merged.into_iter().collect::<Vec<_>>();

        let first = execute_overlay(&session_first, &request_first);
        let second = execute_overlay(&session_second, &request_second);
        prop_assert_eq!(&first.merged, &expected_merged);
        prop_assert_eq!(&second.merged, &expected_merged);
        prop_assert_eq!(first.merged, second.merged);
        prop_assert_eq!(first.inherited, expected_session.clone());
        prop_assert_eq!(second.inherited, expected_session);
        prop_assert_eq!(first.session_count, session_values.len() as u64);
        prop_assert_eq!(second.session_count, session_values.len() as u64);
    }
}
