# Persistence and recovery

This document describes how `selene-db` durably persists its in-memory property
graph and how the engine reconstructs that state after a restart. It is aimed
at engineers operating `selene-db` in production: configuring durability,
sizing recovery windows, and taking backups.

This is the current `c5c0a985` format and behavior reference. It does not
describe format 2 as implemented. [D-019 and M09](v2/README.md) own the future
2.0-only format, anchored directory authority, and cutover.

The persistence subsystem lives entirely inside the
[`selene-persist`](../crates/selene-persist) crate. It owns two on-disk
formats — a write-ahead log and a snapshot envelope — and a recovery
orchestrator that drives them. It depends only on `selene-core`, never on the
graph types, by design.

## The two layers

`selene-db` writes to two complementary durability artifacts in the same
data directory:

| Artifact      | Magic  | Purpose                                           | Cost model                                |
| :------------ | :----- | :------------------------------------------------ | :---------------------------------------- |
| Write-ahead log | `SLDB` | One frame per committed transaction, plus physical checkpoint watermarks. | Constant per frame; bounded by `fsync`. |
| Snapshot      | `SLSN` | Point-in-time freeze of the entire materialized graph. | Linear in graph size; taken on a cadence. |

The WAL gives the engine **per-commit durability**: every `Mutator::commit`
that returns success has produced bytes that are reachable after a process
crash. The snapshot gives the engine **bounded recovery time**: replaying a
multi-million-entry WAL on every restart would be unacceptable, so the engine
periodically materializes the live graph as a single archive and trims its
"replay from" watermark.

Recovery is always the same two steps in order:

1. Apply the most recent snapshot (if any).
2. Read WAL frames whose sequence is greater than the snapshot's sequence,
   skipping physical checkpoint watermarks and replaying logical commits.

This is the only ordering the engine supports. There is no incremental
snapshot format, no delta snapshot, no shadow paging.

## The graph-blind boundary

`selene-persist` does not depend on `selene-graph`. It never sees a
`SeleneGraph`, a `NodeId`-keyed property store, or a typed index. Its job
ends at decoding bytes and producing one of two callbacks:

- A snapshot **section payload**, identified by a `(provider, sub)` four-byte
  tag pair.
- A WAL **`Change`**, which is the canonical mutation enum in
  `selene-core` ([`crates/selene-core/src/changeset.rs`](../crates/selene-core/src/changeset.rs),
  `pub enum Change`).

The contract is the [`RecoveryProvider`](../crates/selene-persist/src/provider.rs)
trait:

```rust
pub trait RecoveryProvider: Send + Sync {
    fn provider_tag(&self) -> [u8; 4];
    fn read_section(&self, sub: [u8; 4], bytes: &[u8]) -> RecoveryResult<()>;
    fn on_change(&self, change: &Change) -> RecoveryResult<()>;
}
```

A `ProviderRegistry` maps four-byte provider tags to `Arc<dyn RecoveryProvider>`
implementations. During recovery the orchestrator:

- Routes each snapshot section to the provider whose tag matches the
  section's `provider` field.
- Fans every replayed `Change` out to **every** registered provider, in
  deterministic provider-tag order. Each provider decides whether the change
  affects its state.

This boundary is load-bearing. It means the persistence layer does not have
to be re-released when graph types evolve — the producer that owns each
snapshot section family registers a `RecoveryProvider` keyed by a stable
four-byte tag, and the orchestrator routes bytes and changes to it by tag.
See [Recovery semantics for a provider](#recovery-semantics-for-a-provider).

`selene-db` is a single native engine with no extension or procedure-pack
system, so the production registry has exactly one provider: `selene-graph`
ships the `CoreProvider` (provider tag `CORE`) that reconstructs the
in-memory graph from `CORE/*` snapshot sections and post-snapshot `Change`s.
The registry's multi-provider, tag-keyed shape is what keeps `selene-persist`
graph-blind, not a plugin surface for third-party state.

Engine-owned audit events do **not** ride this provider path. They live in a
separate, dedicated append-only `audit.log` (`SLAU` magic, D24) with its own
retention policy, independent of the snapshot/WAL lineage — see
[The audit log](#the-audit-log).

## Writing — the WAL

Every committed `Mutator` operation in `selene-graph` produces a `Vec<Change>`.
The persistence layer appends this slice to the WAL as a single entry. Each
entry carries:

- A monotonic 64-bit sequence number assigned by the writer.
- An HLC timestamp (`HlcTimestamp`).
- An origin (`Origin::Local`, or `Origin::Replicated { source_node_id, source_seq }`).
- An optional principal (audit-trail actor), capped at
  `selene_persist::MAX_PRINCIPAL_BYTES`.
- The encoded `Change` payload, with an xxh3 checksum and an optional
  zstd-compressed body when the encoded payload crosses the compression
  threshold.

Append is single-threaded. The `WalWriter` holds an exclusive OS-level
advisory lock on the WAL file for its entire lifetime; a second
`WalWriter::open` on the same path returns
[`PersistError::WriterLockHeld`](../crates/selene-persist/src/error.rs).

### `SyncPolicy`

Durability versus throughput is controlled by the `SyncPolicy` enum in
[`writer.rs`](../crates/selene-persist/src/writer.rs):

```rust
pub enum SyncPolicy {
    /// Flush and fsync after every `N` appended entries, on explicit flush,
    /// and when the writer is dropped. `EveryN(1)` is durability-by-default.
    /// Values > 1 opt into group commit. `EveryN(0)` normalizes to `EveryN(1)`.
    EveryN(u32),
    /// Never fsync during append or drop; only explicit `WalWriter::flush`
    /// fsyncs. Explicit opt-in for benchmark parity and offline paths.
    OnFlushOnly,
}
```

The two policies and their semantics:

| Policy                | Semantics                                                                                | Use case                                              |
| :-------------------- | :--------------------------------------------------------------------------------------- | :---------------------------------------------------- |
| `EveryN(1)`           | fsync after every appended entry; fsync on flush; fsync on drop.                          | Server workloads; the default; "durable per commit".  |
| `EveryN(N)`, `N > 1`  | fsync after every `N` appended entries; fsync on flush; fsync on drop.                    | Group commit; trades up to `N-1` entries of recent durability for throughput. |
| `OnFlushOnly`         | No fsync on append. No fsync on drop. Only explicit `WalWriter::flush` syncs.             | Benchmark parity, offline import. **Not** for production. |

The default `WalConfig::default()` is `SyncPolicy::EveryN(1)` — durability
per commit. Choose `EveryN(N)` only after measuring; the WAL append
benchmarks in [BENCHMARKS.md](../BENCHMARKS.md) show batched appends already
amortize fsync cost.

### `WalConfig`

```rust
pub struct WalConfig {
    pub sync_policy: SyncPolicy,
    /// Highest WAL sequence covered by the snapshot this file extends.
    pub snapshot_seq: u64,
}
```

`snapshot_seq` is the physical WAL watermark a snapshot publishes; it is
written into the WAL file header on a fresh file, and used to seed the first
appended sequence so it lands at `snapshot_seq + 1`. A coordinated graph
checkpoint reserves this sequence with a typed checkpoint-watermark frame
before rotation. On reopen, the on-disk header wins — recovery never moves a
snapshot watermark backward, even if the config passed in is stale.

A new WAL is not initialized in place at `wal.log`. The writer creates a unique
same-directory temporary, locks its inode, writes and fsyncs the complete
header, then publishes that inode with an atomic fail-on-existing hard link and
fsyncs the directory. Competing initializers cannot overwrite one another, and
readers observe either no active WAL or a complete valid header. Fresh WAL
creation therefore requires hard-link support in the persistence filesystem;
an already-present zero-length file is rejected as truncated rather than being
silently initialized.

### Append flow

`WalWriter::append(hlc, origin, principal, &changes)` is the only mutation
entry point. On any error during append, the in-memory sequence counter is
**not** advanced and the file is truncated back to the last fully-committed
entry. The next append (or a reopen + retry) observes a consistent state.

WAL entry-header flag bit 1 identifies a checkpoint watermark. Such a frame
must be local, principal-free, and carry an empty `Vec<Change>`; malformed
marked frames are rejected during decoding. It advances physical WAL order but
is not a logical graph commit, is not fanned out to recovery providers, and
does not produce an audit event. An unflagged empty `Vec<Change>` remains an
ordinary committed transaction and therefore still advances logical graph
generation.

The marker reuses the entry-header flag field and the already-valid empty
payload encoding, so it needed no format-version bump of its own. That is now
moot for downgrades: WAL 3.0 is rejected wholesale by every 1.0.0–1.4.0 build
at the version gate, before any frame is examined, so there is no build that
both predates checkpoint watermarks and can read the file to misinterpret one.
Downgrading across a coordinated checkpoint was already unsupported; it is now
unsupported for the simpler reason that downgrading at all is.

## Commit and checkpoint outcomes

A commit passes through five states. Only the first is a definite negative.

| State | What happened | What the caller gets |
|---|---|---|
| Rejected | Validation or graph-type failure in `seal`; nothing written | A typed error. Definitely no effect. |
| Appended | The record is in the WAL file, not fsynced | `IndeterminateOutcome` if the run then fails |
| Flushed | The group fsync returned `Ok`; the run is durable | `IndeterminateOutcome` if publication then fails |
| Published | The snapshot is visible to readers | — |
| Acknowledged | `Ok(CommitOutcome)`, carrying `durable_at` | Success |

**An `Err` past `seal` does not mean the transition did not happen.** ISO/IEC
39075:2024 §8.4 `<commit command>` GR 1)b) says a failed commit's changes "are
canceled"; selene-db cannot promise that once the record has been appended, so
it reports the outcome as unknown rather than claiming a rollback it did not
perform. The status is `40003` — *transaction rollback — statement completion
unknown* (§23.1 Table 8), reached in Rust as
`GraphError::IndeterminateOutcome`.

The reason the appended-but-unflushed state is not simply discarded: the
committer-managed WAL runs under `SyncPolicy::OnFlushOnly`, so `append_record`
hands the kernel one complete, checksum-valid frame. Recovery's tail repair
truncates only a *torn* tail, and such a frame is not torn. Losing it requires
losing the page cache — a machine crash. Poisoning the committer is not a
crash; it requires you to drop the handle and reopen, and the bytes are still
there.

Three failures reach this state, and all three can leave WAL bytes behind:

- a **partial append failure** inside a batched run error-acks the members
  appended before it, whose records are already written;
- a **group flush failure** error-acks the whole run, and a failed `fsync` does
  not imply the data is absent from stable storage;
- a **publish-tail panic after a successful group flush** error-acks commits
  that are unambiguously durable and merely unpublished.

### Checkpoint failures

A checkpoint splits the same way, and reports it the same way.

*Retryable* — preparation failures: no owned WAL, a non-default WAL filename, a
provider that cannot encode the ordered generation, or a panic inside
preparation. These happen before the MANIFEST protocol begins and consume no
WAL sequence. A verified artifact collision, where the target snapshot already
exists with identical bytes, is also retryable: it leaves the active epoch
unchanged and the writer usable.

*Requires reopen* — anything else once the watermark/rotation phase starts. The
watermark record has already consumed a physical sequence by then, so the
engine cannot prove which side of the MANIFEST commit point it reached and the
new epoch may or may not be published. These arrive as `IndeterminateOutcome`
with the source rotation error preserved in the reason.

**Test `GraphError::requires_reopen()` rather than matching a variant.** Which
failures poison the committer is an engine-internal judgement, and both classes
were previously indistinguishable at the call site — a rotation-phase
`Io(AlreadyExists)` looked exactly like a retryable preparation failure while
the handle it came from was already dead.

### What to do on `IndeterminateOutcome`

1. Stop issuing work on that handle — the committer is poisoned, and every
   later commit, compaction, or checkpoint fails fast with the same status.
2. Drop the handle and reopen through `SharedGraph::recover`.
3. **Read back** to determine whether the transition landed.
4. Only then decide whether to retry.

Retrying without the read-back double-applies any commit that survived. If your
writes are not naturally idempotent, carry an application-level identity so the
re-drive after a reopen is safe. The engine does not supply one.

## Snapshot creation

A snapshot is an `rkyv`-archived envelope of TLV-tagged sections. The file
header (32 bytes) carries:

- 4-byte magic `SLSN`.
- 2-byte major version and 2-byte minor version
  (`selene_persist::SNAPSHOT_VERSION_MAJOR` / `SNAPSHOT_VERSION_MINOR`).
- 2-byte flags (currently the per-section compression toggle).
- 2-byte section count.
- 4 reserved bytes (must be zero).
- 16-byte blake3-128 hash of the section table plus payload bytes.

The header is followed by a fixed-row section table (24 bytes per row,
provider tag + sub-tag + payload offset + payload length) and the
concatenated section payloads.

Low-level and offline snapshot construction lives in
[`SnapshotBuilder`](../crates/selene-persist/src/snapshot_writer.rs):

```rust
use selene_persist::{SectionCompression, SnapshotBuilder, SnapshotConfig};

let mut builder = SnapshotBuilder::new(SnapshotConfig {
    dir: data_dir.to_path_buf(),
    sequence: snapshot_seq,
    compression: SectionCompression::PerSection { level: 1 },
    fsync: true,
});
builder.add_section(*b"CORE", *b"META", core_meta_bytes)?;
builder.add_section(*b"CORE", *b"NODE", core_node_bytes)?;
builder.add_section(*b"CORE", *b"EDGE", core_edge_bytes)?;
let outcome = builder.finalize()?;
```

`finalize()` writes a unique
`snapshot.{sequence}.snap.tmp.{pid}.{attempt}`, fsyncs it when `fsync: true`,
then atomically **hard-links** it to `snapshot.{sequence}.snap` and removes the
temporary. Unique attempts prevent a stale crash temporary from stranding a
retry. The hard-link is the race-safe alternative to `rename` (which silently
overwrites on POSIX); a final-path sequence collision fails fast with
`Io(AlreadyExists)`. The returned `SnapshotFinalizeOutcome` carries
`snapshot_seq`, `body_hash`, and `section_count`. MANIFEST rotation uses an
internal verified-collision mode instead: it retains the newly encoded
temporary and accepts an existing same-sequence final snapshot only when both
regular files have exactly the same length and bytes. This includes headers and
trailing bytes outside the envelope body hash.

Each section's `(provider, sub)` tag pair identifies its producer. Common
tags shipped by the workspace:

| Provider | Sub     | Producer                                  |
| :------- | :------ | :---------------------------------------- |
| `CORE`   | `META`  | `selene-graph` core metadata (`CoreProvider`). |
| `CORE`   | `NODE`  | `selene-graph` node columns.              |
| `CORE`   | `EDGE`  | `selene-graph` edge columns.              |
| `CORE`   | `SCMA`  | `selene-graph` schema catalog.            |

Each producer picks a four-byte ASCII `provider` tag and a `sub` tag per
section family it owns. Tags must be globally unique within a registry —
duplicate registration fails with `PersistError::DuplicateProviderTag`. In
the single native engine the only registered producer is `selene-graph`'s
`CoreProvider`; the tag-keyed shape is the section-routing mechanism, not an
extension surface.

### Coordinated checkpoints for live graphs

For a WAL-backed `SharedGraph`, use `SharedGraph::checkpoint` instead of
assembling and rotating a snapshot directly:

```rust
use selene_graph::{CheckpointConfig, SectionCompression, SharedGraph};

fn checkpoint(graph: &SharedGraph) -> selene_graph::GraphResult<()> {
    let outcome = graph.checkpoint(CheckpointConfig {
        compression: SectionCompression::PerSection { level: 1 },
    })?;
    println!(
        "checkpoint {}: {}",
        outcome.snapshot_sequence,
        outcome.snapshot_path.display()
    );
    Ok(())
}
```

The graph facade derives the directory and sequence from its owned
`data_dir/wal.log`; callers cannot supply a different watermark or disable the
required durability barriers. The checkpoint enters the same ordered committer
queue as commits and snapshot-maintenance work. Earlier commits are flushed and
published, and earlier maintenance is published, before provider sections are
encoded at that exact generation; later writes wait behind the boundary.
Lock-free readers continue to use the previously published graph while the
checkpoint runs.

A successful checkpoint first prepares every provider section without changing
the WAL. It then holds `MANIFEST.lock`, rejects a MANIFEST ahead of the writer's
pre-marker high-water mark, appends a typed checkpoint watermark at the exact
next sequence, flushes any pending group commit, writes and fsyncs the prepared
snapshot, archives the covered WAL, commits the new MANIFEST epoch, and resets
the active WAL through `WalWriter::rotate_with_checkpoint_watermark`. Provider
preparation failures therefore consume no sequence. The marker and rotation
are one writer operation, so no other append can split them.

Every coordinated checkpoint creates a fresh physical epoch, including a
repeated checkpoint with no intervening mutation and the first checkpoint of a
sequence-zero WAL. The returned `CheckpointOutcome` identifies that epoch
through `snapshot_sequence` and `snapshot_path`; its `rotation` is
`WalRotationOutcome::Rotated`. The graph and provider generation encoded in the
snapshot is unchanged by the watermark. `WalRotationOutcome::AlreadyCurrent`
remains a lower-level `rotate_with_manifest` crash-retry result when the exact
requested epoch is already committed; it is not the repeated-checkpoint policy
of `SharedGraph::checkpoint`.

`SharedGraph::write_snapshot` is deliberately different: it is a standalone,
uncoordinated snapshot writer for offline tooling, tests, or a host that has
already quiesced writes. It trusts the caller-provided sequence and fsync
policy and does not rotate the WAL, so what it writes is not a recoverable
epoch on its own — only the ordered checkpoint protocol may claim those snapshot
sequence paths.

Both of its preconditions are enforced rather than merely documented. It refuses
a target directory that already holds a `MANIFEST` or a `wal.log`, returning
`GraphError::ExistingStore` with the evidence that fired; presence is the test,
because a bare-header WAL still declares an epoch whose sequence a standalone
write would preclaim, and a checkpointed directory's active WAL is reset to a
bare header while its data lives in a snapshot. It also encodes every provider
section at one pinned graph generation and then re-checks that the published
graph was not replaced, so a commit, compaction, or vector-index rebuild landing
mid-encode fails with `GraphError::Inconsistent` instead of producing an
envelope torn across generations. That second check is an error rather than a
wait: the call quiesces nothing, so a host racing its own writers must serialize
them and retry.

The refusal deliberately stops short of publishing a `MANIFEST` for the
standalone snapshot. A stray snapshot with no `MANIFEST` makes recovery
cross-check it against the WAL and fail loudly; naming it in a `MANIFEST` would
instead make recovery trust it and apply its sequence as the replay floor,
converting a visible operator error into silent data loss. Likewise,
`WalWriter::rotate_with_manifest` remains the lower-level persistence primitive;
live graph hosts should not collect a `SnapshotBuilder` and try to reproduce the
graph committer's ordering protocol. Direct callers are still constrained to a
nonzero sequence, the conventional `wal.log` filename, and a builder directory
that resolves to the WAL directory. The checkpoint-specific writer operation
instead requires its builder to target exactly `last_sequence + 1` and accepts
sequence zero only as the pre-marker base. `WalWriter::open` canonicalizes the
parent once, rejects a final WAL symlink or other non-file entry, and reports
that anchored path through `WalWriter::path` and rotation outcomes. After
validating the builder directory, rotation publishes the snapshot through the
anchor rather than the caller spelling, so retargeting a parent alias cannot
split artifacts across directories. Before the MANIFEST advances, an existing
snapshot or archive is accepted only after exact comparison with the newly
written temporary; a valid but different same-sequence artifact fails closed.
An incomplete post-MANIFEST rotation, an ahead MANIFEST, or a conflict with an
already-committed target poisons that writer until it is reopened and recovered.

Every managed MANIFEST epoch operation uses the persistent per-directory
`MANIFEST.lock`. `WalWriter::rotate_with_manifest` holds its exclusive side from
the authoritative MANIFEST read through active-WAL reset; free
`selene_persist::prune` and `WalWriter::prune` hold it through their post-commit
artifact deletions. Direct `Manifest::write_atomic` publication also takes the
exclusive lock to protect the shared temporary name, but remains a blind
publication rather than a semantic compare-and-swap. Recovery and online backup
readers take the shared side through MANIFEST selection plus snapshot/WAL use.
Multiple readers can coexist, while epoch mutation waits for every reader to
finish. The lock file is coordination state, not recovery data, and must not be
copied into a backup; never unlink or replace it while any process may use the
live directory.

The epoch lock is advisory coordination among cooperating handles and processes
on a filesystem that supports Rust file locking. The fixed writer order is the
lifetime `wal.log` lock, then shared or exclusive `MANIFEST.lock`, then any
replacement-WAL temporary lock. Do not upgrade a shared guard or invoke
same-directory checkpoint, rotation, prune, or MANIFEST publication while
holding one. Missing-WAL graph recovery is the narrow exception: it verifies
the snapshot under a shared guard, then uses a non-blocking WAL open, so it
cannot wait in the reverse order. The OS releases a held lock when its file
handle is dropped or its process exits, while the named file remains. Each
epoch-lock operation also
canonicalizes its directory before opening the lock. A live writer no longer
uses its original parent alias, so that alias may retarget without redirecting
the writer; keep the resolved directory, its real ancestors, and its `wal.log`
entry stable while the writer is live. Renaming or replacing those entries
requires quiescence. Hard-link aliases of mutable persistence files are
unsupported because rotation replaces only the anchored directory entry.
The same stable-topology requirement applies while opening or recovering: the
portable regular-file checks reject stable symlinks and non-files but are not a
security boundary against a process concurrently replacing resolved ancestors
or `wal.log` between path-based checks. Hostile-directory resistance would
require no-follow, directory-handle-relative operations beyond this contract.

The coordinated facade requires an owned WAL at the standard `wal.log` path. A
graph without a WAL or with a custom WAL filename is rejected without poisoning
the committer; a fresh sequence-zero WAL checkpoints successfully at sequence
1. Provider serialization errors or panics also leave the graph usable for a
later retry because no marker has been appended. An exact pre-MANIFEST artifact
collision leaves the committer usable at the newly reserved physical sequence.
Other errors or panics after the combined watermark/rotation operation starts
poison the committer because the graph facade can no longer prove which
durability phase completed. A MANIFEST ahead of the owned writer, or a conflict
with an already-committed snapshot/archive, also poisons the graph: a later
commit could otherwise reuse a covered sequence or extend state that recovery
cannot reproduce. Close and recover the graph before accepting more writes.

The first rotation durably bootstraps a baseline MANIFEST for the WAL's current
epoch before publishing the new snapshot, so a pre-commit crash cannot make
legacy recovery select an orphan snapshot. Phase 4 replaces `wal.log` with a
fully written, synced, exclusively locked header through an atomic
same-directory rename; recovery therefore sees either the intact old WAL or
the valid new WAL, never a truncate-in-progress file. Relative WAL paths are
resolved when the writer opens, preventing later working-directory changes
from redirecting these artifacts.

### When to checkpoint

The engine does not checkpoint automatically; embedders drive cadence based on
workload and recovery-time budget. A reasonable starting policy:

- Checkpoint after every `N` WAL entries (e.g., `N = 1_000_000`).
- Checkpoint after every `T` minutes of wall-clock (e.g., hourly).
- Checkpoint before a planned shutdown.

Use whichever count or time threshold arrives first. Snapshot encoding is
linear in live graph state and forms a write-publication barrier, so measure its
duration and schedule it in a lower-write window when tail latency matters.
With grouped durability, the checkpoint is also an explicit flush boundary;
all commits on its earlier side are durable when it returns.

### Scheduling row compaction

Snapshots bound recovery time; row compaction reclaims in-memory node and edge
slots left behind by deletes. The engine exposes the maintenance operation but
deliberately does not own a timer or background runtime. Run it from the
embedder's existing maintenance cadence, preferably in a low-write window and
optionally immediately before a coordinated `SharedGraph::checkpoint`.

The default recommendation requires both at least 1,024 reclaimable rows and a
reclaimable-row ratio of at least 25% of allocated rows. Reading the counters is
lock-free and does not rebuild the graph. A host that needs different size
thresholds can compare `reclaimable_rows()` and
`reclaimable_row_basis_points()` directly; an age-based policy must track the
last maintenance time in the host because graph rows do not carry deletion
timestamps. The recommendation check is advisory and separate from compaction;
serialize maintenance ticks per graph so two actors do not both perform the
same full rebuild.

```rust
use selene_graph::{CompactionReport, GraphResult, SharedGraph};

fn run_compaction_tick(
    graph: &SharedGraph,
) -> GraphResult<Option<CompactionReport>> {
    if !graph.compaction_stats().compaction_recommended() {
        return Ok(None);
    }
    graph.compact().map(Some)
}
```

`SharedGraph::compact()` is the publication boundary. It rebuilds the full live
graph while holding the writer lock, then publishes the dense snapshot in the
same ordered handoff used by commits; lock-free readers continue to observe the
previous snapshot until publication. Callers should not invoke the lower-level
`compact_core()` transform and attempt to republish its result themselves.

Compaction changes only physical row layout. It emits no logical `Change`; the
dense layout becomes durable with the next graph snapshot, whose checkpoint
watermark supplies a fresh physical sequence even without an intervening user
mutation. A crash before that snapshot is logically safe: recovery restores the
same graph with its prior sparse layout, and a later maintenance tick can
compact it again.

GQL-driven hosts can use the equivalent native procedures: inspect
`CALL selene.compaction_stats()` and invoke maintenance-tier
`CALL selene.compact()` only when policy says to reclaim. `selene.compact()` is
a standalone maintenance statement and is not valid inside an explicit
transaction. The same scheduling and snapshot-durability rules apply.

For a scheduled live rotation, call `SharedGraph::checkpoint`. Because both
compaction and checkpointing use the ordered committer queue, compacting before
the next checkpoint durably captures the dense layout without manually
quiescing writers or forcing a dummy mutation. The checkpoint watermark gives
the post-compaction snapshot a new path and epoch, so a previously committed
snapshot is never rewritten in place. Direct `SnapshotBuilder` plus
`WalWriter::rotate_with_manifest` orchestration is reserved for lower-level
persistence tooling that does not own a live `SharedGraph`.

## Two-step recovery

On startup, the persistence layer runs
[`recover(dir, &registry)`](../crates/selene-persist/src/recovery.rs):

```rust
use selene_persist::{ProviderRegistry, recover};

let mut registry = ProviderRegistry::new();
registry.register(core_provider.clone())?;

let outcome = recover(data_dir, &registry)?;
```

`recover` acquires a shared [`PersistenceReadGuard`](../crates/selene-persist/src/manifest_lock.rs)
before reading authoritative metadata and retains it through snapshot
verification, provider callbacks, and complete WAL replay. Rotation, prune, and
direct MANIFEST publication therefore cannot switch or delete the selected
epoch midway through recovery. Ordinary WAL appends may continue during the
low-level read, so it reconstructs a valid framed prefix. Acquiring the guard
may create the persistent `MANIFEST.lock` coordination file in an empty or
legacy directory.

The orchestrator:

1. Reads `MANIFEST` when present and selects its authoritative
   `live_snapshot_seq`. A legacy MANIFEST-less directory instead scans
   `snapshot.{seq}.snap` files for the highest sequence.
2. Verifies the selected snapshot's body hash. On mismatch this is a
   **hard failure**
   (`PersistError::BodyHashMismatch`); recovery does not silently fall back to
   an older snapshot.
3. For each section in the snapshot's section table (in declared order),
   looks up the provider by tag and calls `provider.read_section(sub, bytes)`.
   Unknown provider tags surface as `PersistError::UnknownProvider`.
4. Opens the WAL (`wal.log`) if present. On the legacy MANIFEST-less path, the
   WAL header's `snapshot_seq` must match the selected snapshot; a MANIFEST-led
   recovery instead uses its live sequence as the replay floor because the WAL
   header may legitimately lag during a Phase-3-before-Phase-4 crash.
5. Iterates WAL entries whose `sequence > snapshot_seq`. Checkpoint watermarks
   advance the physical `last_wal_seq` but receive no provider callback.
   Non-deduplicated logical commit frames increment
   `wal_commit_entries_applied`, including ordinary empty commits; their
   decoded changes are fanned out in deterministic provider-tag order.
6. Returns a `RecoveryOutcome` with the applied snapshot sequence, physical
   last WAL sequence, provider sets, logical commit-frame count, change count,
   and replicated-deduplication count. `SharedGraph::recover` adds the logical
   commit-frame count to the snapshot's graph generation; it never derives
   generation from the physical checkpoint sequence.

For the common case the convenience wrapper in `selene-graph` does the
registration:

```rust
use selene_graph::SharedGraph;
use selene_core::GraphId;

let graph = SharedGraph::recover(data_dir, GraphId::new(1))?;
```

`SharedGraph::recover` is a writer takeover rather than an online inspection.
When `wal.log` exists, it opens and exclusively locks it before acquiring the
shared epoch guard and driving `recover_guarded`; another live graph therefore
fails fast with `WriterLockHeld`, and rotation cannot interpose between replay
and writer handoff. For a snapshot-only directory, it verifies recovery under
the shared guard before creating a WAL seeded from the verified snapshot. That
open is non-blocking, so a racing writer fails the takeover rather than forming
a lock cycle. Every path requires the retained writer tip to equal recovery's
high-water sequence, keeping later commits above the replay floor. The
closed-graph variant is `SharedGraph::recover_closed(dir, graph_id, bound_type)`.
Both recovery layers resolve the data directory once. The low-level orchestrator
rejects a present `wal.log` symlink or non-file before provider callbacks, and
the graph wrapper uses the same resolved directory for replay, live writer, and
audit-log reopen, so an input alias cannot retarget between phases.

A truncated WAL tail (torn write at the end of the log) is **not** an error
during recovery: the iterator stops at the last fully-checksummed entry,
matching the on-open scan that `WalWriter` runs.

## Recovery semantics for a provider

A `RecoveryProvider` (in the production engine, `CoreProvider`) reconstructs
its in-memory state through two surfaces:

- `read_section(sub, bytes)`: called once per snapshot section that matches
  this provider's tag, in the section table's declared order. The provider
  decodes the bytes back into its in-memory state.
- `on_change(change)`: called for every WAL `Change` past the snapshot
  sequence. The provider decides whether the change is relevant to it.

These callbacks run while the shared persistence epoch is held. They must not
re-enter same-directory checkpoint, rotation, prune, or direct MANIFEST
publication; those operations need the exclusive side of the same lock.

The recovery boundary has a sharp edge: **WAL replay only covers events
after the snapshot's last sequence**. Any state that is not derivable from
post-snapshot `Change`s alone must be persisted in the snapshot itself, not
left to WAL replay to rebuild — the pre-snapshot WAL entries may already
have been pruned. A section that is emitted empty on the assumption the WAL
will reconstruct it will silently lose that state on recovery.

The discipline is: if a provider holds state that affects future behavior
but is not reproducible from post-snapshot `Change`s, capture it in a
snapshot section. The `CoreProvider` follows exactly this rule — the full
node/edge/schema columns are materialized into `CORE/*` sections at
snapshot time so recovery never depends on replaying the entire WAL history.

## The audit log

Engine-owned audit events are kept in a dedicated append-only file,
`audit.log` (magic `SLAU`), that is deliberately separate from the
snapshot/WAL lineage. It is **not** a `RecoveryProvider` and does not
participate in snapshot/WAL recovery — it has its own open/append/read/prune
lifecycle and its own retention policy.

The substrate is intentionally below lifecycle semantics: each record is a
generic `kind`-tagged opaque payload plus a caller-supplied
`recorded_at_unix_nanos` wall-clock stamp (`selene-persist` does not own a
clock). The graph funnel writes WAL-first, audit-after.

```rust
use selene_persist::{AuditLog, AuditRecord, AuditRetentionPolicy};

let mut log = AuditLog::open(&dir.join("audit.log"))?;
log.append(&AuditRecord {
    recorded_at_unix_nanos: now_unix_nanos,
    kind: 1,
    payload: payload_bytes,
})?;

// Independent retention: keep the newest N events and/or drop events older
// than `max_age`. Both constraints default to unbounded and are conjunctive.
let policy = AuditRetentionPolicy {
    keep_n_events: Some(100_000),
    max_age: None,
};
let _outcome = log.prune(&policy, now_unix_nanos)?;
```

`open` repairs a genuine torn tail (a partial trailing record is dropped, not
an error) and refuses interior damage with
`PersistError::AuditMidLogCorruption`, mirroring the WAL's on-open posture.
Recovery reattaches the audit log purely by file presence, so a refusal recurs
on every recovery until an operator moves the damaged file aside — which is the
intended outcome, because the alternative was discarding acknowledged records
silently and reporting success. Retention here is
independent of the snapshot/WAL `RetentionPolicy` (see [Backups](#backups)),
so trimming audit history never affects graph recovery and vice versa.

## Persistence format compatibility

**There is exactly one supported on-disk format: the current one.** A store
written by any other version of selene-db is rejected at open time with
`PersistError::UnsupportedVersion`, before any artifact is read further or
modified. There is no dual decoder, no version dispatch, and no migrator.
Recreate the store from source.

This retracts an earlier claim that read-side compatibility was preserved
across 1.x. It never was: both the WAL and the snapshot gate on an exact
`(major, minor)` match, so several released minor versions could not open one
another's stores despite the promise. The policy is now what the code does.

Format identities shipped to date:

| Release | WAL | Snapshot | Audit log |
|---|---:|---:|---:|
| v1.0.0 | 2.0 | 1.0 | 1 |
| v1.1.0 | 2.0 | 1.1 | 1 |
| v1.2.0 | 2.2 | 1.4 | 1 |
| v1.3.0, v1.4.0 | 2.2 | 1.5 | 1 |
| current alpha | **3.1** | **1.6** | **2** |

F01-PR03 carries intrinsic directionality in logical edge creation and snapshot
reconstruction. The current codecs therefore use exact-match minor bumps to
reject their previous, directed-only payload shapes. These are mechanical
guards for the alpha codecs, not the F02-PR03 format-2 byte-layout decision.

WAL 3.0 brings the frame layout under integrity protection (see
[WAL framing integrity](#wal-framing-integrity)); it is not readable by any
released build, and no released build's WAL is readable by it. Audit log v2
does the same for the record header, for the same reason: the extent has to be
trustworthy before a scan can tell a torn tail from interior damage.

Because a store holds four independently versioned artifacts, the rejection
names which one is at fault — `audit log version unsupported: 1.0` rather than
a message that says `wal` regardless of the file it came from.

The 2.0 line will not open or migrate stores written by 1.x. Persisted data
written by an alpha build has no compatibility guarantee across later alpha
builds. Plan upgrades as recreate-from-source, and take a backup of the source
data — not of the store — before upgrading. This policy does not claim that all
future 2.0 format work is present in the current engine; see the
[2.0 version policy](v2/eol-and-version-policy.md).

- **Writers always emit the current version**. There is no flag to write a
  prior format.
- **Unsupported versions fail closed.** The version gate runs before the rest
  of the header is parsed and before any scan or repair, so a store from
  another version is never partially read or modified on the way to the error.

## WAL framing integrity

Every WAL entry carries two checksums beyond the payload's own:

- a **prefix checksum** over the 40-byte fixed prefix, which authenticates the
  framing fields — the payload length, the principal length, the flags, and
  the payload's own checksum — before any of them is used as a read length or
  a file offset;
- an **extent checksum** over the replicated-provenance tail and the principal
  bytes, which authenticates the identity an entry replays under.

The 24-byte file header is likewise checksummed, covering the snapshot
watermark that a rotated, entry-free WAL carries as its only content.

This is what lets recovery tell a **torn tail** from **corruption**. The
writer only ever appends, so bytes beyond a frame's extent prove that frame was
complete before they were written: a frame that fails validation with data
after it is corrupt, not torn, and recovery refuses rather than truncating,
with `WalMidLogCorruption` naming the offset and carrying the underlying
failure. Match on that variant rather than on the cause: it is the only signal
that says the log is still on disk and worth handing to offline recovery.
A frame that fails with nothing after it — or a trailing run of zeros, which is
what a short extending write leaves — is a tear, is discarded, and is reported
through `WalWriter::tail_repair()` for a direct `selene-persist` caller or
`SharedGraph::recovery_tail_repair()` for a recovered graph. Both carry the
same `WalTailRepair { reason, offset, discarded_bytes }`.

Reporting it is the point. Discarding an unacknowledged tail is correct, so
recovery returns `Ok` and there is nothing to retry — which means a caller that
does not ask has no way to distinguish a clean reopen from one that dropped a
commit some client believed it had submitted. The report exists for
reconciliation, not for error handling.

Before this, both cases were truncated silently: a single flipped bit in a
frame's length field in the middle of a log discarded every committed frame
after it, and `SharedGraph::recover` returned `Ok`.

Two limits are worth stating plainly. Nothing on disk records *when* bytes
became durable, so a lone trailing frame that rotted after being fsynced is
still treated as a tear; the bound that matters holds regardless, in that
nothing before the tail is ever discarded. And refusing preserves the log
without yet recovering it — there is no repair tool, and the entry stream stops
at the first bad frame rather than skipping it, so the guarantee is "the file
is still there, unmodified" rather than "the later frames are readable."

The audit log keeps its own independent format, and it now applies the same
rule. Audit format v2 adds a checksum over each 24-byte record header, which is
what makes a record's declared extent trustworthy and therefore makes "does
anything follow this record?" answerable — the same prerequisite WAL v3's prefix
checksum supplies. A record that fails validation with records after it refuses
with `PersistError::AuditMidLogCorruption` and leaves the file untouched; only a
genuine final tear is repaired. v1 logs are rejected at open.

Reserved bytes in the snapshot header (offsets 12–15) must be zero on disk;
nonzero values are rejected as `ReservedBytesNonZero` to make accidental
forward-compatibility hacks visible.

Per-section payload format is owned by each section's producer. The
`provider`/`sub` tag pair identifies which decoder runs; the producer is
responsible for tagging its own byte layouts with versions if it needs to
evolve them. The first-party `CORE` sections do this through subsection
version bytes inside their rkyv-archived bodies — for example the `CORE/GTYP`
section carries its own `GTYP_VERSION` independent of the `SLSN` container
version.

## Backups

A manifest-free current-state backup is **one** snapshot plus the WAL it
extends, that is:

- `snapshot.{S}.snap` for some sequence `S`, and
- `wal.log` whose header `snapshot_seq` equals `S`.

`CheckpointOutcome` identifies what one checkpoint published; it does not pin
that snapshot against a later checkpoint or prune. An online backup must join
the persistence lock domain and select its epoch only after acquiring the
shared guard:

1. Optionally call `SharedGraph::checkpoint` to bound the active WAL. Then
   acquire `PersistenceReadGuard::acquire(data_dir)`.
2. Re-read the authoritative MANIFEST with `guard.read_manifest()`. If an exact
   checkpoint outcome is required, first verify that the canonical parent of
   `outcome.snapshot_path` equals `guard.dir()`, then compare its
   `snapshot_sequence` with the guarded `live_snapshot_seq`; retry or select the
   newer live epoch on a mismatch. Equal sequence numbers from different data
   directories do not establish identity. Never copy outcome paths blindly.
3. While the guard remains alive, open and copy the named live snapshot and
   `wal.log`. Capture the WAL source length once and copy exactly that prefix;
   ordinary commits may append beyond it, and a partial final frame within the
   prefix is a recoverable torn tail. Rotation and prune wait.
4. For a complete source MANIFEST bundle, also copy every archive named by that
   MANIFEST and publish the MANIFEST in the destination last. A current-state
   two-file backup may instead omit MANIFEST and all archives; the destination
   must contain no stale MANIFEST, and legacy recovery will cross-check the
   snapshot/WAL pair.
5. Drop the guard after source files are closed. Do not copy `MANIFEST.lock`,
   `MANIFEST.tmp`, `wal.log.init.*.tmp`, reset/snapshot temporaries, or
   crash-orphan artifacts.

The shared guard does not itself stop ordinary WAL appends. A checkpoint that
has reached its exclusive-lock wait still occupies the graph's ordered
committer, however, so subsequent graph writes can queue until the backup reader
releases and checkpoint rotation completes.

To restore: drop the two files into the configured data directory and start
up. `recover` will apply the snapshot, replay the WAL forward, and the
engine will reach the same logical state.

For point-in-time restore you can keep archived WALs plus their snapshots and
replay forward from any retained epoch. `rotate_with_manifest` creates the WAL
archives, and `RetentionPolicy` prunes superseded snapshots and archives; the
embedder still owns checkpoint and retention cadence. Copy tracked history
under the same read guard so prune cannot remove an archive midway through the
copy. `audit.log` has an independent append/prune lifecycle and is not protected
by `PersistenceReadGuard`; preserving audit history requires separate
audit-prune quiescence.

## What can go wrong

The persistence layer is engineered to make every failure mode either
recoverable or loud, never silent. The expected failure modes:

| Failure                                | Detection                                  | Outcome                                                |
| :------------------------------------- | :----------------------------------------- | :----------------------------------------------------- |
| Torn write at WAL tail                 | Per-entry xxh3 checksum mismatch, oversized payload/principal lengths, or truncated body. | The on-open scan in `WalWriter::open` truncates the file to the last fully-committed entry. Logged at WARN. |
| Snapshot file truncated                | Header short read.                         | `PersistError::TruncatedSnapshotHeader`; recovery refuses to fall back silently. |
| Snapshot body corruption               | blake3-128 body hash mismatch.             | `PersistError::BodyHashMismatch`. Hard failure — the snapshot is unusable. |
| Unsupported snapshot or WAL version    | Magic + version check on open.             | `PersistError::UnsupportedVersion`. Recreate the store from source; no migration is shipped. |
| Reserved bytes set in snapshot header  | Read-time reserved-byte audit.             | `PersistError::ReservedBytesNonZero`.                  |
| Duplicate provider tag in registry     | `ProviderRegistry::register`.              | `PersistError::DuplicateProviderTag` at startup.       |
| Snapshot section for unknown provider  | Recovery routes by tag.                    | `PersistError::UnknownProvider`. The embedder forgot to register a provider. |
| WAL/snapshot epoch mismatch            | WAL header `snapshot_seq` vs applied snapshot. | `PersistError::WalSnapshotMismatch`. The pair on disk is inconsistent. |
| Non-monotonic WAL sequence             | Per-entry header check during scan.        | `PersistError::NonMonotonicSequence`. Indicates the WAL was edited or merged incorrectly. |
| Active `wal.log` is a symlink or non-file | Open/recovery inspect the anchored final directory entry before mutation or provider callbacks. | `PersistError::WalPathNotRegular`; replace it offline with a regular WAL file. |
| Pre-commit same-sequence snapshot/archive collision | Rotation compares the complete regular-file bytes with its newly written temporary. | `PersistError::ArtifactIdentityMismatch`; the active MANIFEST epoch stays unchanged and the writer remains usable. |
| Committed snapshot/archive is missing, foreign, or invalid | Retry validates regular-file shape, exact snapshot identity, and already-current retained archive structure; a pre-reset retry can recreate a missing archive from the intact active WAL. | `CommittedSnapshotUnavailable`, `CommittedSnapshotIdentityMismatch`, `CommittedArchiveInvalid`, or the underlying snapshot format/hash error; reopen/recover before accepting writes. |
| Prune overlaps rotation | Rotation and prune take `MANIFEST.lock` before their authoritative read and hold it through cleanup. | The later operation blocks, reads the committed epoch under the lock, and cannot regress the MANIFEST or delete the new live snapshot/archive. |
| Recovery or backup overlaps rotation/prune | `PersistenceReadGuard` takes the shared `MANIFEST.lock` side from authoritative selection through artifact use. | Epoch mutation waits; the reader cannot combine an old snapshot with a newly reset WAL or lose a selected file to prune. |
| Graph recovery overlaps a live writer | `SharedGraph::recover` locks an existing `wal.log` before guarded replay, or uses a non-blocking open after verified snapshot-only replay. | `PersistError::WriterLockHeld`; stop the live owner before writer takeover. Low-level guarded recovery remains available for online inspection. |
| Persistence lock path is obstructed or unsupported | Guard acquisition opens and locks the persistent `MANIFEST.lock` regular file before provider callbacks. | An I/O error is returned without reading an unpinned epoch; repair the directory or use a filesystem with supported advisory locks. |
| New WAL initialization overlaps a reader or another initializer | The complete header is fsynced under a unique sibling name, then hard-linked fail-on-existing into `wal.log` while its inode lock remains held. | Readers see absence or a complete header; exactly one initializer publishes, and a live winner makes losers return `WriterLockHeld`. |
| Checkpoint has no eligible owned WAL   | Graph facade validates ownership and the conventional WAL filename at the ordered boundary. | The call fails without poisoning; configure the standard `wal.log`. A fresh sequence-zero WAL is eligible. |
| Checkpoint provider preparation fails | Ordered checkpoint returns the provider error before rotation begins. | The committer remains usable; fix the provider and retry. |
| Other checkpoint rotation errors or panics | The lower-level error cannot always prove which side of the MANIFEST commit point was reached. | The committer is poisoned; close and recover before accepting more writes. |
| Schema drift (closed graph)            | `CoreProvider` validates declared `GraphType` against the snapshot's bound type. | `GraphError::Provider` at recovery time. |

The persistence layer's posture is: a recoverable failure (torn tail)
recovers transparently; an unrecoverable failure (body hash mismatch)
refuses to start, so a stale or corrupt artifact does not silently degrade
the engine's state.

## Configuration recipes

Three concrete configurations that cover common deployment shapes.

### Workstation / development

Goal: fast iteration; some data loss on crash is acceptable; recovery time
matters less than throughput.

```rust
use selene_persist::{SyncPolicy, WalConfig};

let wal_config = WalConfig {
    sync_policy: SyncPolicy::EveryN(64), // group-commit every 64 entries
    snapshot_seq: 0,
};
// Checkpoint at end of session, or every 5 minutes of wall-clock.
```

### Embedded edge

Goal: durable per commit; recovery from WAL only (no snapshots); minimal
storage footprint.

```rust
use selene_persist::{SyncPolicy, WalConfig};

let wal_config = WalConfig {
    sync_policy: SyncPolicy::EveryN(1), // durability per commit
    snapshot_seq: 0,
};
// Skip snapshots entirely. WAL replay rebuilds the graph on every restart.
// Caution: replay time grows linearly in WAL length. If that becomes too slow,
// adopt SharedGraph::checkpoint;
// prune superseded snapshots and WAL archives separately with RetentionPolicy.
```

### Server workload

Goal: durable per commit; bounded recovery time; hot backups available.

```rust
use selene_persist::{SyncPolicy, WalConfig};

let wal_config = WalConfig {
    sync_policy: SyncPolicy::EveryN(1),
    snapshot_seq: 0, // overwritten on open from the snapshot's epoch
};
// Call SharedGraph::checkpoint hourly or after every 1M WAL entries.
// Retain the last N snapshots plus their WAL files.
```

The hot path for all three is identical — `WalWriter::append` — and differs
only in how often `fsync` runs and how often a checkpoint is taken. See
[performance.md](performance.md) for the measured impact of each choice.

## Reference

- WAL writer: [`crates/selene-persist/src/writer.rs`](../crates/selene-persist/src/writer.rs)
- WAL file header: [`crates/selene-persist/src/file_header.rs`](../crates/selene-persist/src/file_header.rs)
- Snapshot writer: [`crates/selene-persist/src/snapshot_writer.rs`](../crates/selene-persist/src/snapshot_writer.rs)
- Snapshot file header: [`crates/selene-persist/src/snapshot_file_header.rs`](../crates/selene-persist/src/snapshot_file_header.rs)
- Recovery orchestrator: [`crates/selene-persist/src/recovery.rs`](../crates/selene-persist/src/recovery.rs)
- Shared persistence read guard: [`crates/selene-persist/src/manifest_lock.rs`](../crates/selene-persist/src/manifest_lock.rs)
- Recovery provider trait: [`crates/selene-persist/src/provider.rs`](../crates/selene-persist/src/provider.rs)
- `Change` enum: [`crates/selene-core/src/changeset.rs`](../crates/selene-core/src/changeset.rs)
- Coordinated graph checkpoint facade: [`crates/selene-graph/src/checkpoint.rs`](../crates/selene-graph/src/checkpoint.rs)
- Graph-side recovery wrapper: [`crates/selene-graph/src/recover.rs`](../crates/selene-graph/src/recover.rs)
- Graph compaction policy and transform: [`crates/selene-graph/src/compaction.rs`](../crates/selene-graph/src/compaction.rs)
- Ordered live compaction publication: [`crates/selene-graph/src/shared.rs`](../crates/selene-graph/src/shared.rs)
