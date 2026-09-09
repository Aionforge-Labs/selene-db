# GQL profile source

`profile.json` is the source of truth for the target profile, feature
implications, runtime inventory, claim state, extensions, profile evidence IDs,
and Selene's Annex B decisions. `rules.json` owns static rule identity and
applicability. `evidence.json` extends profile evidence IDs with expected
evidence dimensions and current dispositions. The rule and evidence sources
have independent format and registry versions and canonical BLAKE3 hashes.

All three sources use closed Rust deserialization and semantic validation.
`schema.json` documents the profile format; the typed Rust records are the
authority for the rule and evidence formats. The tooling does not fetch the ISO
document, network resources, or local services.

The direct target selection is separate from runtime support and formal claim
state. Validation closes the selection over the imported Table 10 edges and
rejects inconsistent runtime support, claims, evidence, and release status.
The ordered compatibility lists preserve the surviving public runtime order.

The imported 71 edges cover only the all-of relationships in Clause 24.7,
Table 10. Alternative-choice requirements are rule records, not implication
edges. The rule inventory is marked `seeded_incomplete`; it does not claim a
complete normative inventory.

Rule IDs use the clause plus a local three-digit ordinal when no standard
machine ID exists; records tied to a feature use its approved feature ID.

The hand-authored Rust inventory oracle contains only the 117 official Annex B
IDs and category boundaries. Validation requires exactly that inventory and
checks each record's applicability, typed disposition, clause and evidence
references, and pending owner. The oracle does not duplicate Selene decisions.
`release_claimable` remains false while any applicable decision is pending and
until the later conformance work is complete.

From the repository root:

```bash
cargo run --locked -p selene-db-profile --bin selene-profile -- --write
cargo run --locked -p selene-db-profile --bin selene-profile -- --check
```

`--write` canonicalizes `profile.json` and regenerates the Rust profile data,
`registry.md`,
`docs/gql/conformance/features.md`,
and `docs/gql/conformance/implementation-defined.md`. `--check` fails when a
profile-generated output is stale.

`selene_profile::load_conformance` validates each declared closure count and
hash against the canonical profile, plus profile and rule hash bindings, IDs,
references, owners, expected dimensions, and dispositions. The checked-in seed
currently pins 144 features; profile growth does not require a validator-code
change. Semantic array reordering does not change canonical bytes or hashes.

The three executable seed records have explicit compiled registrations and
complete static dispositions. The inventory marker remains pending under
M10-PR05, which owns complete inventory and the final claim transition.
Execution manifests are caller-provided external outputs, not static authority.
Refresh or check the SHA-free traceability page with:

```bash
cargo run --locked -p selene-db-testing --bin selene-conformance -- docs --write --root .
cargo run --locked -p selene-db-testing --bin selene-conformance -- docs --check --root .
```

`scripts/check-conformance-claim.sh <full-sha> iso_aligned` requires an exact,
clean revision. `selected_profile` is accepted as a request and currently fails.
