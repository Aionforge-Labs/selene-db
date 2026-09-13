//! Evidence execution, selection, manifests, and traceability output.

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use selene_profile::{
    ClaimState, EvidenceDisposition, EvidenceExpectation, ExpectedNullability, ExpectedOrder,
    ExpectedSideEffects, ExpectedStatus, ExpectedType, ImplementationDefinedDecision,
    InventoryState, TARGET_FEATURE_CLOSURE,
};
use serde::{Deserialize, Serialize};

use super::{ConformanceError, Contract, Harness, invalid};

const FORMAT_VERSION: u32 = 1;
const REPOSITORY: &str = "jscott3201/selene-db";
const RUNNER_VERSION: &str = "2";
pub(super) const TRACE_PATH: &str = "docs/gql/conformance-evidence.md";
const TEST_COMMAND: &str = "cargo run --locked -p selene-db-testing --bin selene-conformance -- run --root . --revision <EXPECTED_REVISION> --claim <CLAIM> --output <EXTERNAL_PATH>";
const ISO_WORDING: &str =
    "ISO-aligned with disclosed conformance gaps; not a complete selected-profile claim.";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ClaimRequest {
    IsoAligned,
    SelectedProfile,
}

impl ClaimRequest {
    pub(super) fn parse(value: &str) -> Result<Self, ConformanceError> {
        match value {
            "iso_aligned" => Ok(Self::IsoAligned),
            "selected_profile" => Ok(Self::SelectedProfile),
            _ => Err(invalid(format!("unknown claim {value}"))),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    pub(super) rule: Option<String>,
    pub(super) feature: Option<String>,
    pub(super) clause: Option<String>,
    pub(super) owner_pr: Option<String>,
}

impl Selection {
    fn is_unfiltered(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Shard {
    pub(super) index: usize,
    pub(super) count: usize,
}

impl Default for Shard {
    fn default() -> Self {
        Self { index: 0, count: 1 }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub(super) claim: ClaimRequest,
    pub(super) selection: Selection,
    pub(super) shard: Shard,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ObservedStatus {
    Success,
    Error { gqlstatus: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ObservedDimension {
    NotObserved,
}

/// Side-effect observation reported by an executable runner.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ObservedSideEffects {
    /// The runner verified that no state was published.
    Forbidden,
    /// The runner verified that the specified state was published.
    Required,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Actual {
    status: ObservedStatus,
    result_type: ObservedDimension,
    nullability: ObservedDimension,
    ordering: ObservedDimension,
    side_effects: ObservedSideEffects,
}

impl Actual {
    /// A parser-only observation: nothing executed, so nothing mutated.
    pub(super) fn parser(status: ObservedStatus) -> Self {
        Self::executed(status, ObservedSideEffects::Forbidden)
    }

    /// An observation from a runner that executed against a database and
    /// checked the side-effect dimension itself.
    pub(super) fn executed(status: ObservedStatus, side_effects: ObservedSideEffects) -> Self {
        Self {
            status,
            result_type: ObservedDimension::NotObserved,
            nullability: ObservedDimension::NotObserved,
            ordering: ObservedDimension::NotObserved,
            side_effects,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum Outcome {
    Passed { actual: Actual },
    Failed { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    registration_id: String,
    evidence_id: String,
    outcome: Outcome,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
struct Blocker {
    code: String,
    detail: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Decision {
    Permitted,
    Denied,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    format_version: u32,
    repository: String,
    revision: String,
    profile_id: String,
    profile_hash: String,
    rules_hash: String,
    evidence_hash: String,
    runner_version: String,
    test_command: String,
    request: Request,
    selected_ids: Vec<String>,
    observations: Vec<Observation>,
    blockers: Vec<Blocker>,
    pub(super) decision: Decision,
    duration_ms: u64,
    result_hash: String,
}

#[derive(Serialize)]
struct SemanticManifest<'a> {
    format_version: u32,
    repository: &'a str,
    revision: &'a str,
    profile_id: &'a str,
    profile_hash: &'a str,
    rules_hash: &'a str,
    evidence_hash: &'a str,
    runner_version: &'a str,
    test_command: &'a str,
    request: &'a Request,
    selected_ids: &'a [String],
    observations: &'a [Observation],
    blockers: &'a [Blocker],
    decision: Decision,
}

impl Manifest {
    fn refresh_hash(&mut self) -> Result<(), ConformanceError> {
        let semantic = SemanticManifest {
            format_version: self.format_version,
            repository: &self.repository,
            revision: &self.revision,
            profile_id: &self.profile_id,
            profile_hash: &self.profile_hash,
            rules_hash: &self.rules_hash,
            evidence_hash: &self.evidence_hash,
            runner_version: &self.runner_version,
            test_command: &self.test_command,
            request: &self.request,
            selected_ids: &self.selected_ids,
            observations: &self.observations,
            blockers: &self.blockers,
            decision: self.decision,
        };
        self.result_hash = blake3::hash(&serde_json::to_vec(&semantic)?)
            .to_hex()
            .to_string();
        Ok(())
    }
}

impl Harness {
    fn select(&self, request: &Request) -> Result<Vec<&Contract>, ConformanceError> {
        validate_shard(request.shard)?;
        let rules = &self.registry.rules().rules;
        if let Some(value) = request.selection.rule.as_deref()
            && !rules.iter().any(|rule| rule.id == value)
        {
            return Err(invalid(format!("unknown rule filter {value}")));
        }
        if let Some(value) = request.selection.feature.as_deref()
            && !self
                .profile
                .profile()
                .features
                .iter()
                .any(|feature| feature.id.as_str() == value)
        {
            return Err(invalid(format!("unknown feature filter {value}")));
        }
        if let Some(value) = request.selection.clause.as_deref()
            && !self
                .registry
                .rules()
                .approved_domains
                .iter()
                .any(|clause| clause == value)
        {
            return Err(invalid(format!("unknown clause filter {value}")));
        }
        if let Some(value) = request.selection.owner_pr.as_deref()
            && !rules.iter().any(|rule| rule.owner_pr == value)
        {
            return Err(invalid(format!("unknown owner filter {value}")));
        }
        let matching = self
            .contracts
            .iter()
            .filter(|contract| contract_matches(contract, &request.selection))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            return Err(invalid("selection produced no executable evidence"));
        }
        let selected = matching
            .into_iter()
            .enumerate()
            .filter_map(|(position, contract)| {
                (position % request.shard.count == request.shard.index).then_some(contract)
            })
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err(invalid("selection shard produced no executable evidence"));
        }
        Ok(selected)
    }

    pub(super) fn run_claim(
        &self,
        request: Request,
        revision: &str,
        duration: Option<Duration>,
    ) -> Result<Manifest, ConformanceError> {
        validate_revision(revision)?;
        if !request.selection.is_unfiltered() || request.shard != Shard::default() {
            return Err(invalid(
                "release claims require the unfiltered single-shard executable set",
            ));
        }
        let started = Instant::now();
        let selected = self.select(&request)?;
        if selected.len() != self.contracts.len() {
            return Err(invalid("release claim omitted executable evidence"));
        }
        let observations = selected
            .iter()
            .map(|item| execute(item))
            .collect::<Vec<_>>();
        let failed = observations
            .iter()
            .any(|item| matches!(item.outcome, Outcome::Failed { .. }));
        let mut blockers = self.static_blockers();
        for observation in &observations {
            if let Outcome::Failed { reason } = &observation.outcome {
                blockers.push(Blocker {
                    code: format!("execution:{}", observation.registration_id),
                    detail: reason.clone(),
                });
            }
        }
        blockers.sort();
        let decision = claim_decision(request.claim, failed, &blockers);
        let mut manifest = Manifest {
            format_version: FORMAT_VERSION,
            repository: REPOSITORY.to_owned(),
            revision: revision.to_owned(),
            profile_id: self.profile.profile().profile_id.clone(),
            profile_hash: self.profile.hash().to_owned(),
            rules_hash: self.registry.rules_hash().to_owned(),
            evidence_hash: self.registry.evidence_hash().to_owned(),
            runner_version: RUNNER_VERSION.to_owned(),
            test_command: TEST_COMMAND.to_owned(),
            request,
            selected_ids: selected
                .iter()
                .map(|item| item.registration.id.to_owned())
                .collect(),
            observations,
            blockers,
            decision,
            duration_ms: duration.unwrap_or_else(|| started.elapsed()).as_millis() as u64,
            result_hash: String::new(),
        };
        manifest.refresh_hash()?;
        Ok(manifest)
    }

    fn static_blockers(&self) -> Vec<Blocker> {
        let mut blockers = Vec::new();
        if self.registry.rules().inventory_state != InventoryState::Complete {
            blockers.push(blocker(
                "inventory_state",
                "rule inventory is seeded_incomplete",
            ));
        }
        for record in &self.registry.evidence().evidence {
            if let EvidenceDisposition::Pending { owner_pr, reason } = &record.disposition {
                blockers.push(blocker(
                    &format!("pending_evidence:{}", record.id),
                    &format!("owner {owner_pr}: {reason}"),
                ));
            }
        }
        let mut claims = BTreeMap::new();
        for feature_id in TARGET_FEATURE_CLOSURE {
            let feature = self
                .profile
                .profile()
                .features
                .iter()
                .find(|feature| feature.id.as_str() == feature_id.as_str())
                .expect("validated target feature");
            let key = match feature.claim_state {
                ClaimState::Unsupported => "unsupported",
                ClaimState::ImplementedUnclaimed => "implemented_unclaimed",
                ClaimState::ClaimedPendingEvidence => "claimed_pending_evidence",
                ClaimState::Claimed => "claimed",
            };
            claims
                .entry(key)
                .or_insert_with(Vec::new)
                .push(feature.id.as_str());
        }
        if claims.keys().any(|key| *key != "claimed") {
            blockers.push(blocker("feature_claims", &format!("{claims:?}")));
        }
        let annex_pending = self
            .profile
            .profile()
            .implementation_defined_choices
            .iter()
            .filter(|record| {
                self.profile.applicability(record.applicability.as_str()) == Some(true)
                    && matches!(
                        record.decision,
                        ImplementationDefinedDecision::Pending { .. }
                    )
            })
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>();
        if !annex_pending.is_empty() {
            blockers.push(blocker(
                "annex_b",
                &format!(
                    "applicable decisions remain pending: {}",
                    annex_pending.join(", ")
                ),
            ));
        }
        if !self.profile.profile().release_claimable {
            blockers.push(blocker(
                "release_claimable",
                "profile release_claimable is false",
            ));
        }
        blockers.sort();
        blockers
    }

    pub(super) fn render_traceability(&self) -> String {
        let mut output = format!(
            "<!-- @generated by selene-conformance; runner-version: {RUNNER_VERSION} -->\n\n# Conformance evidence\n\n| Field | Value |\n|---|---|\n| Profile | `{}` |\n| Profile hash | `{}` |\n| Rules hash | `{}` |\n| Evidence hash | `{}` |\n| Inventory | `{}` |\n| Release claimable | **{}** |\n\n## Static and compiled evidence\n\n| Evidence | Targets | Disposition | Compiled binding | Fixture / fragment |\n|---|---|---|---|---|\n",
            self.profile.profile().profile_id,
            self.profile.hash(),
            self.registry.rules_hash(),
            self.registry.evidence_hash(),
            match self.registry.rules().inventory_state {
                InventoryState::SeededIncomplete => "seeded_incomplete",
                InventoryState::Complete => "complete",
            },
            self.profile.profile().release_claimable,
        );
        for evidence in &self.registry.evidence().evidence {
            let targets = evidence
                .targets
                .iter()
                .map(|item| format!("{} / {}", item.rule_id, item.requirement_id))
                .collect::<Vec<_>>()
                .join("<br>");
            if let Some(contract) = self
                .contracts
                .iter()
                .find(|item| item.evidence.id == evidence.id)
            {
                output.push_str(&format!(
                    "| `{}` | {} | complete | `{}` / `{}` | `{}` / `{}` |\n",
                    evidence.id,
                    targets,
                    contract.registration.id,
                    contract.registration.runner_name,
                    contract.registration.fixture_path,
                    contract.registration.required_fragment,
                ));
            } else {
                let EvidenceDisposition::Pending { owner_pr, .. } = &evidence.disposition else {
                    unreachable!("validated evidence disposition")
                };
                let marker = self
                    .markers
                    .iter()
                    .find(|item| item.evidence_id == evidence.id)
                    .expect("validated pending marker");
                output.push_str(&format!(
                    "| `{}` | {} | pending ({}) | typed pending marker `{}` | — |\n",
                    evidence.id, targets, owner_pr, marker.evidence_id
                ));
            }
        }
        output.push_str("\nStatic completion means a compiled contract exists, not that it passed at this revision. Only an executed result manifest records a pass. G010 currently checks parser feature observation; GC04 includes parser rejection and facade catalog execution. These registrations are not the full supported semantic corpus.\n\nThe [canonical feature matrix](conformance/features.md) enumerates direct selections, their Table 10 implication closure, runtime support and evidence references. The [Annex B report](conformance/implementation-defined.md) records choices and pending decisions. Neither feature cardinalities nor passing regression tests establish minimum conformance: that requires all non-optional syntax and semantics and the required graph, type and Unicode conditions.\n\n## Current blockers\n\n");
        for blocker in self.static_blockers() {
            output.push_str(&format!("- `{}`: {}\n", blocker.code, blocker.detail));
        }
        output.push_str(&format!(
            "\nPermitted wording: “{ISO_WORDING}”\n\nA complete selected-profile claim is blocked. [F06-PR01](../v2/roadmap/Milestone-F06-PR-01.md) carries forward M10-PR05's inventory, Annex B and claim work. This report does not approve a release-scope reduction or excuse incorrect agreed behavior. See [release readiness](../v2/release-readiness.md). Result manifests are external outputs and are not checked in.\n"
        ));
        output
    }
}

fn blocker(code: &str, detail: &str) -> Blocker {
    Blocker {
        code: code.to_owned(),
        detail: detail.to_owned(),
    }
}

fn claim_decision(claim: ClaimRequest, failed: bool, blockers: &[Blocker]) -> Decision {
    if failed || (claim == ClaimRequest::SelectedProfile && !blockers.is_empty()) {
        Decision::Denied
    } else {
        Decision::Permitted
    }
}

fn validate_shard(shard: Shard) -> Result<(), ConformanceError> {
    if shard.count == 0 || shard.index >= shard.count {
        return Err(invalid(
            "shard count must be positive and index must be in range",
        ));
    }
    Ok(())
}

fn contract_matches(contract: &Contract, selection: &Selection) -> bool {
    contract.rules.iter().any(|rule| {
        selection
            .rule
            .as_ref()
            .is_none_or(|value| &rule.id == value)
            && selection
                .feature
                .as_ref()
                .is_none_or(|value| rule.features.contains(value))
            && selection
                .clause
                .as_ref()
                .is_none_or(|value| rule.clause_ids.contains(value))
            && selection
                .owner_pr
                .as_ref()
                .is_none_or(|value| &rule.owner_pr == value)
    })
}

fn execute(contract: &Contract) -> Observation {
    let result = catch_unwind(AssertUnwindSafe(|| {
        (contract.registration.runner)(contract.registration.fixture)
    }));
    let outcome = match result {
        Ok(Ok(actual)) if expectation_matches(&contract.evidence.expected, &actual) => {
            Outcome::Passed { actual }
        }
        Ok(Ok(_)) => Outcome::Failed {
            reason: "observed dimensions did not match static expectation".to_owned(),
        },
        Ok(Err(reason)) => Outcome::Failed { reason },
        Err(_) => Outcome::Failed {
            reason: "runner panicked".to_owned(),
        },
    };
    Observation {
        registration_id: contract.registration.id.to_owned(),
        evidence_id: contract.registration.evidence_id.to_owned(),
        outcome,
    }
}

fn expectation_matches(expected: &EvidenceExpectation, actual: &Actual) -> bool {
    let status = match (&expected.status, &actual.status) {
        (ExpectedStatus::Success, ObservedStatus::Success) => true,
        (ExpectedStatus::Error, ObservedStatus::Error { .. }) => true,
        (
            ExpectedStatus::Exact {
                gqlstatus: expected,
            },
            ObservedStatus::Error { gqlstatus: actual },
        ) => expected == actual,
        _ => false,
    };
    status
        && matches!(expected.result_type, ExpectedType::NotAsserted)
        && matches!(actual.result_type, ObservedDimension::NotObserved)
        && matches!(expected.nullability, ExpectedNullability::NotApplicable)
        && matches!(actual.nullability, ObservedDimension::NotObserved)
        && matches!(
            expected.ordering,
            ExpectedOrder::NotApplicable | ExpectedOrder::Unspecified
        )
        && matches!(actual.ordering, ObservedDimension::NotObserved)
        && matches!(
            (expected.side_effects, actual.side_effects),
            (
                ExpectedSideEffects::Forbidden,
                ObservedSideEffects::Forbidden
            ) | (ExpectedSideEffects::Required, ObservedSideEffects::Required)
        )
}

fn validate_revision(revision: &str) -> Result<(), ConformanceError> {
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "revision must be exactly 40 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}
