---
plan_id: F02-PR06
milestone: F02
initial_status: proposed
---

# F02-PR06 — Make checkpoint publication, rotation and retention one safe lifecycle

**Milestone:** [F02: Bring durable embedding forward](Milestone-F02.md)
**Dependencies:** [F02-PR05](Milestone-F02-PR-05.md)
**Carries forward:** M09-PR06; see the [complete crosswalk](06-OLD-TO-NEW-MAP.md).
**Issue closure:** None; do not close another PR’s issue.
**Focused skills:** `rust-storage-durability`; `rust-async-concurrency`; `rust-test-design` from `yet-more-skills/skills/<name>/SKILL.md`.

## Outcome

Rotate and prune durable artifacts without losing the only recoverable lineage or invalidating readers that have already selected it.

## Start from what exists

F02-PR05 establishes a working checkpoint/reopen slice. The remaining risk is lifecycle coordination: a saved path is not a retention lease, and a rename is not by itself a durable control publication.

**Observed live entry points:** `crates/selene-persist/src/manifest.rs`, `crates/selene-persist/src/writer_rotation.rs`, `crates/selene-persist/src/retention.rs`, `crates/selene-persist/src/manifest_lock.rs`, `crates/selene-persist/src/snapshot_writer.rs`

Paths are navigation, not a closed edit inventory. New modules are implementation choices. Keep required callers and their tests with the behavior they support. Source IDs resolve in [SOURCE-NOTES.md](SOURCE-NOTES.md).

## Implementation sequence

1. Define the ordering among snapshot completion, segment sealing/creation, immutable manifest creation, CURRENT replacement, required file/directory synchronization and old-artifact eligibility for deletion.
2. Acquire retention/epoch protection before selecting a manifest and retain it through actual snapshot/WAL consumption, verification and backup reads. Directory anchoring and retention are separate obligations.
3. Use one lock order across writer rotation, checkpoint, recover/open, prune and callbacks. Do not invoke external provider hooks while holding a lock they can re-enter.
4. Make pruning operate from the selected authoritative lineage plus active leases. Staged or orphan files are not proof of a newer commit; cleanup cannot delete a segment required by the previous recoverable control state.
5. Fail and fence safely on rotation/control-publication errors. Surface reportable cleanup debt rather than converting it into a successful but unrecoverable checkpoint.

Carry forward F02-PR01's unified writer proof: every publication and prune owns
StoreWriter plus the exclusive epoch; readers remain independent. Online
maintenance passes the existing proof rather than reacquiring `LOCK`. CURRENT
selects a self-contained manifest: parent generation/digest links are provenance,
not permanent ancestor-file retention. Retain selected data dependencies and
active reader leases, but do not reintroduce full control-ancestry replay or an
epoch-reset protocol merely to make old unselected manifests prunable.

## Acceptance and concrete regression cases

- [ ] Interrupt every transition around sync, seal, rename and CURRENT replacement; reopening selects a complete valid lineage.
- [ ] A reader paused after manifest selection can finish even while a newer checkpoint makes its files otherwise obsolete.
- [ ] Prune cannot remove a required WAL prefix/suffix, and an unreferenced staged file cannot become authoritative by filename order.
- [ ] Rotation cannot reuse an offset watermark from the old segment in the new segment.
- [ ] Callback re-entry, concurrent verify/checkpoint and error cleanup obey the declared lock order without deadlock.
- [ ] Exhausted disk space or a failed directory sync never yields a success acknowledgment for an unestablished control state.

## Validation and performance

Run deterministic schedule/phase tests and real-directory lifecycle tests. Add a subprocess restart test at selected crash seams; reserve a larger native crash campaign for F02-PR07 and RC. Review locks and leases with the concurrency specialist.

Measure checkpoint foreground pause, rotation latency, retained bytes under slow readers and restart cost after repeated checkpoints. Do not hide retention growth by excluding pinned-reader workloads.

Use the shared [validation guide](05-VALIDATION-AND-RELEASE.md) for runner mechanics and required PR/RC gates. These are planned checks, not reported passes.

## Keep out of this PR

No prune-by-age alone, reader lease represented only by a pathname, automatic destructive repair or callback under an exclusive lifecycle lock.

## Bridge/deletion boundary

Old independent checkpoint/rotation authorities are removed from the new path. F02-PR08 owns complete format-1 code deletion.

## Standards and reviewer focus

§4.6 atomic recovery; filesystem and retention mechanics are implementation design.

**Independent review question:** At each crash point, which exact complete lineage remains recoverable, and who keeps its files alive?

Use [Luna execution guidance](03-LUNA-EXECUTION.md) for material decisions and the compact handoff. A necessary adjacent caller or mechanical migration is not itself a reason to stop; an incompatible semantic, public or durable contract is.
