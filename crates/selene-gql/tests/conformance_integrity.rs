//! BRIEF-125 conformance integrity regression coverage.

use selene_core::{CoreError, NodeId, gqlstatus_name};
use selene_gql::{GqlStatus, ParserError};
use selene_graph::GraphError;
use selene_persist::PersistError;
use selene_profile::{CapabilityStatus, FeatureId, capabilities, capability};

fn supported(feature: FeatureId) -> bool {
    capability(feature).is_some_and(|record| record.status == CapabilityStatus::Supported)
}

fn has_rationale(feature: FeatureId) -> bool {
    capability(feature).is_some_and(|record| !record.non_support_rationale.is_empty())
}

#[test]
fn graph_and_schema_management_are_runtime_supported_and_closed_types_are_not() {
    for feature in [
        FeatureId::GC01,
        FeatureId::GC02,
        FeatureId::GC04,
        FeatureId::GC05,
        FeatureId::GG01,
    ] {
        assert!(supported(feature), "{feature} must be runtime-supported");
        assert!(!has_rationale(feature), "{feature}");
    }
    assert!(!supported(FeatureId::GG02));
    assert!(has_rationale(FeatureId::GG02));
    let source = "CREATE GRAPH demo ANY AS COPY OF source";
    let error = selene_gql::parse(source).expect_err(source);
    assert_eq!(error.gqlstatus().as_str(), "42N01");
    let ParserError::UnsupportedFeature { feature_id, .. } = error else {
        panic!("expected UnsupportedFeature for {source:?}");
    };
    assert_eq!(feature_id, FeatureId::GG05);
}

#[test]
fn path_mode_features_are_runtime_supported() {
    for feature in [
        FeatureId::G010,
        FeatureId::G011,
        FeatureId::G012,
        FeatureId::G013,
    ] {
        assert!(supported(feature));
        assert!(!has_rationale(feature));
    }
}

#[test]
fn record_type_features_are_runtime_supported() {
    // GV45 (record types umbrella / value form) shipped earlier; GV46 (closed), GV47
    // (open), GV48 (nested) record TYPES ship with the typed/closed RECORD grammar.
    for feature in [
        FeatureId::GV45,
        FeatureId::GV46,
        FeatureId::GV47,
        FeatureId::GV48,
    ] {
        assert!(supported(feature), "{feature} must be runtime-supported");
        assert!(
            !has_rationale(feature),
            "{feature} must not retain a non-support rationale"
        );
    }
}

#[test]
fn deferred_reference_value_type_features_remain_unsupported() {
    // GRAPH/TABLE reference types stay deferred.
    for feature in [FeatureId::GV60, FeatureId::GV61] {
        assert!(!supported(feature), "{feature}");
        assert!(has_rationale(feature), "{feature}");
    }
}

#[test]
fn explicit_value_type_nullability_is_runtime_supported() {
    assert!(supported(FeatureId::GV90));
    assert!(!has_rationale(FeatureId::GV90));
}

#[test]
fn supported_features_do_not_carry_non_support_rationales() {
    for record in capabilities() {
        if record.status == CapabilityStatus::Supported {
            assert!(
                record.non_support_rationale.is_empty(),
                "{} is supported but has a non-support rationale",
                record.id
            );
        }
    }
}

#[test]
fn quantifier_features_are_runtime_supported() {
    for feature in [
        FeatureId::G036,
        FeatureId::G037,
        FeatureId::G060,
        FeatureId::G061,
    ] {
        assert!(supported(feature));
        assert!(!has_rationale(feature));
    }
}

#[test]
fn literal_features_are_runtime_supported() {
    for feature in [
        FeatureId::GL01,
        FeatureId::GL02,
        FeatureId::GL03,
        FeatureId::GL04,
        FeatureId::GL05,
        FeatureId::GL06,
        FeatureId::GL07,
        FeatureId::GL08,
        FeatureId::GL09,
        FeatureId::GL10,
        FeatureId::GL11,
    ] {
        assert!(supported(feature));
        assert!(!has_rationale(feature));
    }
}

#[test]
fn approximate_numeric_type_features_are_runtime_supported() {
    for feature in [
        FeatureId::GV21,
        FeatureId::GV22,
        FeatureId::GV23,
        FeatureId::GV24,
    ] {
        assert!(supported(feature));
        assert!(!has_rationale(feature));
    }
}

#[test]
fn match_mode_features_are_runtime_supported() {
    // ISO 39075:2024 §16.4 CR1/CR2: G002 (DIFFERENT EDGES) and G003 (REPEATABLE
    // ELEMENTS) are runtime-supported and must not carry non-support rationales.
    for feature in [FeatureId::G002, FeatureId::G003] {
        assert!(supported(feature));
        assert!(!has_rationale(feature));
    }
}

#[test]
fn sql_drift_status_remaps_are_registered() {
    assert_eq!(GqlStatus::IMPLEMENTATION_DEFINED_ERROR.as_str(), "5GQL0");
    assert_eq!(GqlStatus::IMPLEMENTATION_DEFINED_ERROR.class(), *b"5G");
    assert!(gqlstatus_name("5GQL0").is_some());
    assert!(gqlstatus_name(&["XX", "500"].concat()).is_none());
    assert!(gqlstatus_name(&["220", "23"].concat()).is_none());

    assert_eq!(
        CoreError::StringTooLong { got: 2, max: 1 }.gqlstatus(),
        "22G03"
    );
    assert_eq!(
        GraphError::NodeNotFound { id: NodeId::new(1) }.gqlstatus(),
        "22G03"
    );
    assert_eq!(
        GraphError::Inconsistent {
            reason: "test".to_owned(),
        }
        .gqlstatus(),
        "5GQL0"
    );
    assert_eq!(
        PersistError::Control(selene_persist::ControlError::Checksum).gqlstatus(),
        "5GQL0"
    );
}
