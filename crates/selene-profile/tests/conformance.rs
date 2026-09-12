//! Static conformance registry validation tests.

use std::path::Path;

use selene_profile::{
    EvidenceDisposition, FeatureScope, InventoryState, load_conformance, parse_conformance,
    parse_profile,
};
use serde_json::{Value, json};

const PROFILE: &str = include_str!("../../../spec/gql-profile/profile.json");
const RULES: &str = include_str!("../../../spec/gql-profile/rules.json");
const EVIDENCE: &str = include_str!("../../../spec/gql-profile/evidence.json");

fn sources() -> (Value, Value) {
    (
        serde_json::from_str(RULES).expect("rules JSON"),
        serde_json::from_str(EVIDENCE).expect("evidence JSON"),
    )
}

fn parse(rules: &Value, evidence: &Value) -> Result<selene_profile::ValidatedConformance, String> {
    let profile = parse_profile(PROFILE).expect("profile validates");
    parse_conformance(&rules.to_string(), &evidence.to_string(), &profile)
        .map_err(|error| error.to_string())
}

#[test]
fn checked_in_seed_pins_static_boundary_and_pending_ownership() {
    let profile = parse_profile(PROFILE).expect("profile validates");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let registry = load_conformance(&root, &profile).expect("static registries validate");
    assert_eq!(registry.rules().rules.len(), 9);
    assert_eq!(registry.evidence().evidence.len(), 5);
    assert_eq!(
        registry.rules().inventory_state,
        InventoryState::SeededIncomplete
    );
    let FeatureScope::ProfileTargetClosure {
        expected_count,
        feature_ids_hash,
    } = &registry.rules().target;
    assert_eq!(*expected_count, 146);
    assert_eq!(
        feature_ids_hash,
        "f85e7ad43373806bc64da8b34aa890909402efe671f0a8d29e4aedb5811c7053"
    );
    assert_eq!(
        registry
            .rules()
            .approved_domains
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "CLAUSE-24.2",
            "CLAUSE-24.3",
            "CLAUSE-24.5",
            "CLAUSE-24.6",
            "CLAUSE-24.7",
            "CLAUSE-24.7-TABLE10-P517",
            "CLAUSE-24.7-TABLE10-P518",
            "CLAUSE-24.7-TABLE10-P519",
            "CLAUSE-24.7-TABLE10-P520",
            "CLAUSE-24.7-TABLE10-P521",
            "CLAUSE-ANNEX-B",
        ]
    );
    assert_eq!(
        registry
            .rules()
            .rules
            .iter()
            .find(|rule| rule.id == "RULE-24.7-002")
            .unwrap()
            .clause_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "CLAUSE-24.7-TABLE10-P517",
            "CLAUSE-24.7-TABLE10-P518",
            "CLAUSE-24.7-TABLE10-P519",
            "CLAUSE-24.7-TABLE10-P520",
            "CLAUSE-24.7-TABLE10-P521",
        ]
    );
    assert_eq!(registry.evidence().profile_hash, profile.hash());
    assert_eq!(registry.evidence().rules_hash, registry.rules_hash());
    assert_eq!(
        registry.rules_hash(),
        "e97dd73391a5ad423d006087eeac063e802582c3403a039222045704829a8a94"
    );
    assert_eq!(
        registry.evidence_hash(),
        "34e0e9afc2cfacf321c8ed6f3a6607aa47870bdf51ee4d42acf4b53f13739e43"
    );
    assert!(!profile.profile().release_claimable);

    let mut registered = 0;
    let mut pending = 0;
    for record in &registry.evidence().evidence {
        registered += usize::from(record.registration.is_some());
        if let EvidenceDisposition::Pending { owner_pr, .. } = &record.disposition {
            pending += 1;
            assert_eq!(owner_pr, "M10-PR05");
            assert!(record.registration.is_none());
        } else {
            assert!(record.registration.is_some());
        }
        let authority = profile
            .profile()
            .evidence
            .iter()
            .find(|item| item.id.as_str() == record.id)
            .expect("profile evidence authority");
        // GC04 evidence is executable and points at its compiled runners;
        // the G010 and inventory records keep their M01-PR06 planning anchor.
        let expected_reference = if record.id.starts_with("EVID-CONFORMANCE-GC04-") {
            "crates/selene-testing/src/conformance/mod.rs"
        } else {
            "docs/v2/roadmap/work-items-00-04.md#m01-pr06"
        };
        assert_eq!(authority.reference, expected_reference, "{}", record.id);
    }
    assert_eq!(registered, 4);
    assert_eq!(pending, 1);
}

#[test]
fn closed_decode_rejects_missing_and_extra_fields() {
    let (mut rules, evidence) = sources();
    rules.as_object_mut().unwrap().remove("registry_version");
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("missing field `registry_version`")
    );

    let (rules, mut evidence) = sources();
    evidence["evidence"][0]["expected"]["extra"] = json!(true);
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("unknown field `extra`")
    );
}

#[test]
fn duplicate_and_dangling_references_fail() {
    let (mut rules, evidence) = sources();
    let duplicate = rules["rules"][0].clone();
    rules["rules"].as_array_mut().unwrap().push(duplicate);
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("duplicate rule ID")
    );

    let (rules, mut evidence) = sources();
    let duplicate = evidence["evidence"][0].clone();
    evidence["evidence"].as_array_mut().unwrap().push(duplicate);
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("duplicate evidence")
    );

    for (field, value, expected) in [
        ("features", json!(["G999"]), "unknown target feature G999"),
        (
            "clause_ids",
            json!(["CLAUSE-99"]),
            "unknown or unapproved clause",
        ),
        (
            "applicability",
            json!({"kind":"feature","feature_id":"G999"}),
            "unknown feature applicability G999",
        ),
        ("owner_pr", json!("M02-PR05"), "malformed owner"),
    ] {
        let (mut rules, evidence) = sources();
        rules["rules"][1][field] = value;
        assert!(parse(&rules, &evidence).unwrap_err().contains(expected));
    }

    let (mut rules, evidence) = sources();
    rules["approved_domains"]
        .as_array_mut()
        .unwrap()
        .push(json!("CLAUSE-99"));
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("approved domain CLAUSE-99 is not a profile clause anchor")
    );

    let (mut rules, evidence) = sources();
    rules["rules"][0]["clause_ids"]
        .as_array_mut()
        .unwrap()
        .push(json!("CLAUSE-24.2"));
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("duplicate rule clause")
    );

    let (mut rules, evidence) = sources();
    rules["rules"][1]["owner_pr"] = json!("M0é-PR0");
    let unicode_owner = std::panic::catch_unwind(move || parse(&rules, &evidence));
    assert!(unicode_owner.is_ok(), "Unicode owner must not panic");
    assert!(
        unicode_owner
            .unwrap()
            .unwrap_err()
            .contains("malformed owner")
    );

    for (pointer, value, expected) in [
        (
            "/evidence/0/id",
            json!("EVID-UNKNOWN"),
            "unknown profile evidence ID",
        ),
        (
            "/evidence/0/targets/0/rule_id",
            json!("RULE-UNKNOWN"),
            "references unknown rule",
        ),
        (
            "/evidence/0/targets/0/requirement_id",
            json!("REQ-UNKNOWN"),
            "references unknown requirement",
        ),
        (
            "/evidence/0/registration",
            json!("fixtures::function"),
            "malformed or duplicate registration",
        ),
    ] {
        let (rules, mut evidence) = sources();
        *evidence.pointer_mut(pointer).unwrap() = value;
        assert!(parse(&rules, &evidence).unwrap_err().contains(expected));
    }
}

#[test]
fn stale_boundary_hash_and_contradictory_dispositions_fail() {
    let (mut rules, evidence) = sources();
    rules["target"]["expected_count"] = json!(137);
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("does not match the canonical profile closure")
    );

    for field in ["profile_hash", "rules_hash"] {
        let (rules, mut evidence) = sources();
        evidence[field] = json!("0".repeat(64));
        assert!(
            parse(&rules, &evidence)
                .unwrap_err()
                .contains("stale profile/rules hash bindings")
        );
    }

    let (rules, mut evidence) = sources();
    evidence["evidence"][3]["expected"]["status"]["gqlstatus"] = json!("42n01");
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("malformed GQLSTATUS")
    );

    let (rules, mut evidence) = sources();
    evidence["evidence"][1]["registration"] = Value::Null;
    evidence["evidence"][1]["disposition"] = json!({
        "disposition":"pending", "owner_pr":"M10-PR05", "reason":"test"
    });
    evidence["evidence"][2]["targets"] = evidence["evidence"][1]["targets"].clone();
    evidence["evidence"][2]["expected"] = evidence["evidence"][1]["expected"].clone();
    assert!(
        parse(&rules, &evidence)
            .unwrap_err()
            .contains("mixes complete and pending evidence")
    );

    for (index, registration, disposition, expected) in [
        (
            0,
            Value::Null,
            json!({"disposition":"complete"}),
            "complete without",
        ),
        (
            4,
            json!("REG-INVENTORY"),
            json!({"disposition":"pending","owner_pr":"M10-PR05","reason":"test"}),
            "pending but has",
        ),
    ] {
        let (rules, mut evidence) = sources();
        evidence["evidence"][index]["registration"] = registration;
        evidence["evidence"][index]["disposition"] = disposition;
        assert!(parse(&rules, &evidence).unwrap_err().contains(expected));
    }
}

#[test]
fn semantic_reordering_preserves_canonical_bytes_and_hashes() {
    let (left_rules, left_evidence) = sources();
    let (mut right_rules, mut right_evidence) = sources();
    right_rules["approved_domains"]
        .as_array_mut()
        .unwrap()
        .reverse();
    right_rules["rules"][7]["clause_ids"]
        .as_array_mut()
        .unwrap()
        .reverse();
    right_rules["rules"].as_array_mut().unwrap().reverse();
    right_evidence["evidence"].as_array_mut().unwrap().reverse();
    right_evidence["evidence"][0]["targets"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let left = parse(&left_rules, &left_evidence).expect("left validates");
    let right = parse(&right_rules, &right_evidence).expect("right validates");
    assert_eq!(left.canonical_rules_json(), right.canonical_rules_json());
    assert_eq!(
        left.canonical_evidence_json(),
        right.canonical_evidence_json()
    );
    assert_eq!(left.rules_hash(), right.rules_hash());
    assert_eq!(left.evidence_hash(), right.evidence_hash());
}
