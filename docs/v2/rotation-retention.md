# Format-2 rotation, artifact leases and explicit retention (F02-PR06)

This extends [checkpoint/reopen](checkpoint-reopen.md), not the format-1 path.
No release, conformance, power-loss, automatic repair or PR07 recovery-campaign
claim is made. Native Linux/macOS local filesystem assumptions remain unchanged.

## Owner decisions

1. **Explicit prune only.** `Database::checkpoint()` publishes and rotates;
   `Database::prune()` separately requests cleanup. No checkpoint auto-deletes.
   `PruneOutcome` separates durably removed artifacts, retained/deferred bytes and
   concrete cleanup errors. Publication success is not cleanup success.
2. **Latest two.** Retain CURRENT and one previous completed checkpoint, all
   snapshot/WAL/control dependencies of each, and every active artifact lease.
   There is no configurable weaker retention floor in this slice. A corrupt
   CURRENT never authorizes fallback to history or newest-file selection.
3. **Preserve PR05 reads.** SLLM/SLDM format-2 stores retain their original
   single-segment interpretation and full-prefix verification. Open neither
   converts nor rewrites them. Only a successful explicit subsequent checkpoint
   selects the rotating layout. This is format-2 continuity, not 1.x migration.

## One writer, view and publication

The permanent StoreWriter LOCK precedes the facade write reservation, durable
owner mutex, and exclusive MANIFEST.lock epoch. Checkpoint holds that reservation
through actual DatabaseState encoding, semantic validation, snapshot I/O and
selection. No catalog/graph authority, provider vote, grammar or index semantics
are added. All native indexes are still eagerly reconstructed or open fails.

Bootstrap uses the existing initial SLDM snapshot. Subsequent checkpoint:

1. Reject a fenced owner or live/synchronized boundary mismatch. Verify the old
   selected snapshot and complete WAL through that exact sequence/offset/digest
   before creating any new artifact, so a valid memory image cannot hide corruption.
2. Write, validate, synchronize and immutably publish a complete snapshot; sync
   the directory. Its boundary is the **old segment** position.
3. Verify the old segment length equals the synchronized boundary and synchronize
   it. Record its sealed end in the successor control, without appending a footer,
   truncating, renaming or otherwise changing the old segment.
4. Create a fresh named empty segment with a fresh anchor; synchronize its file
   and directory entry before selecting any control referring to it.
5. Write/sync/publish the immutable manifest; sync directory. Stage/sync CURRENT,
   atomically replace it, sync directory. Only then switch the live append handle.

Before CURRENT replacement the old selector and complete old WAL remain usable.
After replacement the new complete snapshot and segment are available. Any
publication/I/O error fences the owner. An attempted replacement or failed
post-selection handle swap is uncertain, not canceled or acknowledged. Old and
new potentially authoritative artifacts are retained; no automatic rollback or
repair discards them. The inherited append rollback still uses only the exact
synchronized boundary in the **same** active segment.

## Wire and recovery boundaries

SLRM is a distinct version-1 bounded postcard control envelope (same 4096-byte
framing and integrity primitive as other control envelopes), never a silent SLDM
reinterpretation. Its fields, in order, are:

- complete EmptyManifest compatibility/store/epoch/generation/parent metadata;
- SnapshotDescriptor (name, length, complete digest, old Position, publication);
- Rotation: new active base Position, old snapshot segment base Position, and
  previous completed CurrentSelector plus that segment's sealed-end Position.

The new name is `WAL-<64 lowercase anchor hex digits>.logical`. There is no segment
header/footer: the selected control declares the empty base. SLSNP2 and SLTXN2
envelopes are unchanged. New base sequence equals the snapshot-covered global
sequence, offset is zero, and origin digest equals the fresh nonzero segment
anchor. The first new record is global sequence + 1. Rotation itself consumes no
transaction sequence and creates no acknowledgment. Queued prepared groups carry
the full old base and are rejected after a rotation.

The snapshot boundary and its declared old base must have the same store, epoch
and segment. Offset zero means its sequence and digest equal that declared base,
not necessarily global sequence zero. Decoder expectations come from selected
control, never a frame or snapshot choosing its own trusted context. Parent links
are provenance, not mandatory recovery ancestry or authenticated rollback proof.

SLRM current recovery reads the self-contained snapshot and only its new active
segment. It does not reread a deleted old prefix. PR05 recovery still verifies its
full original prefix and exact snapshot placement. Active incomplete tails remain
non-destructive errors on reopen. Prune verifies retained sealed history against
exact length and terminal sequence/offset/digest; truncated, corrupt or trailing
sealed bytes fail. Old unselected ancestor files are not needed for current open.

## Artifact leases and lock order

Legacy PersistenceReadGuard remains shared-through-use. Format-2 LogicalReader
selects CURRENT under that shared epoch, independently opens the immutable
selected manifest, locks it shared, and captures the active WAL length **before
releasing the epoch**. The owned manifest lock pins that manifest and all its
snapshot/WAL names through delayed snapshot and WAL consumption. Append-only
suffixes do not change the captured read extent. Each reader uses its own open
file description; no already-locked clone is relocked or explicitly unlocked.

Prune owns StoreWriter and the exclusive epoch, then independently opens manifest
files writable and tries exclusive locks nonblockingly. A blocked probe retains
the root and dependencies, including their actual current file lengths. There is
no selection/registration window, global in-memory registry, PID/age heartbeat,
or reliance on unlinked-but-readable descriptors. Drop/process death releases
the OS lease without mutating store data. Permanent LOCK/MANIFEST.lock entries
are never replaced. Artifact readers do not block newer immutable publication.
New lifecycle methods have no external provider callbacks under the epoch.

## Explicit prune, debt and limits

All classification and validation precede deletion. Current and previous
completed roots are retained, then leased roots and their dependencies. Every
inspected manifest uses its exact distinct decoder, store/epoch/compatibility and
canonical filename checks. Referenced available snapshot/WAL bytes are verified;
missing required dependencies fail. Missing **obsolete** dependencies may be the
result of interrupted cleanup and do not prevent resuming removal of the rest.

Before the first unlink, prune revalidates the current selection and synchronizes
CURRENT, its manifest, snapshot, active WAL and the directory. This deliberately
reestablishes durability after reopen following an uncertain CURRENT replacement;
reopen's WAL sync alone is insufficient. Failure here performs no unlink.

Only dependencies of verified, unselected, unleased roots become candidates.
Even a complete higher-numbered staged root grants no authority; it can be
discarded only after current durability is reestablished. Unknown/corrupt manifest
inputs stop planning. Unclassified snapshots/staging files remain deferred debt,
not silently adopted or deleted. Special or multiply-linked files fail the native
capability checks. An immutable name collision is not overwritten or adopted.

Dependencies are deleted before their describing manifests. Each removal is
directory-synchronized before being counted as reclaimed. The first cleanup
failure stops further unlinking and returns remaining/uncertain entries as
Deferred plus the causal error. A subsequent explicit call may resume; healthy
commits are not fenced solely by cleanup debt. A directory-sync failure after
unlink does **not** claim those bytes were reclaimed or that the name still
exists. Reports are observations, not file leases or permanent storage totals.

Maintenance bounds its inventory to 4096 artifacts during directory enumeration,
before control payload reads, with a fixed per-root schema
and constant simultaneous probe handles. Rotation admission reserves eight slots
for an attempt's staged/final artifacts so successful repeated checkpoints cannot
exceed that maintenance bound. Call explicit prune before exhausting this budget.
Existing oversized PR05 directories can still open, but bounded prune can return
ResourceLimit without deletion. Unclassified debt may require later owner-led
reconciliation; no destructive reconciliation API is introduced here.

## Evidence and future ownership

Focused tests cover old-layout byte-preserving open, nonzero empty bases, stale
groups, default-two dependency inventory, delayed actual snapshot/WAL reads,
cross-process reader completion and kill, selection/registration rendezvous,
sealed corruption, interrupted cleanup, ENOSPC-like partial snapshot failure,
and failed durability reestablishment after uncertain CURRENT followed by reopen.
Native subprocess SIGKILL witnesses pause before CURRENT, after replacement but
before directory sync, and after unlink before prune sync. These preserve OS page
cache and are **process-crash**, not device power-loss evidence.

The existing durable_checkpoint target adds actual artifact-lease retained-byte,
reclamation/debt, rotation-stage and repeated-checkpoint/prune reopen rows; its
three-graph/native-index and 60-read/40-write comparators remain. See BENCHMARKS.md
for commands and measured ranges. Decoder fuzzing repairs integrity over a valid
SLRM seed and exercises nonzero empty snapshot bases, besides ordinary raw input.

Native pinned `nightly-2026-08-15` compiled all eight persist fuzz targets,
including a final rebuild after the old-root verification guard (log 69).
Sixty-second smoke runs used `-verbosity=0 -print_final_stats=1`:

| Target | Maximum input bytes | Executions | Sanitizer-process peak RSS (MiB) |
|---|---:|---:|---:|
| decode_control | 4096 | 798,062 | 487 |
| decode_logical_snapshot | 16384 | 323,693 | 255 |
| decode_logical | 16384 | 406,568 | 453 |

All completed without a finding; corpora and artifact directories were retained.
These RSS figures include sanitizer/fuzzer/quarantine/corpus state and are not
production database RSS limits. Raw build/run output is in task evidence logs
51–54 (earlier smoke retained in 20–23); these short runs do not replace the
broader PR07/F06 campaigns.

PR07/F06 retain the broad native corruption/crash and RC qualification campaigns.
PR08 retains legacy codec/authority deletion and the isolated RecoveryState bridge.
This slice neither adds a second recovery authority nor a format-1 retention path.
