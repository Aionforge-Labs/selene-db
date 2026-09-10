//! Profile format, validation, determinism, and freshness regression tests.

use std::collections::BTreeMap;
use std::path::PathBuf;

use selene_profile::{
    CapabilityClaimState, CapabilityStatus, ClaimState, DIRECT_SELECTED_FEATURES, EvidenceStatus,
    FeatureId, FeatureSurface, FlaggerStatus, PROFILE_FORMAT_VERSION, PROFILE_GENERATOR_VERSION,
    PROFILE_HASH, PROFILE_ID, ProfileRelation, RELEASE_CLAIMABLE, SessionUserDeclaredType,
    TARGET_FEATURE_CLOSURE, annex_b_records, capabilities, capability_by_id, check_repository,
    current_profile_identity, current_session_defaults, parse_profile, render_outputs,
    write_repository,
};
use serde_json::{Value, json};

const SOURCE: &str = include_str!("../../../spec/gql-profile/profile.json");
const SCHEMA: &str = include_str!("../../../spec/gql-profile/schema.json");

fn source_value() -> Value {
    serde_json::from_str(SOURCE).expect("checked-in profile is JSON")
}

fn parse_value(value: &Value) -> Result<selene_profile::ValidatedProfile, String> {
    parse_profile(&serde_json::to_string(value).expect("fixture serializes"))
        .map_err(|error| error.to_string())
}

fn feature_mut(value: &mut Value, index: usize) -> &mut serde_json::Map<String, Value> {
    value["features"][index]
        .as_object_mut()
        .expect("feature fixture is an object")
}

fn choice_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    value["implementation_defined_choices"]
        .as_array_mut()
        .expect("implementation-defined choices")
        .iter_mut()
        .find(|choice| choice["id"] == id)
        .expect("known implementation-defined ID")
}

#[test]
fn checked_in_profile_loads_and_preserves_seed_contract() {
    let profile = parse_profile(SOURCE).expect("checked-in profile validates");
    assert_eq!(profile.hash(), PROFILE_HASH);
    assert_eq!(profile.profile().features.len(), 199);
    assert_eq!(profile.profile().implementation_extensions.len(), 11);
    assert_eq!(capabilities().len(), 210);
    assert_eq!(
        capabilities()
            .iter()
            .filter(|record| record.status == CapabilityStatus::Supported)
            .count(),
        143
    );
    assert_eq!(
        capabilities()
            .iter()
            .filter(|record| record.status == CapabilityStatus::Unsupported)
            .count(),
        38
    );
    assert_eq!(
        capabilities()
            .iter()
            .filter(|record| record.flagger_status == FlaggerStatus::Accepted)
            .count(),
        153
    );
    assert_eq!(annex_b_records().count(), 117);
    assert_eq!(PROFILE_FORMAT_VERSION, 3);
    assert_eq!(PROFILE_GENERATOR_VERSION, 3);
    assert_eq!(PROFILE_ID, "selene-gql-core-2.0");
    assert_eq!(RELEASE_CLAIMABLE, profile.profile().release_claimable);
    assert!(!profile.profile().release_claimable);
    assert_eq!(DIRECT_SELECTED_FEATURES.len(), 143);
    assert_eq!(TARGET_FEATURE_CLOSURE.len(), 146);

    assert!(
        profile
            .profile()
            .features
            .iter()
            .all(|feature| feature.claim_state != ClaimState::Claimed)
    );
    assert!(
        profile
            .profile()
            .features
            .iter()
            .all(|feature| { !feature.id.as_str().starts_with("IM_") })
    );
}

#[test]
fn generated_runtime_records_cover_identity_status_relation_and_evidence() {
    let identity = current_profile_identity();
    assert_eq!(identity.profile_id(), PROFILE_ID);
    assert_eq!(identity.source_format_version(), PROFILE_FORMAT_VERSION);
    assert_eq!(identity.generator_version(), PROFILE_GENERATOR_VERSION);
    assert_eq!(identity.canonical_hash(), PROFILE_HASH);

    let source = parse_profile(SOURCE).expect("checked-in profile validates");
    let mut expected = source
        .profile()
        .features
        .iter()
        .map(|record| (record.runtime_order, record.id.as_str()))
        .chain(
            source
                .profile()
                .implementation_extensions
                .iter()
                .map(|record| (record.runtime_order, record.id.as_str())),
        )
        .collect::<Vec<_>>();
    expected.sort_unstable_by_key(|(order, _)| *order);
    assert_eq!(
        capabilities()
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        expected.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
    );

    let gp04 = capability_by_id("GP04").expect("GP04 capability");
    assert_eq!(gp04.id, FeatureId::GP04);
    assert_eq!(gp04.status, CapabilityStatus::Supported);
    assert_eq!(gp04.flagger_status, FlaggerStatus::Accepted);
    assert_eq!(gp04.surface, FeatureSurface::Iso);
    assert_eq!(gp04.profile_relation, ProfileRelation::Direct);
    assert_eq!(gp04.claim_state, CapabilityClaimState::ImplementedUnclaimed);
    assert_eq!(gp04.evidence_status, EvidenceStatus::Incomplete);
    assert_eq!(gp04.evidence_count, 0);

    for id in ["GV60", "GV65"] {
        assert_eq!(
            capability_by_id(id)
                .expect("implied capability")
                .profile_relation,
            ProfileRelation::Implied
        );
    }
    let gc04 = capability_by_id("GC04").expect("GC04 capability");
    assert_eq!(gc04.status, CapabilityStatus::Supported);
    assert_eq!(gc04.profile_relation, ProfileRelation::Direct);
    assert_eq!(gc04.flagger_status, FlaggerStatus::Accepted);
    assert_eq!(
        capability_by_id("GV65").expect("GV65 capability").status,
        CapabilityStatus::Referenced
    );
    assert!(
        capabilities()
            .iter()
            .filter(|record| record.status != CapabilityStatus::Supported)
            .all(|record| !record.non_support_rationale.is_empty())
    );
    let record = capability_by_id("GV66").expect("direct unsupported capability");
    assert_eq!(record.status, CapabilityStatus::Unsupported);
    assert_eq!(record.profile_relation, ProfileRelation::Direct);
    assert_eq!(record.flagger_status, FlaggerStatus::Accepted);
    assert!(!record.non_support_rationale.is_empty());
    for id in ["GS04", "GS05", "GS06", "G043", "G044", "G045", "GH02"] {
        let record = capability_by_id(id).expect("supported session capability");
        assert_eq!(record.status, CapabilityStatus::Supported);
        assert_eq!(record.profile_relation, ProfileRelation::Direct);
        assert_eq!(record.flagger_status, FlaggerStatus::Accepted);
    }

    let json = capability_by_id("IM_JSON").expect("IM_JSON capability");
    assert_eq!(json.status, CapabilityStatus::Supported);
    assert_eq!(json.flagger_status, FlaggerStatus::Accepted);
    assert_eq!(json.surface, FeatureSurface::Extension);
    assert_eq!(json.profile_relation, ProfileRelation::Extension);
    assert_eq!(json.claim_state, CapabilityClaimState::NotApplicable);

    for id in [
        "GC03", "GE04", "GE05", "GG02", "GG20", "GG21", "GV66", "GV67",
    ] {
        let record = capability_by_id(id).expect("direct parser-visible capability");
        assert_eq!(record.status, CapabilityStatus::Unsupported, "{id}");
        assert_eq!(record.profile_relation, ProfileRelation::Direct, "{id}");
        assert_eq!(record.flagger_status, FlaggerStatus::Accepted, "{id}");
    }

    for id in ["GG03", "GG04", "GG05", "GV65"] {
        assert_eq!(
            capability_by_id(id)
                .expect("implied capability")
                .flagger_status,
            FlaggerStatus::Rejected,
            "{id}",
        );
    }
    let unselected = capability_by_id("GV20").expect("GV20 capability");
    assert_eq!(unselected.status, CapabilityStatus::Unsupported);
    assert_eq!(unselected.profile_relation, ProfileRelation::Unselected);
    assert_eq!(unselected.flagger_status, FlaggerStatus::Rejected);
    for id in ["GV60", "GV61"] {
        let record = capability_by_id(id).expect("reference type capability");
        assert_eq!(record.status, CapabilityStatus::Unsupported, "{id}");
        assert_eq!(record.flagger_status, FlaggerStatus::Rejected, "{id}");
    }
}

#[test]
fn generated_session_defaults_are_typed_profile_values() {
    let defaults = current_session_defaults();
    assert_eq!(defaults.time_zone().seconds(), 0);
    assert_eq!(defaults.initial_parameter_count(), 0);
    assert_eq!(
        defaults.session_user_declared_type(),
        SessionUserDeclaredType::String
    );
}

#[test]
fn session_default_generation_rejects_incompatible_annex_b_shapes() {
    let mut invalid_offset = source_value();
    choice_mut(&mut invalid_offset, "ID048")["decision"]["value"] =
        json!({"type": "string", "value": "UTC+00:00"});
    let profile = parse_value(&invalid_offset).expect("generic Annex B value remains valid");
    let error = render_outputs(&profile).unwrap_err().to_string();
    assert!(error.contains("ID048 must select a fixed UTC displacement identifier"));

    let mut nonempty_parameters = source_value();
    choice_mut(&mut nonempty_parameters, "ID049")["decision"]["value"]["value"] = json!(1);
    let profile = parse_value(&nonempty_parameters).expect("unsigned count remains valid");
    let error = render_outputs(&profile).unwrap_err().to_string();
    assert!(error.contains("ID049 must select zero"));

    let mut wrong_user_type = source_value();
    choice_mut(&mut wrong_user_type, "ID061")["decision"]["value"]["value"] = json!("BYTES");
    let profile = parse_value(&wrong_user_type).expect("identifier choice remains valid");
    let error = render_outputs(&profile).unwrap_err().to_string();
    assert!(error.contains("ID061 must select STRING"));
}

#[test]
fn closed_decode_rejects_missing_and_extra_fields() {
    let mut missing = source_value();
    feature_mut(&mut missing, 0).remove("name");
    assert!(
        parse_value(&missing)
            .unwrap_err()
            .contains("missing field `name`")
    );

    let mut extra = source_value();
    feature_mut(&mut extra, 0).insert("surprise".to_owned(), json!(true));
    assert!(
        parse_value(&extra)
            .unwrap_err()
            .contains("unknown field `surprise`")
    );

    let mut missing_order = source_value();
    missing_order
        .as_object_mut()
        .expect("profile fixture is an object")
        .remove("supported_feature_order");
    assert!(
        parse_value(&missing_order)
            .unwrap_err()
            .contains("missing field `supported_feature_order`")
    );
}

#[test]
fn malformed_state_and_identifier_fail() {
    let mut state = source_value();
    feature_mut(&mut state, 0).insert("claim_state".to_owned(), json!("implemented"));
    let error = parse_value(&state).unwrap_err();
    assert!(error.contains("unknown variant `implemented`"), "{error}");

    let mut identity = source_value();
    feature_mut(&mut identity, 0).insert("id".to_owned(), json!("g002"));
    let error = parse_value(&identity).unwrap_err();
    assert!(error.contains("malformed feature ID g002"), "{error}");
}

#[test]
fn format_and_generator_versions_are_incompatible_boundaries() {
    let mut format = source_value();
    format["format_version"] = json!(1);
    assert!(
        parse_value(&format)
            .unwrap_err()
            .contains("format_version must be 3, got 1")
    );

    let mut generator = source_value();
    generator["generator_version"] = json!(1);
    assert!(
        parse_value(&generator)
            .unwrap_err()
            .contains("generator_version must be 3, got 1")
    );
}

#[test]
fn duplicate_ids_and_runtime_orders_fail() {
    let mut duplicate = source_value();
    let first = duplicate["features"][0].clone();
    duplicate["features"]
        .as_array_mut()
        .expect("features")
        .push(first);
    let error = parse_value(&duplicate).unwrap_err();
    assert!(error.contains("duplicate feature ID"), "{error}");

    let mut order = source_value();
    let first_order = order["features"][0]["runtime_order"].clone();
    feature_mut(&mut order, 1).insert("runtime_order".to_owned(), first_order);
    let error = parse_value(&order).unwrap_err();
    assert!(error.contains("duplicate runtime_order"), "{error}");
}

#[test]
fn supported_compatibility_order_is_unique_known_and_complete() {
    let mut duplicate = source_value();
    let first = duplicate["supported_feature_order"][0].clone();
    duplicate["supported_feature_order"]
        .as_array_mut()
        .expect("supported order")
        .push(first);
    let error = parse_value(&duplicate).unwrap_err();
    assert!(
        error.contains("duplicate supported compatibility ID"),
        "{error}"
    );

    let mut unknown = source_value();
    unknown["supported_feature_order"][0] = json!("IM_UNKNOWN");
    let error = parse_value(&unknown).unwrap_err();
    assert!(
        error.contains("supported_feature_order references unknown runtime ID IM_UNKNOWN"),
        "{error}"
    );

    let mut incomplete = source_value();
    incomplete["supported_feature_order"]
        .as_array_mut()
        .expect("supported order")
        .pop();
    let error = parse_value(&incomplete).unwrap_err();
    assert!(
        error.contains(
            "supported_feature_order must contain every runtime-supported feature and extension exactly once"
        ),
        "{error}"
    );
}

#[test]
fn extension_ids_must_be_rust_identifiers() {
    for invalid in ["IM_BAD-ID", "IM_BAD.ID"] {
        let mut value = source_value();
        value["implementation_extensions"][0]["id"] = json!(invalid);
        let error = parse_value(&value).unwrap_err();
        assert!(
            error.contains(&format!("malformed implementation extension ID {invalid}")),
            "{error}"
        );
    }
}

#[test]
fn unknown_implication_evidence_and_applicability_references_fail() {
    let mut implication = source_value();
    implication["implications"] = json!([{
        "id": "IMP-UNKNOWN",
        "source": "G002",
        "target": "G999",
        "clause_anchors": [],
        "evidence": []
    }]);
    let error = parse_value(&implication).unwrap_err();
    assert!(
        error.contains("references unknown feature ID G999"),
        "{error}"
    );

    let mut evidence = source_value();
    feature_mut(&mut evidence, 0).insert("evidence".to_owned(), json!(["EVID-MISSING"]));
    let error = parse_value(&evidence).unwrap_err();
    assert!(
        error.contains("references unknown evidence ID EVID-MISSING"),
        "{error}"
    );

    let mut applicability = source_value();
    feature_mut(&mut applicability, 0).insert("applicability".to_owned(), json!("APP-MISSING"));
    let error = parse_value(&applicability).unwrap_err();
    assert!(
        error.contains("references unknown applicability ID APP-MISSING"),
        "{error}"
    );
}

#[test]
fn other_dangling_references_fail() {
    let mut clause = source_value();
    feature_mut(&mut clause, 0).insert("clause_anchors".to_owned(), json!(["CLAUSE-MISSING"]));
    let error = parse_value(&clause).unwrap_err();
    assert!(
        error.contains("references unknown clause ID CLAUSE-MISSING"),
        "{error}"
    );

    let mut choice = source_value();
    choice["applicability"][0]["expression"] = json!({
        "kind": "implementation_defined",
        "choice_id": "IZ999"
    });
    let error = parse_value(&choice).unwrap_err();
    assert!(
        error.contains("references unknown implementation-defined ID IZ999"),
        "{error}"
    );
}

#[test]
fn implication_and_applicability_cycles_fail() {
    let mut implications = source_value();
    implications["implications"] = json!([
        {"id":"IMP-A","source":"G002","target":"G003","clause_anchors":[],"evidence":[]},
        {"id":"IMP-B","source":"G003","target":"G002","clause_anchors":[],"evidence":[]}
    ]);
    let error = parse_value(&implications).unwrap_err();
    assert!(
        error.contains("implication cycle: G002 -> G003 -> G002"),
        "{error}"
    );

    let mut applicability = source_value();
    applicability["applicability"]
        .as_array_mut()
        .expect("applicability definitions")
        .extend([
            json!({"id":"APP-A","expression":{"kind":"applicability","applicability_id":"APP-B"}}),
            json!({"id":"APP-B","expression":{"kind":"applicability","applicability_id":"APP-A"}}),
        ]);
    let error = parse_value(&applicability).unwrap_err();
    assert!(
        error.contains("applicability cycle: APP-A -> APP-B -> APP-A"),
        "{error}"
    );
}

#[test]
fn applicability_depth_is_bounded() {
    let mut value = source_value();
    let mut expression = json!({"kind":"always"});
    for _ in 0..66 {
        expression = json!({"kind":"not","item":expression});
    }
    value["applicability"][0]["expression"] = expression;
    let error = parse_value(&value).unwrap_err();
    assert!(error.contains("exceeds applicability depth 64"), "{error}");
}

#[test]
fn canonical_hash_and_outputs_ignore_semantic_reordering() {
    let mut left = source_value();
    left["applicability"][0]["expression"] = json!({
        "kind":"all",
        "items":[
            {"kind":"feature","feature_id":"G002"},
            {"kind":"feature","feature_id":"G003"}
        ]
    });
    let mut right = left.clone();
    right["features"]
        .as_array_mut()
        .expect("features")
        .reverse();
    right["implementation_defined_choices"]
        .as_array_mut()
        .expect("choices")
        .reverse();
    right["applicability"][0]["expression"]["items"]
        .as_array_mut()
        .expect("items")
        .reverse();

    let left = parse_value(&left).expect("left validates");
    let right = parse_value(&right).expect("right validates");
    assert_eq!(left.canonical_json(), right.canonical_json());
    assert_eq!(left.hash(), right.hash());
    assert_eq!(
        render_outputs(&left).unwrap(),
        render_outputs(&right).unwrap()
    );
}

#[test]
fn repeated_generation_is_byte_identical() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let source_path = root.join("spec/gql-profile/profile.json");
    std::fs::create_dir_all(source_path.parent().expect("source parent")).unwrap();
    std::fs::write(&source_path, SOURCE).unwrap();

    write_repository(root).expect("first generation");
    let profile = parse_profile(SOURCE).expect("source validates");
    let first = render_outputs(&profile)
        .unwrap()
        .into_iter()
        .map(|(path, _)| {
            let bytes = std::fs::read(root.join(&path)).unwrap();
            (path, bytes)
        })
        .collect::<BTreeMap<PathBuf, Vec<u8>>>();
    write_repository(root).expect("second generation");
    let second = first
        .keys()
        .map(|path| (path.clone(), std::fs::read(root.join(path)).unwrap()))
        .collect::<BTreeMap<PathBuf, Vec<u8>>>();
    assert_eq!(first, second);
}

#[test]
fn generated_capability_evidence_summary_tracks_registered_references() {
    let mut value = source_value();
    let gp04 = value["features"]
        .as_array_mut()
        .expect("features")
        .iter_mut()
        .find(|record| record["id"] == "GP04")
        .expect("GP04 record");
    gp04["evidence"] = json!(["EVID-FLAGGER"]);

    let profile = parse_value(&value).expect("evidence fixture validates");
    let feature_data = render_outputs(&profile)
        .expect("outputs render")
        .into_iter()
        .find(|(path, _)| {
            path == std::path::Path::new("crates/selene-profile/src/generated/feature_data.rs")
        })
        .expect("feature data output")
        .1;
    let gp04_line = feature_data
        .lines()
        .find(|line| line.contains("FeatureId::GP04"))
        .expect("GP04 generated record");
    assert!(gp04_line.contains("EvidenceStatus::Present"));
    assert!(gp04_line.contains("evidence_count: 1"));
}

#[test]
fn stale_generated_output_is_rejected() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let source_path = root.join("spec/gql-profile/profile.json");
    std::fs::create_dir_all(source_path.parent().expect("source parent")).unwrap();
    std::fs::write(&source_path, SOURCE).unwrap();
    write_repository(root).expect("initial generation");

    let stale = root.join("crates/selene-profile/src/generated/feature_data.rs");
    std::fs::write(&stale, "stale\n").unwrap();
    let error = check_repository(root).expect_err("stale output must fail");
    assert!(
        matches!(error, selene_profile::ProfileError::Stale(path) if path == std::path::Path::new("crates/selene-profile/src/generated/feature_data.rs"))
    );
}

#[test]
fn markdown_generation_escapes_table_content() {
    let mut value = source_value();
    let feature = value["features"]
        .as_array_mut()
        .expect("features")
        .iter_mut()
        .find(|item| item["id"] == "GC03")
        .expect("GC03 is present");
    feature["unsupported_rationale"] = json!("use <name> & left | right");

    let profile = parse_value(&value).expect("fixture validates");
    let markdown = render_outputs(&profile)
        .expect("outputs render")
        .into_iter()
        .find(|(path, _)| path == std::path::Path::new("spec/gql-profile/registry.md"))
        .expect("Markdown output")
        .1;
    assert!(markdown.contains("use &lt;name&gt; &amp; left \\| right"));
}

#[test]
fn checked_in_outputs_are_fresh() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    check_repository(&root).expect("run the documented --write command");
}

#[test]
fn schema_closes_every_object_rule() {
    fn visit(value: &Value, path: &str) {
        match value {
            Value::Object(object) => {
                if object.get("type") == Some(&Value::String("object".to_owned())) {
                    assert_eq!(
                        object.get("additionalProperties"),
                        Some(&Value::Bool(false)),
                        "open object schema at {path}"
                    );
                    let required = object["required"]
                        .as_array()
                        .expect("object schema declares required fields")
                        .iter()
                        .map(|item| item.as_str().expect("required field is text"))
                        .collect::<std::collections::BTreeSet<_>>();
                    let properties = object["properties"]
                        .as_object()
                        .expect("object schema declares properties")
                        .keys()
                        .map(String::as_str)
                        .collect::<std::collections::BTreeSet<_>>();
                    assert_eq!(
                        required, properties,
                        "optional or undeclared field at {path}"
                    );
                }
                for (key, child) in object {
                    visit(child, &format!("{path}/{key}"));
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    visit(child, &format!("{path}/{index}"));
                }
            }
            _ => {}
        }
    }

    let schema: Value = serde_json::from_str(SCHEMA).expect("schema is JSON");
    visit(&schema, "#");
    assert_eq!(
        schema["$defs"]["extension_id"]["pattern"],
        "^IM_[A-Z0-9_]+$"
    );
    assert_eq!(
        schema["$id"],
        "https://selene-db.dev/schema/gql-profile-v3.json"
    );
    assert_eq!(schema["properties"]["format_version"]["const"], 3);
    assert_eq!(schema["properties"]["generator_version"]["const"], 3);
    assert_eq!(
        schema["properties"]["selected_features"]["uniqueItems"],
        true
    );
}
