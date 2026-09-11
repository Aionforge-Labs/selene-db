# Coherent format-2 checkpoint and facade reopen (F02-PR05)

This is the first public fallible durable facade, not durable-preview, GA,
conformance or power-loss qualification. `release_claimable=false` and GT03 remain
unchanged. [PR04](durable-commit.md) owns the separate transaction outcomes;
[PR06](rotation-retention.md) adds explicit checkpoint-coupled rotation/retention,
PR07 owns the comprehensive recovery campaign, and PR08
legacy deletion and the durable-preview contract.

## Recorded owner decisions

1. **Serialize checkpoint writes.** Hold the existing facade serial write
   reservation through image encoding, validation, file synchronization and
   durable CURRENT publication. Existing immutable readers remain valid. This
   does not promise concurrent writes during checkpoint I/O or that every new
   request avoids waiting. PR06 owns any later concurrency optimization.
2. **Rebuild all first.** Before open returns, reconstruct every retained
   supported scalar, edge-property, composite, vector and text index. No optional
   deferral, background readiness or silently omitted accelerator. Any required
   or optional reconstruction failure returns a typed error and no Database.
   Retained ineligible backing is rebuilt but remains ineligible. Existing
   kind-drift/NaN probe-decline and exact-scan rules are preserved.
3. **Extend the public Rust schema builder.** The prior property-free builder
   could not produce the default/constraint consumer fixture. Add facade-owned
   property and endpoint-oriented edge definitions using the existing `Type`,
   `Value`, name and graph validators. Preserve property-free callers. This does
   **not** enable unsupported GQL catalog property/edge grammar or claim complete
   GG02 support. Index construction continues through the existing supported
   mutation calls/DDL; this slice adds no ignored directionality or index knobs.

Named graph data still conforms to both the shared named definition and the
instance definition. Unused local declarations may differ; they cannot widen the
named data contract. Existing single-property UNIQUE and immutable rules are
reused, not replaced by a new constraint system. New graph constraint descriptors
are staged with the graph in the same outer catalog transaction.

## Public ownership and lifecycle

- `Database::builder().build()` remains infallible and memory-only.
- `Database::create(path)` strictly creates a store in an **already existing**
  directory. `Database::open(path)` opens only an existing format-2 database.
  Neither is open-or-create, overwrite, migration, salvage or fallback-to-empty.
- `DatabaseDirectory::from_file(file, locator)` accepts an already-open directory.
  `create_in`/`open_in` operate relative to it. The locator is diagnostic only.
  Path wrappers anchor once; final managed artifact symlinks are rejected.
- Native Linux/macOS filesystem semantics are required: permanent file-lock
  domains, same-directory atomic rename/hard-link publication and file/directory
  synchronization. Unsupported platforms return typed errors.
- Database clones, Catalog handles and Sessions share the Arc ownership root.
  Dropping Database alone does not release LOCK while a Session remains. Open
  fails contention until all such owners close. No second WAL voter is installed.
- `Database::open_mode()` reports the actual instance mode. `config()` remains
  the common memory-builder settings; it is not a persistence selector. There is
  no setter accepting a durable mode that `build()` would ignore.
- `StorageError` has a facade-owned category, phase and retained causal chain.
  Transaction failures retain `Error::durable_commit_outcome()` separately.
  `MutationIndeterminate` keeps its already-published in-memory meaning.

Open allocates a new process-local DatabaseId and reference domain. Durable
StoreId, epoch, catalog identities and published/deleted node/edge floors survive.
TransactionId is process-local and may restart; sequence, publication and catalog
generation are distinct concepts. The facade currently emits exactly one WAL
record for each outer publication and validates that relation on open/checkpoint.
Every catalog high-water domain is encoded; unsupported binding-table allocation
is rejected rather than reset. The retained procedure watermark is not replaced
with the current builtin count.

Unpublished allocation burns remain process-local until captured in a later
published graph's high-water metadata. Checkpoint records the actual published
metadata, not the allocation counters of outstanding detached work. Advancing
those counters in an image would incorrectly reject a still-valid detached
transaction that commits after checkpoint. Already captured holes/deletions/burns
remain consumed after checkpoint and later WAL-only reopen.

## One pinned view and one lock order

The lifetime LOCK proof precedes the facade writer reservation, durable-owner
mutex and exclusive persistence epoch. The actual pinned `DatabaseState`
supplies catalog, named bodies and graph snapshots. Checkpoint does not serialize
the retained PR04 preflight candidate as a substitute. A fenced owner or live/WAL
boundary mismatch is rejected before publication. Encoding failures release the
reservation without touching authoritative bytes. Publication/I/O failures fence
the owner; no later append or checkpoint is admitted on it.

The PR05 implementation took existing LOCK before one shared epoch, retaining it through
snapshot use, complete WAL verification, isolated semantic replay, native
declaration validation and runtime reconstruction. It does not nest an exclusive
upgrade. PR06 replaces the through-use epoch with an owned artifact lease
established under the selection epoch; legacy guard users are unchanged. Only
after all validation does the engine open the retained segment for appends,
seek to the verified complete cursor and explicitly sync that complete tail.
Previously complete but unacknowledged records are recovered, never called
canceled or overwritten. RecoveryInfo reports work and positions, not historical
acknowledgment proof. Failed construction drops all staged runtimes/threads/locks.

## Native snapshot and selected control

The new snapshot is **not SLSN** and never calls legacy SnapshotBuilder/Reader.
All fixed integers are little-endian; no Rust enum/layout, RowIndex, DatabaseId,
query reference, path, accelerator content or callable code is stored.

| Offset | Bytes | Snapshot header field |
|---:|---:|---|
| 0 | 8 | `SLSNP2\0\0` |
| 8 | 4 | major 2 / minor 0 (two u16) |
| 12 | 4 | flags/reserved = 0 |
| 16 | 8 | exact semantic body length |
| 24 | 8 | pinned outer publication ordinal |
| 32 | 16 | StoreId UUID bytes |
| 48 | 8 | store epoch |
| 56 | 32 | retained WAL segment anchor |
| 88 | 8 | complete WAL sequence |
| 96 | 8 | exact end offset within that segment |
| 104 | 32 | complete WAL boundary digest |
| 136 | 32 | BLAKE3 of the preceding 136 header bytes |

The body follows the 168-byte header. The trailer is `SLSNEND2` plus BLAKE3 of
every preceding file byte. Total overhead is 208 bytes. The selected descriptor
provides expected context, length and digest; input bytes cannot select their
own trusted context. Exact lengths, unknown versions/reserved bits, truncation,
integrity, missing/extra sections, duplicate identities and trailing bytes fail.

Semantic body version 1 contains mandatory tags 1/2/3, in order:

1. **Full catalog**, not a delta: generation, nine u64 watermarks in the existing
   catalog domain order, count and every explicitly encoded descriptor, including
   native metadata. There is no dependency on the current process's seed.
2. **All named types**: count, sorted stable type IDs and complete logical bodies.
3. **All graphs**: count, sorted graph IDs and full creation-only graph images,
   generation, node/edge floors, complete instance definitions and retained
   backing IDs. Node/edge creation records reuse the explicit semantic field
   codec in [logical transactions](format-2-logical-transactions.md), including
   mixed directionality, canonical endpoints and recursive stored values.

The new `SLDM` bounded control envelope retains full compatibility/store/epoch/
generation/parent metadata, the **independent original WAL anchor**, and the
snapshot descriptor. CURRENT hashes exact selected bytes. Changing CURRENT's
manifest digest does not change the original WAL context. Unselected ancestors
are not read. Unkeyed BLAKE3 detects corruption, not intentional rollback/forgery.

Publication writes a new snapshot staging file, validates its actual staged
bytes, synchronizes it, publishes its immutable name and synchronizes the
directory before staging/selecting the new immutable manifest. CURRENT replacement
and final directory sync use the existing uncertainty protocol. Orphan/staged/
newest files are never chosen. Old snapshots, manifests and the entire WAL remain
retained. Repeated checkpoints therefore grow storage. An orphan collision is not
silently adopted or overwritten; cleanup/retention policy remains PR06.

For preserved SLLM/SLDM selections, open verifies **the entire retained WAL prefix from its original
anchor**, checks the checkpoint's exact sequence/offset/digest placement, then
semantically applies only complete suffix records. This is not prefix-free or
constant-cost recovery. Any damaged prefix, bad suffix or incomplete unsealed
tail fails without truncation, repair, or publication of a healthy-looking prefix.
An explicit successful PR06 checkpoint selects SLRM with a new declared segment
base; that selection no longer depends on the snapshot-covered old prefix. Open
never performs this transition implicitly.

## Bounds and reconstruction

The complete aggregate semantic image uses one tightened-or-default codec budget:
256 MiB encoded body, 256 MiB conservative allocation charge, 1,048,576 work/items,
4096 metadata entries, stored-value depth 256, and the graph owner's narrower
schema depth 64. The budget is **not reset per graph or section**. Snapshot I/O
checks the header and exact file length before full-buffer allocation. Input/output
buffers, runtime state and preflight can coexist; this is not a 256 MiB process
RSS promise. Limit errors are typed and do not return a partial Database.

Eager runtime reconstruction has a separate cumulative 256 MiB charge domain for
retained primary state, graph copies, every registration, variable-width keys and
ANN configuration costs. It never silently skips optional indexes when a limit
is exceeded. Scalar drift remains visible through the existing declined probes.
The public Rust builder additionally limits schema/default entries to 4096 and
rejects lossy type adapters (for example bounded lists, narrower integer types,
unions, query references and unsupported list-of-record schemas).

Rust defaults accept already-native Value representations. JSON text and vector
numeric lists need an explicit native conversion first. Recursive LIST and RECORD
defaults use existing structural validators; open records preserve exact source
field names. The builder rejects duplicate canonical property/type names, duplicate
record fields, missing endpoints, wrong/null defaults and nested refs/paths.

## Evidence and limits of the claim

Facade-only integration exercises the public Rust builder, named schemas/graphs,
defaults, UNIQUE, mixed edges, explicit commit/rollback, suffix index changes,
fresh references, repeated reopen and further commits. A graceful subprocess
restart witness verifies another process can reopen/checkpoint/append; it is not
a process-kill or loss-of-page-cache experiment. Deterministic schedules prove
the waiting-writer/held-reader boundary and rejection after fenced commit outcomes.

Native-file tests interrupt snapshot create/write/partial-write/sync/publish,
manifest and CURRENT seams; mutate selected identities/boundaries/checksums;
preserve damaged bytes; ignore orphans; and demonstrate ancestor-independent
origin retention. Graph-owner tests exercise edge/scalar and retained-ineligible
backing, while public native searches cover every supported vector kind. A valid
17-graph public fixture with maximum-fanout HNSW registrations exceeds the
aggregate rebuild charge: open returns ResourceLimit/Rebuild, preserves every
artifact byte, releases partially constructed runtimes/LOCK, and repeats the
same typed rejection rather than returning partial readiness.

The excluded persist harness compiles all eight targets. Pinned-nightly native
60-second smoke runs completed for logical snapshot, control and logical WAL:
305,742 / 1,197,555 / 399,389 executions, respectively, in the final runs.
Sanitizer-process peak RSS was 254 / 492 / 406 MiB; these include fuzzer/quarantine/corpus state, not production
per-image allocation. Corpus and artifact directories remain under the harness.
These runs are smoke evidence, not exhaustive decoding or PR07/F06 qualification.

See the `durable_checkpoint` section of the repository-root `BENCHMARKS.md`
for actual checkpoint pause, first open, prefix/suffix replay, eager rebuild,
storage growth and native isolated-process RSS measurements. The temporary
isolated `RecoveryState` semantic adapter calls no legacy byte decoder; its
replacement remains F02-PR08-owned. This slice adds no rotation, prune, repair,
background recovery or conformance claim.
