# Format-2 commit authority (F02-PR04)

F02-PR04 connected **private** durable construction to the existing facade
publication funnel. The public `DatabaseBuilder` remains infallible and in-memory.
[F02-PR05](checkpoint-reopen.md) adds public create/open/checkpoint and query-ready reconstruction; no
ephemeral database is advertised as a durable preview. Real-file tests replay
semantic transactions over the known, compatibility-bound initial catalog seed,
not a recovered query-ready database. The `test-harness` feature exposes only a
scripted benchmark operation, never a durable database/session handle.

## Recorded owner decisions

1. **Separate outcomes:** retain `MutationIndeterminate`'s already-published
   in-memory meaning. Add `Error::durable_commit_outcome()` with explicit recovery
   state, phase, live-publication flag, candidate and independently established
   written/synchronized/acknowledged positions. No automatic retry.
2. **Align both modes:** named catalog types constrain data in both memory and
   durable modes. Graph-local unused declarations may differ from a named type;
   they do not change the shared named body or permit nonconforming data.
   The previous Base-only named type / Sensor-data uniqueness fixture was invalid.
   It is retained as a negative regression; the uniqueness/index-drop fixture now
   supplies a genuinely compatible named Sensor definition.

These are scoped behavior decisions, not new conformance claims. ISO/IEC
39075:2024 §§4.13.2.1 and 12.4 bind named graph types and their data constraints;
§§4.6.2, 8.3, 8.4 and 23 distinguish rollback, unresolved termination and diagnostics.
The supplied licensed standard was consulted; no extract is redistributed here.

## Ordering and ownership

`DatabaseInner::publish_database_draft` remains the common endpoint for direct
catalog/declaration APIs and implicit/explicit GQL commits. Its serial reservation
spans validation, encode/append, synchronization, one outer catalog+graph store,
observer notifications and acknowledgment. No graph-local durable provider votes.

Preparation checks catalog revisions, bindings, lifecycle constraints, instance
and named type entity/operation rules, and ready index enforcement. The owning
graph validators are reused, including per-operation immutability checks. Changed
named types/bindings revalidate retained referencing graphs; unchanged immutable
triples do not trigger unrelated graph scans. **Affected named graphs are fully
scanned**, not O(delta). Statement admission also validates the named constraint,
so an explicit statement failure discards all earlier detached writes.

For durability, the encoded body is also applied to isolated derived `ReplayState`
with the same cumulative bounds and semantic checks used by replay. Encoding
success alone is insufficient. This retained preflight state is not another
publication authority, a provider voter or query-ready runtime. It adds CPU and
retained-memory cost; it can only be replaced by an equivalent proved validator.
All replacement runtimes and the next outer Arc are prepared before append.

The stream uses direct `File` writes and explicit `sync_all`; it has no buffered
success mode, background worker, asynchronous drop work, or unbounded admission
queue. Controlled groups contain 1–32 prepared records, bounded in aggregate
encoded bytes. A group synchronizes once, reports one outer publication and then
acknowledges its complete boundary. The facade submits single logical transactions;
stream group benchmarks are **not** facade group-commit speedup claims.

## Failure and diagnostic contract

| Evidence | Durable state / live visibility | Diagnostic |
|---|---|---|
| Semantic statement rejected before commit | No store/append; original cause retained | G2000 for named type violations |
| Local commit prevented before append | Canceled; not published | 40N01, with original semantic/other cause |
| Append/sync failed; truncation and cleanup sync succeeded | Canceled; not published; writer remains fenced | 40N01 |
| Truncation or cleanup sync failed | Uncertain; not published; writer fenced | 40003, never a rollback guarantee |
| Sync succeeded; publication interrupted | CommittedUnacknowledged; not published; writer fenced | 40003 |
| Outer store succeeded; observer/ack interrupted | CommittedUnacknowledged; published; writer fenced | 40003 |
| Sync, store and acknowledgment completed | Complete record and state committed | Normal successful outcome |

40N01 is the IE008 local commit-rollback subclass under §8.4 GR1(b). G2000
statement errors are not indiscriminately relabeled as I/O failures; when a
semantic error prevents an actual commit, its typed source and nested diagnostic
remain available beneath the rollback status. Existing optimistic-conflict and
in-memory cancellation statuses remain unchanged.

40003 is this implementation's reporting choice under IW005/§23 for unresolved
local termination and unacknowledged completion. The number alone does **not**
prove rollback, live visibility, or standards conformance. The typed state
distinguishes uncertainty from a known synchronized record and identifies whether
publication occurred. Connection exception 08007 is not used for local files.

An append/sync failure fences admission **before** cleanup. Rollback uses only the
same retained segment's last successful synchronization boundary, explicitly
truncates, repositions the cursor (`set_len` alone does not), and synchronizes
the truncation. Previously acknowledged records cannot be removed. A complete
written watermark is not proof of synchronization and may exclude an incomplete
candidate suffix. Cleanup errors are retained alongside the primary cause.

After successful synchronization there is no rollback attempt. Synchronous
publication/observer unwind is caught and classified; a dropped session before
commit merely discards its detached draft. Mid-call cancellation in the lower
protocol is sampled before append and after irreversible phases, never treated
as proof of rollback. There is no spawned task for a dropped caller to abandon.

## Selected control and readers

The permanent StoreWriter `LOCK` lease is retained. Consuming valid empty control
publishes a new immutable **SLLM** envelope selecting
`WAL-00000000000000000001.logical`; it does not reinterpret SLEM empty metadata
as a data manifest. This one-segment manifest contains the existing bounded store,
epoch, generation, compatibility and parent metadata plus a fresh 32-byte segment
anchor. CURRENT binds its exact digest; that selected manifest digest is the
initial frame lineage anchor. PR05's subsequent SLDM selections retain this origin
independently of each new selected manifest digest. SLTXN2 frames never supply their own expected context.

The empty segment is synchronized before control selection. Bootstrap errors do
not guess or adopt orphans. Empty-only control rejects data/mixed artifacts.
Selected validation never depends on opening unselected ancestor payloads.
These unkeyed hashes detect corruption, not malicious rollback/authentication.

An exclusive epoch spans append through publication/ack or durable rollback.
Readers retain a shared epoch from CURRENT selection through all WAL consumption,
so a reader cannot race cleanup. This conservative serialization is intentional;
there is no PR06 rotation/checkpoint/retention implementation here. Callbacks must
not re-enter same-directory operations that require this epoch.

Readers validate bounded headers before allocating full frames, require exact
store/epoch/segment/sequence/digest lineage, and fail closed on complete corruption.
Only an incomplete final unsealed suffix is reported separately; no reader repairs
it or scans past corruption. A read error terminates that reader.

## Evidence and deletion owners

`transaction/durable_tests.rs`, `transaction/named_tests.rs` and
`logical_stream/tests.rs` exercise live state, status, source chains, semantic
real-file replay, synchronized-prefix preservation, failed cleanup, post-store
unwind, cancellation, dropped drafts, blocked readers, foreign logs, stale groups
and bootstrap faults. `BENCHMARKS.md` records the sanctioned `durable_commit`
measurements, including actual per-ack p95/p99 and named/unbound workloads.

The legacy SLDB v3 writer, graph committer and durable provider adapters are not
used by this new path. #1128 is addressed by replacement, not by preserving a
second legacy authority. F02-PR08 owns their deletion and the old codec/semantic
conversion bridge; F02-PR05 owns public durable reconstruction; F02-PR07/F06 own
heavy crash/recovery/native qualification. No process-kill or power-loss claim
is inferred from deterministic tests or successful sync calls. Guarantees remain
conditional on supported native Linux/macOS filesystem and device behavior.
