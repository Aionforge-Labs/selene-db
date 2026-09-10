use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use selene_profile::{load_conformance, load_profile};
use serde_json::Value;

use super::super::*;
use super::*;

const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn authorities() -> (ValidatedProfile, ValidatedConformance) {
    let root = root();
    let profile = load_profile(&root.join(PROFILE_PATH)).unwrap();
    let registry = load_conformance(&root, &profile).unwrap();
    (profile, registry)
}

fn harness() -> Harness {
    Harness::load(&root()).unwrap()
}

fn request(claim: ClaimRequest) -> Request {
    Request {
        claim,
        selection: Selection::default(),
        shard: Shard::default(),
    }
}

fn selected_ids<'a>(harness: &'a Harness, request: &Request) -> Vec<&'a str> {
    harness
        .select(request)
        .unwrap()
        .iter()
        .map(|item| item.registration.id)
        .collect()
}

fn registry_error(registrations: &[Registration], markers: &[PendingMarker]) -> String {
    let (profile, registry) = authorities();
    Harness::from_parts(profile, registry, registrations, markers)
        .unwrap_err()
        .to_string()
}

#[test]
fn compiled_registry_rejects_missing_duplicate_unknown_stale_and_contradictory_records() {
    assert!(Harness::load(&root()).is_ok());
    assert!(registry_error(&REGISTRATIONS[..2], PENDING_MARKERS).contains("no compiled"));

    let mut registrations = REGISTRATIONS.to_vec();
    registrations.push(REGISTRATIONS[0]);
    assert!(registry_error(&registrations, PENDING_MARKERS).contains("duplicate compiled"));

    for (field, value, expected) in [
        ("evidence", "EVID-UNKNOWN", "unknown compiled evidence"),
        ("fragment", "not in fixture", "stale fixture fragment"),
        ("source", "", "incomplete compiled metadata"),
        ("registration", "REG-WRONG", "contradicts static evidence"),
    ] {
        let mut registrations = REGISTRATIONS.to_vec();
        match field {
            "evidence" => registrations[0].evidence_id = value,
            "fragment" => registrations[0].required_fragment = value,
            "source" => registrations[0].fixture_path = value,
            "registration" => registrations[0].id = value,
            _ => unreachable!(),
        }
        assert!(registry_error(&registrations, PENDING_MARKERS).contains(expected));
    }

    for (markers, expected) in [
        (&[][..], "no compiled marker"),
        (
            &[PendingMarker {
                evidence_id: "EVID-UNKNOWN",
            }][..],
            "unknown pending marker",
        ),
        (
            &[PendingMarker {
                evidence_id: "EVID-CONFORMANCE-G010-POSITIVE",
            }][..],
            "contradicts static evidence",
        ),
    ] {
        assert!(registry_error(REGISTRATIONS, markers).contains(expected));
    }
}

#[test]
fn selection_is_conjunctive_permutation_independent_and_exactly_shardable() {
    let harness = harness();
    let cases = [
        ("rule", "RULE-24.3-G010", 1),
        ("feature", "G010", 1),
        ("clause", "CLAUSE-24.6", 3),
        ("owner", "M01-PR05", 1),
        ("owner", "M02-PR04", 3),
        ("feature", "GG01", 3),
    ];
    for (kind, value, count) in cases {
        let mut request = request(ClaimRequest::IsoAligned);
        match kind {
            "rule" => request.selection.rule = Some(value.to_owned()),
            "feature" => request.selection.feature = Some(value.to_owned()),
            "clause" => request.selection.clause = Some(value.to_owned()),
            "owner" => request.selection.owner_pr = Some(value.to_owned()),
            _ => unreachable!(),
        }
        assert_eq!(harness.select(&request).unwrap().len(), count);
    }
    let mut conjunction = request(ClaimRequest::IsoAligned);
    conjunction.selection.feature = Some("GC04".to_owned());
    conjunction.selection.clause = Some("CLAUSE-24.6".to_owned());
    assert_eq!(harness.select(&conjunction).unwrap().len(), 3);
    assert!(
        harness
            .run_claim(conjunction, REVISION, Some(Duration::ZERO))
            .is_err()
    );
    assert_eq!(
        selected_ids(&harness, &request(ClaimRequest::IsoAligned)),
        selected_ids(&harness, &request(ClaimRequest::SelectedProfile))
    );

    for (kind, value, expected) in [
        ("rule", "RULE-UNKNOWN", "unknown rule"),
        ("feature", "G999", "unknown feature"),
        ("clause", "CLAUSE-99", "unknown clause"),
        ("owner", "M99-PR99", "unknown owner"),
        ("rule", "RULE-24.2-001", "produced no executable"),
    ] {
        let mut request = request(ClaimRequest::IsoAligned);
        match kind {
            "rule" => request.selection.rule = Some(value.to_owned()),
            "feature" => request.selection.feature = Some(value.to_owned()),
            "clause" => request.selection.clause = Some(value.to_owned()),
            "owner" => request.selection.owner_pr = Some(value.to_owned()),
            _ => unreachable!(),
        }
        assert!(
            harness
                .select(&request)
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }

    let mut shards = Vec::new();
    for index in 0..2 {
        let mut request = request(ClaimRequest::IsoAligned);
        request.shard = Shard { index, count: 2 };
        shards.push(selected_ids(&harness, &request));
    }
    assert!(shards[0].iter().all(|id| !shards[1].contains(id)));
    assert_eq!(
        shards.into_iter().flatten().collect::<BTreeSet<_>>(),
        selected_ids(&harness, &request(ClaimRequest::IsoAligned))
            .into_iter()
            .collect()
    );
    let (profile, registry) = authorities();
    let mut reversed = REGISTRATIONS.to_vec();
    reversed.reverse();
    let permuted = Harness::from_parts(profile, registry, &reversed, PENDING_MARKERS).unwrap();
    assert_eq!(
        selected_ids(&harness, &request(ClaimRequest::IsoAligned)),
        selected_ids(&permuted, &request(ClaimRequest::IsoAligned))
    );
    for shard in [
        Shard { index: 0, count: 0 },
        Shard { index: 2, count: 2 },
        Shard { index: 4, count: 5 },
    ] {
        let mut request = request(ClaimRequest::IsoAligned);
        request.shard = shard;
        assert!(harness.select(&request).is_err());
    }
}

#[test]
fn seeded_runners_pass_iso_aligned_and_block_selected_profile() {
    let harness = harness();
    let manifest = harness
        .run_claim(
            request(ClaimRequest::IsoAligned),
            REVISION,
            Some(Duration::from_millis(17)),
        )
        .unwrap();
    assert_eq!(manifest.decision, Decision::Permitted);
    assert_eq!(manifest.observations.len(), 4);
    assert!(
        manifest
            .observations
            .iter()
            .all(|item| matches!(item.outcome, Outcome::Passed { .. }))
    );
    assert_eq!(
        manifest
            .blockers
            .iter()
            .map(|item| item.code.as_str())
            .collect::<Vec<_>>(),
        [
            "annex_b",
            "feature_claims",
            "inventory_state",
            "pending_evidence:EVID-CONFORMANCE-INVENTORY-PENDING",
            "release_claimable",
        ]
    );
    let selected = harness
        .run_claim(
            request(ClaimRequest::SelectedProfile),
            REVISION,
            Some(Duration::ZERO),
        )
        .unwrap();
    assert_eq!(selected.decision, Decision::Denied);
}

#[test]
fn claim_decision_enforces_failure_and_selected_profile_closure() {
    use ClaimRequest::{IsoAligned, SelectedProfile};
    use Decision::{Denied, Permitted};

    let blockers = [blocker("test", "test")];
    assert_eq!(claim_decision(IsoAligned, false, &[]), Permitted);
    assert_eq!(claim_decision(IsoAligned, false, &blockers), Permitted);
    assert_eq!(claim_decision(IsoAligned, true, &[]), Denied);
    assert_eq!(claim_decision(IsoAligned, true, &blockers), Denied);
    assert_eq!(claim_decision(SelectedProfile, false, &[]), Permitted);
    assert_eq!(claim_decision(SelectedProfile, false, &blockers), Denied);
    assert_eq!(claim_decision(SelectedProfile, true, &[]), Denied);
    assert_eq!(claim_decision(SelectedProfile, true, &blockers), Denied);
}

fn failed_runner(source: &str) -> Result<Actual, String> {
    if source.is_empty() {
        Ok(Actual::parser(ObservedStatus::Success))
    } else {
        Err("injected failure".to_owned())
    }
}

fn panic_runner(_: &str) -> Result<Actual, String> {
    panic!("injected panic")
}

fn mismatched_runner(source: &str) -> Result<Actual, String> {
    if source.is_empty() {
        Err("empty fixture".to_owned())
    } else {
        Ok(Actual::parser(ObservedStatus::Error {
            gqlstatus: "42N01".to_owned(),
        }))
    }
}

#[test]
fn failures_and_panics_are_normalized_and_block_every_claim() {
    let (profile, registry) = authorities();
    for (runner, expected) in [
        (failed_runner as Runner, "injected failure"),
        (panic_runner as Runner, "runner panicked"),
        (
            mismatched_runner as Runner,
            "observed dimensions did not match static expectation",
        ),
    ] {
        let mut registrations = REGISTRATIONS.to_vec();
        registrations[0].runner = runner;
        let harness = Harness::from_parts(
            profile.clone(),
            registry.clone(),
            &registrations,
            PENDING_MARKERS,
        )
        .unwrap();
        for claim in [ClaimRequest::IsoAligned, ClaimRequest::SelectedProfile] {
            let manifest = harness
                .run_claim(request(claim), REVISION, Some(Duration::ZERO))
                .unwrap();
            assert_eq!(manifest.decision, Decision::Denied);
            assert!(manifest.blockers.iter().any(|item| item.detail == expected));
        }
    }
}

/// The generated GC04 record claims the complete capability. This test ties
/// that claim to the three statement shapes section 12.4/12.5 put under it
/// so the record and the facade cannot drift apart silently.
#[test]
fn gc04_supported_record_matches_facade_behaviour() {
    let record = selene_profile::capability(selene_profile::FeatureId::GC04).unwrap();
    assert_eq!(record.status, selene_profile::CapabilityStatus::Supported);
    assert_eq!(
        record.flagger_status,
        selene_profile::FlaggerStatus::Accepted
    );
    let database = selene_db::Database::builder().build();
    let catalog = database.catalog();
    let schema = selene_db::SchemaPath::regular("selene", "memory").unwrap();
    let selected = selene_db::ObjectPath::regular("selene", "memory", "evidence").unwrap();
    catalog
        .create_schema(&schema, selene_db::CreatePolicy::Strict)
        .unwrap();
    catalog
        .create_graph(&selected, None, selene_db::CreatePolicy::Strict)
        .unwrap();
    let session = database.session(&selected).unwrap();
    let path = selene_db::ObjectPath::regular("selene", "memory", "demo").unwrap();
    let omitted = selene_db::ExecutionOutcome::SUCCESSFUL_OMITTED;
    assert_eq!(session.execute("CREATE GRAPH demo ANY").unwrap(), omitted);
    let created = catalog.snapshot().resolve_graph(&path).unwrap();
    assert_eq!(
        session.execute("CREATE OR REPLACE GRAPH demo ANY").unwrap(),
        omitted
    );
    let replaced = catalog.snapshot().resolve_graph(&path).unwrap();
    assert_ne!(replaced.id, created.id);
    assert_eq!(session.execute("DROP GRAPH demo").unwrap(), omitted);
    assert!(catalog.snapshot().resolve_graph(&path).is_err());
}

#[test]
fn fixed_provenance_manifest_is_closed_and_hashes_only_semantics() {
    let harness = harness();
    let manifest = harness
        .run_claim(
            request(ClaimRequest::IsoAligned),
            REVISION,
            Some(Duration::from_millis(17)),
        )
        .unwrap();
    assert_eq!(
        manifest.result_hash,
        "b81e44053d08dd9cb8bb7777e2e67258013816d0cea7c02f9a8a348ace8a7ca8"
    );
    let encoded = serde_json::to_vec(&manifest).unwrap();
    assert_eq!(
        serde_json::from_slice::<Manifest>(&encoded).unwrap(),
        manifest
    );
    let mut value = serde_json::from_slice::<Value>(&encoded).unwrap();
    value["environment"] = serde_json::json!("ignored");
    assert!(serde_json::from_value::<Manifest>(value).is_err());

    let mut duration = manifest.clone();
    duration.duration_ms = 9_999;
    duration.refresh_hash().unwrap();
    assert_eq!(duration.result_hash, manifest.result_hash);
    for mutation in 0..7 {
        let mut changed = manifest.clone();
        match mutation {
            0 => changed.revision.replace_range(..1, "f"),
            1 => changed.runner_version.push('x'),
            2 => changed.test_command.push('x'),
            3 => changed.blockers[0].detail.push('x'),
            4 => changed.decision = Decision::Denied,
            5 => changed.request.claim = ClaimRequest::SelectedProfile,
            6 => changed.selected_ids[0].push('x'),
            _ => unreachable!(),
        }
        changed.refresh_hash().unwrap();
        assert_ne!(changed.result_hash, manifest.result_hash);
    }
    assert!(validate_revision("ABCDEF0123456789abcdef0123456789abcdef01").is_err());
}

#[test]
fn checked_in_traceability_is_fresh_and_sha_free() {
    let harness = harness();
    let rendered = harness.render_traceability();
    assert!(!rendered.contains(REVISION));
    assert!(!rendered.contains("duration_ms"));
    assert_eq!(
        std::fs::read_to_string(root().join(TRACE_PATH)).unwrap(),
        rendered
    );
}

#[test]
fn manifest_output_refuses_existing_files_and_symlinks() {
    let dir =
        std::env::temp_dir().join(format!("selene-conformance-output-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let output = dir.join("result.json");
    std::fs::write(&output, b"existing").unwrap();
    let manifest = harness()
        .run_claim(
            request(ClaimRequest::IsoAligned),
            REVISION,
            Some(Duration::ZERO),
        )
        .unwrap();
    assert!(super::super::cli::write_manifest(&root(), &output, &manifest).is_err());
    assert_eq!(std::fs::read(&output).unwrap(), b"existing");
    #[cfg(unix)]
    {
        std::fs::remove_file(&output).unwrap();
        let target = dir.join("target.json");
        std::fs::write(&target, b"target").unwrap();
        std::os::unix::fs::symlink(&target, &output).unwrap();
        assert!(super::super::cli::write_manifest(&root(), &output, &manifest).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"target");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
