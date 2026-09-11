# Full read-only recovery verification (F02-PR07)

`Database::verify(path)` and `Database::verify_in(&DatabaseDirectory)` validate
**full recovery readiness** for one pinned on-disk view. They do not inspect the
live database graph and are not the GQL `CALL selene.verify()` health procedure.
This is the recorded full-readiness policy, not a lightweight checksum scan.

## Shared authority and lifecycle

Open and verification use the same selected snapshot/prefix/suffix consumer and
the same facade preparation function. Both load and validate the complete logical
catalog, graph/type inventory, allocation domains and native declarations, and
eagerly reconstruct every retained supported index. There is no warning-only,
optional-index degradation or partial healthy result. Only open acquires existing
exclusive writer ownership, synchronizes the verified tail, creates a fresh
DatabaseId/mutation coordinator and publishes a Database.

Verification opens `MANIFEST.lock` **existing-only and read-only** on an
independent file description. The shared epoch still protects CURRENT selection,
independent immutable-manifest shared locking and WAL-extent capture. The epoch
is then released; the manifest artifact lease retains the required snapshot and
WAL through consumption and reconstruction. Append, checkpoint and explicit prune
can advance while verification is reading the older selection. No lock upgrade,
writer LOCK, file creation, writable open, file/directory sync, repair, registry
sidecar, or durable-provider mutation is performed by verification.

The graph owner's existing all-index rebuild creates temporary memory-only
SharedGraphs. Their existing committer ownership closes the channel and joins
workers on drop; verification does not submit mutations to them. Temporary state
and the artifact lease are released before returning on success or error. The
facade never constructs a DatabaseInner on this path. These temporary runtimes
cost real CPU/memory; the report is not a promise of cheap inspection.

Legacy read-guard callers retain their create-capable acquisition API. The new
format-2 reader uses only existing coordination entries. Missing CURRENT, required
data files or coordination state is an error, not an invitation to initialize.
Native Linux/macOS filesystem assumptions are unchanged; local macOS evidence
does not constitute exact-head Linux qualification.

## Report and diagnostics

`VerificationReport` owns the exact selected manifest basename, digest and
generation, snapshot basename/extent/covered boundary, active WAL basename and
captured extent, final complete position/digest, graph/node/edge counts and eager
index count. Timings distinguish selection, snapshot, physical WAL framing,
semantic replay and rebuild; its `RecoveryInfo.synchronize_elapsed` is zero.
The public rustdoc example is executable while a live Database owns LOCK.

Success says only that **this captured view** passes the engine's recovery
readiness checks. It is not historical acknowledgment evidence, physical/device
durability evidence, current/future write permission, authenticated rollback
protection, the latest state after selection, or a retention lease after return.
Complete unacknowledged transactions can be present. Do not automatically retry
an uncertain operation based on this report.

`StorageError` retains phase, a facade-owned typed category, bounded escaped
artifact basename, and byte cursor/expected sequence where known. Sequence
failures additionally retain the validated observed sequence. Categories distinguish
missing artifacts, integrity, structural corruption, unsupported format,
compatibility, lineage, foreign store/epoch/segment, digest lineage, gaps/overlaps,
incomplete unsealed tail, incomplete required data, semantic/native admission and
resource limits. General control lineage remains a control-level error. Native I/O
and other original causes remain available through `std::error::Error::source`.
Routine Display/Debug omit arbitrary source payloads and unrelated host paths;
callers explicitly inspecting source errors must handle their own sensitive text.
Basenames in reports/errors grant no filesystem authority.

Unrecognized namespace entries retain their bounded escaped basename, including
when CURRENT is absent; the `UnsupportedFormat` category does not hide which
entry prevented admission. Writer-establishment open, metadata, seek and sync
failures retain the selected WAL basename and verified complete boundary, while
preserving the original I/O cause. These write-side operations remain exclusive
to open, not verification.

Recognizable common framing and available fixed-header integrity precede version
and trusted-context interpretation. An unsupported-version fixture must therefore
repair the applicable integrity first. Complete record integrity is checked before
decompression or semantic decoding. A count claiming more entries than the body
can contain is incomplete; a sufficiently backed over-budget count is a resource
error before allocation. These are failures, not a salvage rule. An incomplete
captured unsealed suffix has its own category but **still fails full verification
and open**. Sealed/interior incompleteness and complete bad checksums never become
harmless tails. A failed reader stays failed; retrying cannot turn failure into
EOF or writer establishment. A snapshot must actually be consumed successfully.
In particular, an SLDM snapshot proves its covered original prefix must be
complete: truncation to zero, to an earlier complete-record end, or within a
required record is `IncompleteRequired`, not an unsealed suffix. A present expected
sequence with the wrong offset or digest remains `Lineage`. An empty SLRM segment
complete at its declared nonzero base remains valid. These checks neither add an
ancestry dependency nor permit repair.

## Continuity and bounds

SLLM/SLDM lower format-2 interpretation is unchanged; snapshot-bearing SLDM
recovery verifies its entire retained original WAL prefix. SLLM alone has no
initial full database snapshot and is not a full-readiness database result.
Explicit checkpoint upgrades to SLRM; open/verify never upgrade implicitly.
SLRM current recovery needs its selected self-contained snapshot and active new
segment, not old unselected ancestor payloads. Sealed retained-history checks
remain maintenance checks, not extra current-recovery roots. Prune retains its
strict unknown-root/no-deletion behavior and explicit latest-two-plus-leases floor.

The existing aggregate 256 MiB semantic and reconstruction charges are unchanged,
including the valid 17-graph maximum-fanout HNSW rejection witness. These are
accounting limits, **not process RSS limits**. Whole snapshot buffers, retained
semantic state, graph copies and indexes can coexist. Selected directory
classification still enumerates names under the prior policy; it is not an
input-independent memory promise. Recursive depth, work, width and metadata bounds
remain in the owning codecs. Per-transaction semantic replay can revalidate
retained state; there is no general linear-in-suffix claim.

## Evidence and remaining qualification

Fast tests include public open/verify parity and byte/namespace preservation for
SLDM and SLRM selected-control, snapshot and WAL mutations; integrity-repaired
version/identity/sequence/semantic mutations; read-only permissions and missing
coordination; actual writable-open observation; online captured-view retention
through append/checkpoint/prune; all eight vector families; native rejection; the
unchanged aggregate resource rejection; and an independent bounded sequence model.
Existing graph-owner constraints, scalar/edge/composite/text/backing and recursive
semantic tests remain part of the joined suite, not a separate test recovery engine.

The facade ACK-set model covers explicit multi-request cancellation,
committed-unacknowledged, proved rollback, uncertain cleanup, and fail-closed partial
uncertain append. Native parent-invoked children are SIGKILLed and waited at full
append, successful sync, publication, pre/post CURRENT, prune sync and captured
verification. Reopened ordered transaction identities, parts and values are
checked against independently authored expectations, with verification position
and readiness agreement. OS page cache remains intact: **process crash, not power
loss**. An unselected completed checkpoint left by a pre-CURRENT crash needs
explicit prune before name reuse; open never silently overwrites/adopts it.

Pinned `nightly-2026-08-15` builds all eight persist fuzz targets. Three serial
60-second bounded runs retained existing corpora and reported:

| Target | Maximum input | Executions | Sanitizer-process peak RSS (MiB) |
|---|---:|---:|---:|
| decode_control | 4096 B | 780,646 | 469 |
| decode_logical_snapshot | 16,384 B | 315,848 | 250 |
| decode_logical | 16,384 B | 385,449 | 542 |

These runs repair integrity around mutated valid seeds to reach semantic and
typed classification checks. RSS includes fuzzer/sanitizer/quarantine/corpus state,
not production allocation. This bounded campaign does not complete the wider
native milestone/RC stress and power-loss qualification program.
Final-source build and run output is retained in evidence log
`49-diagnostic-fuzz-refresh.log`; earlier passes and intermediate failures are retained,
not overwritten. The pure codec harnesses do not exercise filesystem enumeration,
reader EOF classification or writer establishment: real-file regressions and
native operation fault seams cover those diagnostic branches. The non-UTF-8
namespace fixture is Linux-only because the macOS filesystem rejects that filename
at creation; ordinary and control-character name cases are enabled on both native
platforms. The joined native macOS gate ran 5,215 tests without retries and
34 doctests; native Linux exact-head CI remains the delivery owner's gate.

See the PR07 full-readiness section in `BENCHMARKS.md` for measured suffix/index
work, variability, rejection and isolated-process RSS. No GA, conformance or
durable-preview qualification is inferred. GT03/25G04/GP18 and the profile are
unchanged. PR08 still owns legacy byte-codec/entry-point deletion and the isolated
RecoveryState semantic bridge; no legacy decoder is introduced into this pipeline.
