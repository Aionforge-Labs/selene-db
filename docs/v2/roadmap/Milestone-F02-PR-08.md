---
plan_id: F02-PR08
milestone: F02
initial_status: proposed
---

# F02-PR08 — Cut over exclusively to format 2 and expose a durable integration preview

**Milestone:** [F02: Bring durable embedding forward](Milestone-F02.md)
**Dependencies:** [F02-PR07](Milestone-F02-PR-07.md)
**Carries forward:** M09-PR08; see the [complete crosswalk](06-OLD-TO-NEW-MAP.md).
**Issue closure:** None; do not close another PR’s issue.
**Focused skills:** `rust-storage-durability`; `rust-api-design`; `rust-ci-release` from `yet-more-skills/skills/<name>/SKILL.md`.

## Outcome

Leave one supported format-2 persistence path, remove legacy codecs and publish a precise durable-preview contract that downstream adapters can use before the entire GA program finishes.

## Start from what exists

The durable value/reference boundary was settled in F03-PR02 and consumed by F02-PR03. This PR removes the old encoded bare-ID/reference variants and legacy persistence adapters only after the new path is complete; it must not strand a public Value bridge. Source: S05.

**Observed live entry points:** `crates/selene-db/src/lib.rs`, `crates/selene-db/src/database.rs`, `crates/selene-db/src/config.rs`, `crates/selene-persist/src/lib.rs`, `crates/selene-core/src`, `crates/selene-testing/src`

Paths are navigation, not a closed edit inventory. New modules are implementation choices. Keep required callers and their tests with the behavior they support. Source IDs resolve in [SOURCE-NOTES.md](SOURCE-NOTES.md).

## Implementation sequence

1. Inventory all writer, reader, snapshot, recovery and constructor dispatch paths. Delete format-1 codecs/compatibility branches and stable embedding entry points that bypass database commit authority. Keep legitimate lower-crate construction for advanced/internal graph use; it must not be advertised as a stable persisted database facade.
2. Keep only a bounded header probe that recognizes an old store and returns unsupported format. It must not mutate that store or offer an implicit conversion.
3. Check the public value/parameter/result API and remove remaining obsolete persistence re-exports. Runtime references must never be confused with durable identifiers simply because their type names are similar.
4. Run a facade-only durable consumer scenario: create catalog, parameterized insert/read, explicit rollback, reopen, delete/recreate identity and verify. Include active constraints supported at this point; reject unsupported declarations rather than pretending they persist correctly.
5. Document the preview boundary: eligible for deliberate integration testing, not GA; supported native platforms and durability assumptions; no automatic retry after ambiguity; no format/API compatibility guarantee beyond what is explicitly adopted. Produce a package artifact only under the existing authorized workflow.

## Acceptance and concrete regression cases

- [ ] A format-1 header is rejected before write access, and no legacy decoder is reachable from any supported open API.
- [ ] An external-style facade consumer survives commit/reopen and sees stable stored identity with fresh validated runtime handles.
- [ ] Corrupt format-2 inputs remain failures rather than falling through to a legacy or empty-store path.
- [ ] No raw row, candidate layout token or old encoded runtime reference variant remains in the supported persisted schema.
- [ ] Create/open/checkpoint/verify documentation compiles or is exercised against the actual API, and unsupported modes fail explicitly.
- [ ] All required recovery and phase-outcome fixtures from F02-PR04–07 pass together.

## Validation and performance

Run persistence/facade integration, negative format tests, separate doctests and package-level compile smoke. Retain the normal PR workspace gate; full native RC qualification still belongs to F06-PR02. Record preview exclusions instead of calling a workspace build release evidence.

Capture durable create/commit/reopen/checkpoint baselines for downstream comparison. These are observed measurements, not delivery SLOs invented by the plan.

Use the shared [validation guide](05-VALIDATION-AND-RELEASE.md) for runner mechanics and required PR/RC gates. These are planned checks, not reported passes.

## Keep out of this PR

No migration tool, forced downstream upgrade, production-ready label, unapproved tag/publish or claims that all native extensions are already reintegrated.

## Bridge/deletion boundary

Format-1 code and old encoded-value bridges are deleted. Temporary query/compiler adapters remain explicitly owned by F03-PR04/F04-PR09, not persistence.

Useful thin path constructors may remain when they anchor once and acquire the
required writer proof. Delete obsolete format/authority bypasses, not ergonomic
constructors solely because they accept a path. Preserve selected-state-only
control validation; unselected ancestor files are not a hidden reopen dependency.

## Standards and reviewer focus

§24 claims policy; product format/version policy is separate from ISO language conformance.

**Independent review question:** Is the preview a genuinely usable durable facade with honest boundaries, rather than a new wrapper over legacy persistence?

Use [Luna execution guidance](03-LUNA-EXECUTION.md) for material decisions and the compact handoff. A necessary adjacent caller or mechanical migration is not itself a reason to stop; an incompatible semantic, public or durable contract is.
