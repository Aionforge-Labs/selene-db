---
plan_id: F02-PR01
milestone: F02
initial_status: proposed
---

# F02-PR01 — Anchor store operations and establish format-2 store control

**Milestone:** [F02: Bring durable embedding forward](Milestone-F02.md)
**Dependencies:** [PLAN-01](PLAN-01.md)
**Carries forward:** M09-PR01, M09-PR02; see the [complete crosswalk](06-OLD-TO-NEW-MAP.md).
**Issue closure:** #1088
**Focused skills:** `rust-storage-durability`; `rust-api-design`; `rust-test-design` from `yet-more-skills/skills/<name>/SKILL.md`.

## Outcome

Establish one anchored directory capability, one writer lock domain, durable store identity and empty-store control publication. Close #1088 only after its rename-race tests pass.

## Start from what exists

Current persistence retains canonical/stable paths, but an anchored pathname is not an opened directory capability. The existing files are reusable starting points; this is not a request to discard the persistence crate. Source: S07/S08.

**Observed live entry points:** `crates/selene-persist/src/wal_path.rs`, `crates/selene-persist/src/snapshot_path.rs`, `crates/selene-persist/src/manifest.rs`, `crates/selene-persist/src/manifest_lock.rs`, `crates/selene-persist/src/artifact_identity.rs`

Paths are navigation, not a closed edit inventory. New modules are implementation choices. Keep required callers and their tests with the behavior they support. Source IDs resolve in [SOURCE-NOTES.md](SOURCE-NOTES.md).

## Implementation sequence

1. Implement a StoreDirectory boundary that owns managed artifact open/create/rename/remove/sync operations for the lifetime of an opened store. Validate child names and reject traversal, absolute names and unsupported link behavior.
2. Reuse a maintained safe capability API when std cannot express the required relative operations. The workspace forbids unsafe code: do not add hand-written syscall wrappers to avoid one justified dependency. A supported-platform gap must be exposed, not silently replaced by path re-resolution.
3. Bind the store-wide lock to the anchored store. Persistence-control reopen preserves durable StoreId/epoch. Facade durable open and fresh process-local DatabaseId/handle binding belong to F02-PR05, not this empty-control API.
4. Define CURRENT and immutable manifest generation selection, format/profile/collation identity and artifact lineage. Empty-store creation stages and synchronizes files and directory entries before advertising the store.
5. Route all persistence entry points through this boundary, including legacy-internal callers that remain until F02-PR08. Do not claim complete directory safety while checkpoint/prune still re-resolve user paths.

Owner-directed pre-commit refinements: every managed publication/prune requires
the same owned StoreWriter proof, including standalone MANIFEST/snapshot/prune
conveniences; online components pass their existing lease without reacquiring
it. Read guards stay independent. Empty-control reopen completely validates
CURRENT and its self-contained selected manifest only. The adjacent parent link
is publication provenance, not a requirement to keep or decode ancestor files.
Unselected history may be removed; namespace validation still scans directory
entries, so bounded payload reads do not imply constant-time overall open.

## Acceptance and concrete regression cases

- [ ] Renaming or replacing the parent pathname after opening does not redirect WAL, snapshot, manifest or prune operations into an attacker-controlled replacement directory.
- [ ] Two aliases to the same opened directory cannot obtain independent writer authority; a second writer fails deterministically.
- [ ] Standalone publication/prune cannot bypass an existing StoreWriter; explicit owned-lease calls succeed and preserve epoch/read ordering without deadlock.
- [ ] Missing or corrupt unselected ancestor payloads do not prevent reopen/publication; selected-state integrity and compatibility checks still fail closed. Payload reads remain two across retained/pruned histories.
- [ ] Child path traversal, an unexpected final symlink and cross-store artifacts fail before mutation.
- [ ] Persistence-control empty create/reopen preserves StoreId and epoch without advertising durable facade sessions or query commits. F02-PR05 owns fresh facade DatabaseId/handle validation across durable reopen.
- [ ] Fault injection at staged create, file sync, rename and directory sync selects either the prior complete control state or the complete new state, never a fabricated mix.
- [ ] Unsupported native filesystem guarantees produce an explicit open/configuration error or a clearly documented supported-mode restriction.

## Validation and performance

Run path/manifest/lock tests and native rename-race tests. Linux PR coverage is necessary; macOS capability and synchronization behavior must be exercised in the native milestone/RC lane before claiming that platform. Review every remaining path-based artifact operation.

Measure open/create and control-publication overhead; normal queries must not pay repeated directory validation. Do not trade away directory sync merely to improve startup timings.

Use the shared [validation guide](05-VALIDATION-AND-RELEASE.md) for runner mechanics and required PR/RC gates. These are planned checks, not reported passes.

## Keep out of this PR

No arbitrary external file access, 1.x migration, custom unsafe shim, weaker durability fallback or assumption that canonicalization solves parent replacement.

The owner explicitly requires genuine handle-relative prevention, superseding
the older detection-only ruling on #1088. Native filesystem mode initially
supports Linux/macOS, with typed `UnsupportedPlatform` elsewhere; pure codecs
and in-memory operation are unaffected. The approved backend is existing
`rustix 1.1.4` safe APIs with validated single-component child names. The
investigated cap-std/cap-fs-ext 4.0.3 graph failed the unchanged duplicate-version
bans policy and is not retained. The detailed directory/control API contract is
tracked at `docs/store-directory-control.md` in the repository.

`DatabaseBuilder::build` and production facade sessions remain unchanged and
in-memory. Durable facade creation/opening, catalog/graph recovery, transaction
commits, and reissued handle binding are explicitly deferred to F02-PR05 and its
owning prerequisites. Empty control does not wrap an ephemeral Database to
simulate those outcomes.

## Bridge/deletion boundary

Path wrappers may remain as convenience constructors only when they delegate once into the anchored capability. F02-PR08 deletes obsolete legacy entry points.

## Standards and reviewer focus

Implementation storage mechanism; §4.6 recovery atomicity constrains observable results. The standard does not prescribe a WAL layout.

**Independent review question:** Does any artifact lifecycle operation escape the retained directory capability?

Use [Luna execution guidance](03-LUNA-EXECUTION.md) for material decisions and the compact handoff. A necessary adjacent caller or mechanical migration is not itself a reason to stop; an incompatible semantic, public or durable contract is.
