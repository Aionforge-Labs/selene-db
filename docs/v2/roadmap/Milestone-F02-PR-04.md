---
plan_id: F02-PR04
milestone: F02
initial_status: proposed
---

# F02-PR04 — Connect durable commit to the existing publication authority

**Milestone:** [F02: Bring durable embedding forward](Milestone-F02.md)
**Dependencies:** [F02-PR03](Milestone-F02-PR-03.md)
**Carries forward:** M09-PR03; see the [complete crosswalk](06-OLD-TO-NEW-MAP.md).
**Issue closure:** #1128
**Focused skills:** `rust-storage-durability`; `rust-async-concurrency`; `rust-test-design` from `yet-more-skills/skills/<name>/SKILL.md`.

## Outcome

Extend the landed facade mutation authority with one authoritative WAL, precise watermarks and defensible commit outcomes. This is the closure owner for #1128.

The [implemented behavior contract](../durable-commit.md) records the owner-approved
separate durable outcomes and the subsequent both-mode named-type admission
alignment. It describes private construction, real-file evidence, preflight costs,
diagnostic mapping and the PR05/PR08 boundaries; no public durable open is added.

## Start from what exists

The facade already has detached drafts, a serial mutation coordinator and one outer in-memory publication. Its prepare/flush failpoints are not a durable implementation. The current WAL flush calls sync_data but does not retain a flushed offset. Source: S06/S08.

**Observed live entry points:** `crates/selene-db/src/transaction.rs`, `crates/selene-db/src/database.rs`, `crates/selene-persist/src/writer.rs`

**Search hints, not verified current filenames:** `crates/selene-persist/src/writer/append.rs`, `crates/selene-graph/src/committer_batch_wal_tests/indeterminate.rs`. Locate the owning symbols before editing.

Paths are navigation, not a closed edit inventory. New modules are implementation choices. Keep required callers and their tests with the behavior they support. Source IDs resolve in [SOURCE-NOTES.md](SOURCE-NOTES.md).

## Implementation sequence

1. Trace one implicit and one explicit multi-request commit through the current draft/publication code. Insert durable preparation into that funnel rather than creating a graph-local second authority.
2. Track written, successfully synchronized, published and acknowledged transaction boundaries separately. Preserve enough segment/sequence context that an offset from one segment cannot be applied to another.
3. Prepare all fallible validation and allocation before crossing the irreversible commit boundary when possible. Synchronize the authoritative record, publish the complete outer state, then acknowledge. Derived providers are observers, not independent commit voters.
4. On an append/sync failure, fence the writer. Report definite cancellation only after non-recovery of that transaction is established, including durable truncation/control updates where required. A failed cleanup sync leaves an indeterminate outcome; it is not safe to relabel it rolled back.
5. Keep the existing in-memory MutationIndeterminate promise distinct from durable ambiguity that may exist before live publication. Introduce an explicit durable-outcome distinction or deliberately revise the pre-GA public contract and its callers/docs; do not reuse the current “already visible” description for a merely possibly durable transaction. Cancellation after entering an irreversible commit phase requests a classified outcome, not unsafe forced rollback.
6. For a post-sync publication/acknowledgment failure, retain replayable committed state and return the documented unknown/indeterminate outcome without inviting a blind retry. Serialize group-commit acknowledgment ordering and drain/fail queued requests consistently.

## Acceptance and concrete regression cases

- [ ] Failure before any authoritative append leaves no new state live or after reopen.
- [ ] A partial group append followed by successfully durable rollback does not replay the canceled members; previous durable members remain intact.
- [ ] Truncation failure or synchronization failure during cleanup stays indeterminate and poisons/fences further writes.
- [ ] After successful WAL sync but before acknowledgment, recovery includes the whole transaction and callers do not receive a false canceled result.
- [ ] Catalog and graph state never become visible in separate publications; a rollback after an earlier statement failure leaves no earlier transaction writes.
- [ ] Observer failure cannot undo an authoritative commit or produce a success report while silently losing a required constraint index.
- [ ] A dropped/canceled caller cannot cause transaction cleanup to erase a successfully synchronized transaction or acknowledge it as definitely canceled.

## Validation and performance

Use deterministic phase failpoints plus real-file reopen tests. Assert error kind, GQL diagnostic mapping, live visibility, replayed transaction set and subsequent writer usability. Explicitly map local commit failures under §8.4 separately from connection-related unknown-status provisions; do not infer a GQL code from a generic Rust I/O error alone.

Measure durable single-transaction latency, controlled group size/throughput and p95/p99 acknowledgment latency. Label buffered and durable measurements separately. No group-commit speedup may alter the success promise.

Use the shared [validation guide](05-VALIDATION-AND-RELEASE.md) for runner mechanics and required PR/RC gates. These are planned checks, not reported passes.

## Keep out of this PR

No second WAL authority, fsync-free success, panic-as-rollback assumption, blind retry helper, raw offset rollback across rotation or rewrite of the already-correct outer publication design.

## Bridge/deletion boundary

Remove superseded poison/watermark logic from the authoritative path. The legacy persistence adapter is deletion-owned by F02-PR08, not a second supported durability mode.

## Standards and reviewer focus

§4.6 transactions; §8.3 rollback; §8.4 commit; §23 diagnostics. See the phase table in 04-GQL-AND-DURABILITY-NOTES.md.

**Independent review question:** For every returned canceled/committed/indeterminate outcome, is the corresponding live and recovered state demonstrably possible—and are stronger claims avoided?

Use [Luna execution guidance](03-LUNA-EXECUTION.md) for material decisions and the compact handoff. A necessary adjacent caller or mechanical migration is not itself a reason to stop; an incompatible semantic, public or durable contract is.
