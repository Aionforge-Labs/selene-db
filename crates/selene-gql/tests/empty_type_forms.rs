//! Internal empty-type normalization does not admit optional source syntax.

use selene_gql::{GqlType, ParserError, normalize_value_type, parse};

#[test]
fn internal_empty_type_normal_forms_are_identical() {
    let empty = normalize_value_type(&GqlType::Nothing).unwrap();
    assert_eq!(
        normalize_value_type(&GqlType::NotNull(Box::new(GqlType::Null))).unwrap(),
        empty
    );
    assert_eq!(empty, selene_core::StructuralType::EMPTY);
    assert!(!empty.matches(&selene_core::Value::Null));
}

#[test]
fn empty_and_null_type_source_forms_remain_unsupported() {
    for source in [
        "RETURN NULL IS TYPED NULL NOT NULL AS ok",
        "RETURN [] IS TYPED NULL NOT NULL ARRAY AS ok",
        "RETURN [] IS TYPED NULL NOT NULL ARRAY NOT NULL AS ok",
        "RETURN [NULL] IS TYPED NULL NOT NULL ARRAY AS ok",
        "RETURN NULL IS TYPED NOTHING AS ok",
    ] {
        let error = parse(source).unwrap_err();
        assert!(
            matches!(error, ParserError::UnsupportedFeature { .. }),
            "{source}: {error:?}"
        );
        assert_eq!(error.gqlstatus().as_str(), "42N01");
    }
}

#[test]
fn redundant_nothing_not_null_is_rejected_as_syntax() {
    for source in [
        "RETURN NULL IS TYPED NOTHING NOT NULL AS ok",
        "RETURN [] IS TYPED NOTHING NOT NULL ARRAY AS ok",
        "RETURN [] IS TYPED LIST<NOTHING NOT NULL> AS ok",
    ] {
        assert!(
            matches!(parse(source).unwrap_err(), ParserError::SyntaxError { .. }),
            "{source}"
        );
    }
}
