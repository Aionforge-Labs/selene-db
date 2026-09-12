# Persistence and recovery

This document describes the format-2 `selene-persist` data lifecycle,
recovery windows, and backups. Format 1 was cut over in F02-PR08: the only
supported on-disk format is format 2, the only durable entry points are the
`selene-db` facade (`Database::create` / `open` / `checkpoint` / `verify`),
and retired `SLDB` / `SLSN` / `SLMF` / `SLAU` headers are rejected by a
bounded read-only probe before any write access. There is no dual decoder, no
version dispatch, no migrator, and no audit log in the preview.

The persistence subsystem lives entirely inside the
[`selene-persist`](../crates/selene-persist) crate. It owns the format-2
control file, the logical WAL segments, and the logical snapshots, plus the
recovery, checkpoint, rotation, and prune operations over them. It depends
only on `selene-core`, never on the graph types, by design. The in-memory
graph in `selene-graph` performs no persistence I/O of its own: durability
below the graph layer is the owning database handle's authority. Filesystem
authority is retained [`StoreDirectory`](store-directory-control.md); see
that document for the file, lock, platform, and empty-control contract.

## Artifacts

A format-2 store directory holds:

| Artifact | Name | Purpose |
| :------- | :--- | :------ |
| Control | `CURRENT` + `MANIFEST-{generation:020}.control` | Selected manifest generation: compatibility identity, live snapshot selection, WAL segment selection. |
| Logical WAL | `WAL-{segment:020}.logical` (first segment `WAL-00000000000000000001.logical`) | Framed logical-transaction bodies appended and read through the logical stream. |
| Snapshot | Selected checkpoint bytes named by the manifest selection | Immutable checkpoint image with its integrity digest and covered WAL boundary. |
| Locks | `LOCK`, `MANIFEST.lock` | Permanent writer ownership and the shared/exclusive persistence-epoch lock domain. |

`CheckpointOutcome` paths are diagnostic locators, not retention leases.
Snapshot and WAL bytes are checksummed envelopes: corrupt format-2 inputs
fail explicitly and never fall through to a legacy or empty-store path.

## Writers, locks, and the epoch domain

Writer ownership is one permanent `LOCK` domain. Every managed
publication and prune requires the `StoreWriter` proof; standalone
conveniences acquire it, while online callers pass the existing lease.
Rotation, prune, and direct control publication take the exclusive epoch
side; recovery, backup-style reads, and `LogicalReader` consumption hold
the shared side (`PersistenceReadGuard`) from authoritative selection
through artifact use, so epoch mutation cannot switch or delete the
selected epoch midway. Ordinary appends may continue under a shared guard.
Prune is explicit, never automatic, and retains the latest two completed
checkpoints plus active artifact leases; the lock file is coordination
state and must not be copied into a backup.

## The legacy probe

[`legacy_probe.rs`](../crates/selene-persist/src/legacy_probe.rs) is the
only format-1 code that remains, and it is not a decoder. It recognizes the
retired `SLDB` (WAL/archive), `SLSN` (pre-`SLSNP2` snapshot), `SLMF`
(manifest), and `SLAU` (audit) magic prefixes and returns
`PersistError::UnsupportedVersion` naming the artifact family. It never
mutates the store and offers no conversion. The probe runs inside every
managed open path — control create/open, logical-stream reader, recovery
and reopen, manifest-lock acquisition, and `StoreDirectory` coordination —
so a format-1 header is rejected before write access, no LOCK file is
created, and the foreign bytes are left untouched.

## Writing — the logical stream

Durable commits append framed logical-transaction bodies to the selected
WAL segment through [`LogicalWal`](../crates/selene-persist/src/logical_stream.rs)
(`prepare` then `commit` under the publication protocol). The stream
enforces exact sequence order: a gap, an overlap, or a digest/lineage
break fails the commit rather than skipping records. An incomplete final
unsealed tail is reported, not repaired; no repair is authorized.

## Commit and checkpoint outcomes

A facade commit is acknowledged only after its frames are durably
published; an acknowledged commit survives reopen. Past the in-graph seal,
an `Err` does not mean "the transition did not happen": the engine reports
`GraphError::IndeterminateOutcome` (GQLSTATUS `40003`, statement completion
unknown) rather than a rollback it did not perform.

### What to do on `IndeterminateOutcome`

1. Stop issuing work on that handle — the committer is poisoned, and every
   later submit fails fast with the same status.
2. Drop the handle and reopen through the owning database handle
   (`Database::open`). There is no in-graph recover entry point.
3. **Read back** to determine whether the transition landed.
4. Only then decide whether to retry.

Retrying without the read-back double-applies any commit that survived. If
writes are not naturally idempotent, carry an application-level identity so
the re-drive after a reopen is safe. The engine does not supply one. Test
`GraphError::requires_reopen()` rather than matching a variant.

A checkpoint (`Database::checkpoint`) snapshots the caller's pinned image
at the established live boundary, publishes it with its integrity digest
and covered WAL boundary, and on rotation moves to a fresh segment without
resetting sequence. Old artifacts remain until explicit prune. Any
publication or I/O error fences the owner; reopen is non-destructive.

## Recovery and verification

Recovery selects the authoritative manifest generation, verifies the
selected snapshot's integrity digest, then replays WAL bodies past the
covered boundary in exact sequence order. A digest or lineage failure is a
hard failure: recovery never falls back to an older snapshot or skips a
bad frame.

`Database::verify` / `verify_in` share full open readiness through a
read-only, existing-only selection epoch and artifact lease. They never
acquire the writer LOCK and never publish: reports cover only the captured
on-disk view, not acknowledgment, physical durability, write permission,
or subsequent freshness. See [recovery
verification](v2/recovery-verification.md), [checkpoint /
reopen](v2/checkpoint-reopen.md), the [rotating
lifecycle](v2/rotation-retention.md), and [durable
commit](v2/durable-commit.md) for the tracked 2.0 contracts.

## Retention and prune

Prune is explicit (`Database::prune` / stream `prune`), never automatic.
It retains the latest two completed checkpoints plus active artifact
leases, probes shared-side leases nonblockingly under writer proof and
the exclusive epoch, revalidates `CURRENT` and the selected dependencies
before unlink, and reports what it retained and removed. Unrotated
selections verify the full WAL prefix; a rotating reopen verifies only
the independently selected new segment.

## Backups

Back up a selected epoch, not a live directory listing:

1. Acquire `PersistenceReadGuard` over the retained directory authority.
2. Re-select the manifest generation under the guard and copy the named
   live snapshot plus the covered WAL-segment prefix while the guard is
   held, so rotation and prune wait.
3. Never copy `LOCK`, `MANIFEST.lock`, temporaries, or crash orphans.
4. To restore, place the copied artifacts in a directory with no stale
   control files and open normally; recovery verifies before use.

## What can go wrong

The posture is: a recoverable condition (incomplete tail) is reported
transparently; an unrecoverable one (digest mismatch, lineage break)
refuses to start, so a stale or corrupt artifact never silently degrades
engine state. Facade-facing kinds live in
[`StorageErrorKind`](../crates/selene-db/src/durable/error.rs):

| Failure | Detection | Outcome |
| :------ | :-------- | :------ |
| Retired format-1 header | Magic-prefix probe on every managed open | `PersistError::UnsupportedVersion` naming the artifact; store untouched, no LOCK created. |
| Uninitialized directory | Missing `CURRENT` / lock files | `NotInitialized`; strict create refuses an occupied directory (`AlreadyInitialized`). |
| Foreign or mixed artifacts | Directory capability and lineage checks | `ForeignStore` / `ForeignEpoch` / `ForeignSegment`, or mixed-artifact refusal. |
| Corrupt selected bytes | Envelope digest / structural validation | `Integrity` / `Corruption`; recovery fails closed, never falls back. |
| Sequence gap or overlap | Exact-order stream accounting | `SequenceGap` / `SequenceOverlap`. |
| Incomplete final tail | End-of-segment framing | `IncompleteTail`; reported, no repair authorized. |
| Missing selected artifact | Selection-time existence check | `MissingArtifact` (not permission denied). |
| Lock contention | Permanent writer / epoch lock domain | `Contention` (`WriterLockHeld` below); stop the live owner. |

## Configuration guidance

There is no sync-policy knob, no WAL writer options struct, and no
background checkpoint timer in the preview. The embedder owns cadence:

- Checkpoint after every `N` commits, every `T` minutes of wall-clock, and
  before a planned shutdown — whichever arrives first.
- Snapshot encoding is linear in live graph state and forms a
  write-publication barrier; schedule it in a lower-write window when tail
  latency matters.
- Run row-compaction maintenance from the embedder's existing cadence
  (see `SharedGraph::compact` / `CALL selene.compact()`), preferably
  before the next checkpoint so the dense layout is captured durably.
- Prune explicitly after successful checkpoints and verified backups;
  never copy lock files or temporaries into a backup.

## Reference

- Facade durable API: [`crates/selene-db/src/durable.rs`](../crates/selene-db/src/durable.rs)
  (`Database::create` / `open` / `checkpoint` / `verify` / `prune`).
- Facade diagnostics: [`crates/selene-db/src/durable/error.rs`](../crates/selene-db/src/durable/error.rs)
- Control generations: [`crates/selene-persist/src/control.rs`](../crates/selene-persist/src/control.rs)
- Logical stream (append / read / checkpoint / rotation / prune):
  [`crates/selene-persist/src/logical_stream.rs`](../crates/selene-persist/src/logical_stream.rs)
- Logical frames and snapshots:
  [`crates/selene-persist/src/logical_frame.rs`](../crates/selene-persist/src/logical_frame.rs),
  [`crates/selene-persist/src/logical_snapshot.rs`](../crates/selene-persist/src/logical_snapshot.rs)
- Format-2 value/change codec:
  [`crates/selene-core/src/logical/`](../crates/selene-core/src/logical/)
- Legacy header probe (recognition only):
  [`crates/selene-persist/src/legacy_probe.rs`](../crates/selene-persist/src/legacy_probe.rs)
- Directory capability and platform contract:
  [`store-directory-control.md`](store-directory-control.md)
- Tracked 2.0 contracts: [`v2/durable-commit.md`](v2/durable-commit.md),
  [`v2/checkpoint-reopen.md`](v2/checkpoint-reopen.md),
  [`v2/rotation-retention.md`](v2/rotation-retention.md),
  [`v2/recovery-verification.md`](v2/recovery-verification.md)
- `Change` enum:
  [`crates/selene-core/src/changeset.rs`](../crates/selene-core/src/changeset.rs)
- Graph compaction policy and transform:
  [`crates/selene-graph/src/compaction.rs`](../crates/selene-graph/src/compaction.rs)
