//! Boundary coverage for single-token scalar type-name keywords.

use selene_gql::{ParserError, parse};

#[test]
fn scalar_type_keywords_require_boundaries_before_type_suffixes() {
    for source in [
        "RETURN NULL IS TYPED UUIDARRAY AS ok",
        "RETURN NULL IS TYPED JSONARRAY AS ok",
        "RETURN NULL IS TYPED VECTORARRAY AS ok",
        "RETURN NULL IS TYPED BYTEAARRAY AS ok",
        "RETURN NULL IS TYPED PATHARRAY AS ok",
        "RETURN NULL IS TYPED NULLARRAY AS ok",
        "RETURN NULL IS TYPED NOTHINGARRAY AS ok",
        "RETURN NULL IS TYPED UUIDNOT NULL AS ok",
        "RETURN NULL IS TYPED JSONNOT NULL AS ok",
        "RETURN NULL IS TYPED VECTORNOT NULL AS ok",
        "RETURN NULL IS TYPED BYTEANOT NULL AS ok",
        "RETURN NULL IS TYPED PATHNOT NULL AS ok",
        "RETURN NULL IS TYPED NULLNOT NULL AS ok",
        "RETURN NULL IS TYPED NOTHINGNOT NULL AS ok",
    ] {
        assert_syntax_error(source);
    }
}

#[test]
fn scalar_type_keywords_accept_real_boundaries_before_type_suffixes() {
    for source in [
        "RETURN NULL IS TYPED UUID ARRAY AS ok",
        "RETURN NULL IS TYPED JSON ARRAY AS ok",
        "RETURN NULL IS TYPED VECTOR ARRAY AS ok",
        "RETURN NULL IS TYPED BYTEA ARRAY AS ok",
        "RETURN NULL IS TYPED PATH ARRAY AS ok",
        "RETURN NULL IS TYPED NULL ARRAY AS ok",
        "RETURN NULL IS TYPED NOTHING ARRAY AS ok",
        "RETURN NULL IS TYPED UUID /* boundary */ NOT NULL AS ok",
        "RETURN NULL IS TYPED JSON /* boundary */ NOT NULL AS ok",
        "RETURN NULL IS TYPED VECTOR /* boundary */ NOT NULL AS ok",
        "RETURN NULL IS TYPED BYTEA /* boundary */ NOT NULL AS ok",
        "RETURN NULL IS TYPED PATH /* boundary */ NOT NULL AS ok",
        "RETURN NULL IS TYPED NULL /* boundary */ NOT NULL AS ok",
    ] {
        if source.contains("TYPED NULL") || source.contains("TYPED NOTHING") {
            assert!(
                matches!(
                    parse(source).unwrap_err(),
                    ParserError::UnsupportedFeature { .. }
                ),
                "{source}"
            );
        } else {
            parse(source).unwrap_or_else(|err| panic!("{source:?} should parse: {err:?}"));
        }
    }
}

fn assert_syntax_error(source: &str) {
    let err = parse(source).expect_err("source should reject");
    assert!(
        matches!(err, ParserError::SyntaxError { .. }),
        "{source:?} should reject as syntax, got {err:?}"
    );
}
