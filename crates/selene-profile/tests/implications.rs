//! Table 10 inventory, target selection, and dependency-validation regressions.

use std::collections::{BTreeMap, BTreeSet};

use selene_profile::{
    ClaimState, RuntimeSupport, TARGET_FEATURE_CLOSURE, parse_profile, render_outputs,
};
use serde_json::{Value, json};

const SOURCE: &str = include_str!("../../../spec/gql-profile/profile.json");
const BASE_SUPPORTED: &str = include_str!("fixtures/m01_pr01_supported.txt");

const DOWNGRADED: &[&str] = &[
    "GC03", "GE04", "GE05", "GG02", "GG20", "GG21", "GV66", "GV67",
];

const PROMOTED: &[&str] = &["GS05", "GS06"];

const IMPORTED: &[(&str, &str)] = &[
    ("G030", "Null predicates"),
    ("G031", "Non-nullable value types"),
    ("G032", "Truth value operations"),
    ("G033", "Extended truth value operations"),
    ("G038", "Regular value specification"),
    ("G039", "Extended value specification"),
    ("G041", "Basic binding table expressions"),
    ("G048", "Path value constructors"),
    ("G049", "Record value constructors"),
    ("G050", "Field references"),
    ("G051", "Field dereferences"),
    ("G080", "Let value variable statement"),
    ("G081", "Let value variable statement: value expression"),
    ("G082", "Let value variable statement: initializer"),
    ("GA04", "Binding table function value expressions"),
    ("GA09", "Binding table function WHERE clause"),
    ("GD03", "Insert and replace statements"),
    ("GD04", "Insert statement"),
    ("GG03", "Graph type inline specification"),
    ("GG04", "Graph type like a graph"),
    ("GG05", "Graph from a graph source"),
    ("GG22", "Non-abstract element types"),
    ("GG23", "Abstract element types"),
    ("GP17", "Return statement"),
    ("GQ01", "USE graph clause"),
    ("GV65", "Dynamic union types"),
    ("GV70", "Path type"),
    ("GV71", "Open path types"),
    ("GV72", "Closed path types"),
];

const TABLE_10: &[(&str, &str, u16)] = &[
    ("G031", "G030", 517),
    ("G033", "G032", 517),
    ("G039", "G080", 517),
    ("G041", "G051", 517),
    ("G045", "G044", 517),
    ("G048", "G038", 517),
    ("G049", "G038", 517),
    ("G050", "G038", 517),
    ("G051", "G050", 517),
    ("G061", "G060", 517),
    ("G081", "G082", 518),
    ("G082", "G080", 518),
    ("GA04", "GA09", 518),
    ("GC02", "GC01", 518),
    ("GC03", "GG02", 518),
    ("GC05", "GC04", 518),
    ("GD01", "GT01", 518),
    ("GD03", "GD04", 518),
    ("GE04", "GV60", 518),
    ("GE05", "GV61", 518),
    ("GE06", "GV55", 518),
    ("GF04", "GV55", 518),
    ("GF07", "GV35", 518),
    ("GG01", "GC04", 518),
    ("GG02", "GC04", 518),
    ("GG03", "GG02", 518),
    ("GG04", "GG02", 518),
    ("GG05", "GC04", 518),
    ("GG20", "GG02", 518),
    ("GG21", "GG02", 518),
    ("GG22", "GG02", 518),
    ("GG23", "GG02", 518),
    ("GP02", "GP01", 518),
    ("GP03", "GP01", 519),
    ("GP05", "GP17", 519),
    ("GP06", "GP05", 519),
    ("GP07", "GP05", 519),
    ("GP08", "GP17", 519),
    ("GP08", "GV61", 519),
    ("GP09", "GP08", 519),
    ("GP10", "GP08", 519),
    ("GP11", "GP17", 519),
    ("GP11", "GV60", 519),
    ("GP12", "GP11", 519),
    ("GP13", "GP11", 519),
    ("GQ05", "GQ04", 519),
    ("GQ07", "GQ06", 519),
    ("GQ11", "GQ10", 519),
    ("GS04", "GS05", 519),
    ("GS04", "GS06", 520),
    ("GS04", "GS07", 520),
    ("GS04", "GS08", 520),
    ("GS07", "GV40", 520),
    ("GS10", "GS02", 520),
    ("GS10", "GS13", 520),
    ("GS11", "GS03", 520),
    ("GS11", "GS14", 520),
    ("GS15", "GV40", 520),
    ("GT03", "GQ01", 520),
    ("GV36", "GV35", 520),
    ("GV37", "GV35", 520),
    ("GV38", "GV35", 520),
    ("GV40", "GV39", 520),
    ("GV41", "GV40", 520),
    ("GV46", "GV45", 520),
    ("GV47", "GV45", 520),
    ("GV48", "GV45", 521),
    ("GV66", "GV65", 521),
    ("GV67", "GV65", 521),
    ("GV71", "GV70", 521),
    ("GV72", "GV70", 521),
];

fn source_value() -> Value {
    serde_json::from_str(SOURCE).expect("checked-in profile is JSON")
}

fn parse_value(value: &Value) -> Result<selene_profile::ValidatedProfile, String> {
    parse_profile(&serde_json::to_string(value).expect("fixture serializes"))
        .map_err(|error| error.to_string())
}

fn feature_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    value["features"]
        .as_array_mut()
        .expect("features")
        .iter_mut()
        .find(|feature| feature["id"] == id)
        .expect("known feature")
}

fn add_test_evidence(value: &mut Value) {
    value["evidence"]
        .as_array_mut()
        .expect("evidence")
        .push(json!({
            "id": "EVID-TEST",
            "reference": "tests/implications.rs",
            "description": "Synthetic validation fixture"
        }));
}

fn claim_with_evidence(value: &mut Value, id: &str) {
    let feature = feature_mut(value, id);
    feature["claim_state"] = json!("claimed");
    feature["evidence"] = json!(["EVID-TEST"]);
}

fn resolve_annex_b_for_release_fixture(value: &mut Value) {
    for record in value["implementation_defined_choices"]
        .as_array_mut()
        .expect("Annex B records")
    {
        match record["decision"]["disposition"].as_str() {
            Some("pending") => {
                record["decision"] = json!({
                    "disposition": "selected",
                    "value": {"type": "boolean", "value": false},
                    "rationale": "Synthetic release fixture selection.",
                    "stability": "stable",
                    "visibility": "internal"
                });
            }
            Some("selected") => record["decision"]["stability"] = json!("stable"),
            Some("not_applicable") => {}
            other => panic!("unexpected decision {other:?}"),
        }
    }
}

fn set_runtime_unsupported(value: &mut Value, id: &str) {
    let feature = feature_mut(value, id);
    feature["runtime_support"] = json!("unsupported");
    feature["claim_state"] = json!("unsupported");
    feature["unsupported_rationale"] = json!("Synthetic dependency downgrade.");
    value["supported_feature_order"]
        .as_array_mut()
        .expect("supported order")
        .retain(|item| item != id);
    value["unsupported_feature_order"]
        .as_array_mut()
        .expect("unsupported order")
        .push(json!(id));
}

fn set_runtime_supported(value: &mut Value, id: &str) {
    let feature = feature_mut(value, id);
    feature["runtime_support"] = json!("supported");
    feature["claim_state"] = json!("implemented_unclaimed");
    feature["unsupported_rationale"] = json!("");
    value["unsupported_feature_order"]
        .as_array_mut()
        .expect("unsupported order")
        .retain(|item| item != id);
    value["supported_feature_order"]
        .as_array_mut()
        .expect("supported order")
        .push(json!(id));
}

#[test]
fn table_10_golden_pins_edges_endpoints_ids_and_pages() {
    let profile = parse_profile(SOURCE).expect("profile validates");
    let actual = profile
        .profile()
        .implications
        .iter()
        .map(|edge| {
            assert_eq!(
                edge.id.as_str(),
                format!("IMP-{}-{}", edge.source.as_str(), edge.target.as_str())
            );
            assert_eq!(edge.clause_anchors.len(), 1);
            let page = edge.clause_anchors[0]
                .as_str()
                .rsplit_once('P')
                .expect("page anchor")
                .1
                .parse::<u16>()
                .expect("numeric page");
            (edge.source.as_str(), edge.target.as_str(), page)
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, TABLE_10.iter().copied().collect());
    assert_eq!(actual.len(), 71);
    assert_eq!(
        actual
            .iter()
            .flat_map(|(source, target, _)| [*source, *target])
            .collect::<BTreeSet<_>>()
            .len(),
        96
    );
    let pages = actual
        .iter()
        .fold(BTreeMap::new(), |mut counts, (_, _, page)| {
            *counts.entry(*page).or_insert(0) += 1;
            counts
        });
    assert_eq!(
        pages,
        BTreeMap::from([(517, 10), (518, 23), (519, 16), (520, 17), (521, 5)])
    );
}

#[test]
fn imported_endpoint_names_and_orders_are_exact() {
    let profile = parse_profile(SOURCE).expect("profile validates");
    // GC01 was imported as a referenced endpoint and is now runtime-supported
    // (M02-PR04 part 1); it keeps its imported runtime order.
    let gc01 = profile
        .profile()
        .features
        .iter()
        .find(|feature| feature.id.as_str() == "GC01")
        .expect("GC01 is present");
    assert_eq!(gc01.runtime_order, 252);
    assert_eq!(gc01.name, "Graph schema management");
    assert_eq!(gc01.runtime_support, RuntimeSupport::Supported);
    let imported = profile
        .profile()
        .features
        .iter()
        .filter(|feature| {
            feature.runtime_order >= 234
                && !["GC01", "G043", "G044", "G045", "GP16"].contains(&feature.id.as_str())
        })
        .map(|feature| {
            if feature.id.as_str() == "GQ01" {
                assert_eq!(feature.runtime_support, RuntimeSupport::Unsupported);
                assert!(!feature.unsupported_rationale.is_empty());
            } else {
                assert_eq!(feature.runtime_support, RuntimeSupport::Referenced);
                assert!(feature.unsupported_rationale.is_empty());
            }
            assert_eq!(feature.claim_state, ClaimState::Unsupported);
            (feature.id.as_str(), feature.name.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(imported, IMPORTED);
    assert_eq!(
        profile
            .profile()
            .features
            .iter()
            .filter(|feature| feature.runtime_order >= 234
                && !["G043", "GP16"].contains(&feature.id.as_str()))
            .map(|feature| feature.runtime_order)
            .collect::<Vec<_>>(),
        (234..=265).collect::<Vec<_>>()
    );
}

#[test]
fn direct_target_and_surviving_compatibility_order_preserve_m01_pr01() {
    let profile = parse_profile(SOURCE).expect("profile validates");
    let base = BASE_SUPPORTED.lines().collect::<Vec<_>>();
    assert_eq!(base.len(), 147);
    let downgraded = DOWNGRADED.iter().copied().collect::<BTreeSet<_>>();
    // F01-PR04 adds the complete spelling family and restores GH02 support;
    // the relative order of all previously surviving capabilities is unchanged.
    let mut expected_survivors = vec!["G043", "G044", "G045", "GH02"];
    for id in base.iter().copied() {
        if !downgraded.contains(id) && id != "IM_DROP_GRAPH" && id != "GH02" {
            expected_survivors.push(id);
            if id == "GS04" {
                expected_survivors.extend(PROMOTED.iter().copied());
            }
        }
    }
    assert_eq!(
        profile
            .profile()
            .supported_feature_order
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        expected_survivors
    );

    let mut expected_iso = base
        .iter()
        .copied()
        .filter(|id| !id.starts_with("IM_"))
        .collect::<BTreeSet<_>>();
    assert_eq!(expected_iso.len(), 136);
    expected_iso.extend(PROMOTED.iter().copied());
    expected_iso.extend(["G043", "G044", "G045"]);
    // F03-PR01 selects bounded ISO scope admission, not complete runtime support.
    expected_iso.extend(["GP16", "GQ01"]);
    assert_eq!(
        profile
            .profile()
            .selected_features
            .iter()
            .map(|id| id.as_str())
            .collect::<BTreeSet<_>>(),
        expected_iso
    );
}

#[test]
fn exact_runtime_downgrades_are_pinned_and_mutations_are_detected() {
    let profile = parse_profile(SOURCE).expect("profile validates");
    let base_iso = BASE_SUPPORTED
        .lines()
        .filter(|id| !id.starts_with("IM_"))
        .collect::<BTreeSet<_>>();
    let current_supported = profile
        .profile()
        .features
        .iter()
        .filter(|feature| feature.runtime_support == RuntimeSupport::Supported)
        .map(|feature| feature.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        base_iso
            .difference(&current_supported)
            .copied()
            .collect::<BTreeSet<_>>(),
        DOWNGRADED.iter().copied().collect()
    );
    for id in DOWNGRADED {
        let mut value = source_value();
        value["features"]
            .as_array_mut()
            .expect("features")
            .iter_mut()
            .find(|feature| feature["id"] == **id)
            .expect("downgraded feature")["runtime_support"] = json!("supported");
        let error = parse_value(&value).unwrap_err();
        assert!(error.contains(id), "{id}: {error}");
    }
}

#[test]
fn closure_conflicts_reject_reenabling_runtime_support() {
    for (source, dependency) in [
        ("GC03", "GG02"),
        ("GE04", "GV60"),
        ("GE05", "GV61"),
        ("GG20", "GG02"),
        ("GG21", "GG02"),
        ("GV66", "GV65"),
        ("GV67", "GV65"),
    ] {
        let mut value = source_value();
        set_runtime_supported(&mut value, source);
        let error = parse_value(&value).unwrap_err();
        assert!(
            error.contains(&format!("runtime-supported source {source}")),
            "{error}"
        );
        assert!(error.contains(dependency), "{error}");
        assert!(error.contains("path"), "{error}");
    }
}

#[test]
fn unknown_duplicate_and_omitted_selected_features_fail() {
    let mut unknown = source_value();
    unknown["selected_features"][0] = json!("G999");
    assert!(
        parse_value(&unknown)
            .unwrap_err()
            .contains("selected_features references unknown feature ID G999")
    );

    let mut duplicate = source_value();
    let first = duplicate["selected_features"][0].clone();
    duplicate["selected_features"]
        .as_array_mut()
        .expect("selected")
        .push(first);
    assert!(
        parse_value(&duplicate)
            .unwrap_err()
            .contains("duplicate selected feature ID")
    );

    let mut omitted = source_value();
    omitted["selected_features"]
        .as_array_mut()
        .expect("selected")
        .retain(|id| id != "G002");
    assert!(
        parse_value(&omitted)
            .unwrap_err()
            .contains("runtime-supported feature G002 is omitted from selected_features")
    );
}

#[test]
fn duplicate_edge_and_table_edge_removal_are_detected() {
    let mut duplicate = source_value();
    let mut edge = duplicate["implications"][0].clone();
    edge["id"] = json!("IMP-DUPLICATE-PAIR");
    duplicate["implications"]
        .as_array_mut()
        .expect("implications")
        .push(edge);
    assert!(
        parse_value(&duplicate)
            .unwrap_err()
            .contains("duplicate implication edge")
    );

    let mut removed = source_value();
    removed["implications"]
        .as_array_mut()
        .expect("implications")
        .pop();
    let actual = removed["implications"]
        .as_array()
        .expect("implications")
        .iter()
        .map(|edge| {
            (
                edge["source"].as_str().unwrap(),
                edge["target"].as_str().unwrap(),
            )
        })
        .collect::<BTreeSet<_>>();
    let expected = TABLE_10
        .iter()
        .map(|(source, target, _)| (*source, *target))
        .collect::<BTreeSet<_>>();
    assert_ne!(
        actual, expected,
        "golden inventory must catch an edge removal"
    );
}

#[test]
fn runtime_transitive_dependency_diagnostic_uses_minimal_path() {
    let mut value = source_value();
    value["implications"]
        .as_array_mut()
        .expect("implications")
        .extend([
            json!({"id":"IMP-TEST-A","source":"G002","target":"G003","clause_anchors":[],"evidence":[]}),
            json!({"id":"IMP-TEST-B","source":"G003","target":"G010","clause_anchors":[],"evidence":[]}),
        ]);
    set_runtime_unsupported(&mut value, "G010");
    let error = parse_value(&value).unwrap_err();
    assert!(error.contains("runtime-supported source G002"), "{error}");
    assert!(error.contains("transitive dependency G010"), "{error}");
    assert!(error.contains("path G002 -> G003 -> G010"), "{error}");
}

#[test]
fn claimed_dependencies_require_claims_evidence_and_minimal_paths() {
    let mut unclaimed = source_value();
    add_test_evidence(&mut unclaimed);
    unclaimed["implications"]
        .as_array_mut()
        .expect("implications")
        .push(json!({"id":"IMP-TEST-CLAIM","source":"G002","target":"G003","clause_anchors":[],"evidence":[]}));
    claim_with_evidence(&mut unclaimed, "G002");
    let error = parse_value(&unclaimed).unwrap_err();
    assert!(error.contains("claimed source G002"), "{error}");
    assert!(error.contains("direct dependency G003"), "{error}");
    assert!(error.contains("path G002 -> G003"), "{error}");

    let mut incomplete = unclaimed;
    feature_mut(&mut incomplete, "G003")["claim_state"] = json!("claimed");
    let error = parse_value(&incomplete).unwrap_err();
    assert!(error.contains("evidence=incomplete"), "{error}");

    let mut transitive = source_value();
    add_test_evidence(&mut transitive);
    transitive["implications"]
        .as_array_mut()
        .expect("implications")
        .extend([
            json!({"id":"IMP-TEST-CLAIM-A","source":"G002","target":"G003","clause_anchors":[],"evidence":[]}),
            json!({"id":"IMP-TEST-CLAIM-B","source":"G003","target":"G010","clause_anchors":[],"evidence":[]}),
        ]);
    claim_with_evidence(&mut transitive, "G002");
    claim_with_evidence(&mut transitive, "G003");
    let error = parse_value(&transitive).unwrap_err();
    assert!(error.contains("transitive dependency G010"), "{error}");
    assert!(error.contains("path G002 -> G003 -> G010"), "{error}");
}

#[test]
fn claimed_source_and_release_claimable_require_complete_evidence() {
    let mut claimed = source_value();
    feature_mut(&mut claimed, "G002")["claim_state"] = json!("claimed");
    assert!(
        parse_value(&claimed)
            .unwrap_err()
            .contains("claimed feature G002 has incomplete evidence")
    );

    let mut release = source_value();
    release["release_claimable"] = json!(true);
    let error = parse_value(&release).unwrap_err();
    assert!(error.contains("pending decision IA003"), "{error}");

    resolve_annex_b_for_release_fixture(&mut release);
    let error = parse_value(&release).unwrap_err();
    assert!(
        error.contains("release_claimable profile selene-gql-core-2.0"),
        "{error}"
    );
    assert!(error.contains("directly selected G002"), "{error}");
    assert!(error.contains("path G002"), "{error}");
}

#[test]
fn generated_claim_matrix_pins_counts_blockers_and_boundary() {
    let profile = parse_profile(SOURCE).expect("profile validates");
    let markdown = render_outputs(&profile)
        .expect("outputs render")
        .into_iter()
        .find(|(path, _)| path == std::path::Path::new("docs/gql/conformance/features.md"))
        .expect("claim matrix")
        .1;
    assert!(markdown.contains("| Direct selections | 143 |"));
    assert!(markdown.contains("| Complete Table 10 closure | 146 |"));
    assert_eq!(markdown.matches("| direct selection | ").count(), 286);
    assert!(markdown.contains("| GC03 | GC04 | transitive dependency | GC03 → GG02 → GC04 |"));
    assert!(markdown.contains("71 direct all-of relationships"));
    assert!(markdown.contains("96 endpoint features"));
    assert!(markdown.contains("M01-PR05 owns those rules"));
}

#[test]
fn generated_claim_matrix_reports_a_valid_release_claimable_target() {
    let mut release = source_value();
    add_test_evidence(&mut release);
    resolve_annex_b_for_release_fixture(&mut release);
    release["selected_features"] = json!(
        TARGET_FEATURE_CLOSURE
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
    );
    for id in TARGET_FEATURE_CLOSURE {
        let id = id.as_str();
        let is_supported = release["features"]
            .as_array()
            .expect("features")
            .iter()
            .find(|feature| feature["id"] == id)
            .expect("target closure feature")["runtime_support"]
            == "supported";
        if !is_supported {
            set_runtime_supported(&mut release, id);
        }
        claim_with_evidence(&mut release, id);
    }
    release["release_claimable"] = json!(true);

    let profile = parse_value(&release).expect("release-claimable fixture validates");
    let markdown = render_outputs(&profile)
        .expect("outputs render")
        .into_iter()
        .find(|(path, _)| path == std::path::Path::new("docs/gql/conformance/features.md"))
        .expect("claim matrix")
        .1;
    assert!(
        markdown.contains("This target is release-claimable under the validated profile contract.")
    );
    assert!(!markdown.contains("This target is not release-claimable."));
    assert!(
        markdown
            .contains("this status does not establish minimum- or selected-profile conformance")
    );
}
