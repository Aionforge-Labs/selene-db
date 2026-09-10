# Retained store directory and empty control

F02-PR01 supplies a filesystem capability and persistence-layer **empty-store**
control. It does not supply durable `selene-db` facade sessions, catalog/graph
reopen, logical format-2 WAL transactions, or durable query commits. The facade's
infallible in-memory builder is unchanged. F02-PR05 owns durable facade creation,
reopen and fresh process-local DatabaseId/handle binding; F02-PR08 owns deletion
of the legacy data entry points after format-2 cutover.

## Authority and supported mode

`StoreDirectory::open(path)` anchors one existing directory; initial ambient
resolution is outside the post-anchor guarantee. `StoreDirectory::from_file`
accepts a caller-owned already-open directory file and validates its metadata.
That API covers the gap between caller setup and later engine use. The handle,
not `locator()`, grants authority. Clones retain the same physical directory.
Renaming/replacing the real root or ancestors, changing CWD, or retargeting an
original alias cannot redirect subsequent managed operations.

The backend uses the already-resolved **rustix 1.1.4** safe filesystem APIs.
Every child name is validated before operations: nonempty single components
only, no `.`, `..`, slash, backslash, colon, NUL, or absolute names. Opens use
`NOFOLLOW`, `CLOEXEC`, `NONBLOCK`, and `NOCTTY`; file metadata is checked again on
the obtained handle before any content mutation. The nonblocking flag prevents
a raced FIFO from hanging validation. No `/proc/self/fd` paths, CWD switching,
custom syscalls, raw descriptors, or unsafe code are used in the engine.

Managed files must be regular, single-link entries. External hard links,
symlinks, devices, sockets, and directories are not data artifacts. Legacy
no-overwrite publication briefly creates an internal hard link, then removes
its temporary name. Unlink/rename never follow a symlink target. Non-cooperating
mutation of entries *inside* a live store, including removal/replacement of its
coordination files, is unsupported; this is not a sandbox against a directory
owner who can alter arbitrary file contents. A leftover externally aliased
artifact fails closed, rather than permitting writes through an external link.

| Native host | Mode |
|---|---|
| Linux | Safe handle-relative filesystem mode; exact-head hosted qualification required |
| macOS | Safe handle-relative filesystem mode; native APFS exercised locally |
| Other platforms | `PersistError::Directory(DirectoryError::UnsupportedPlatform)` at capability construction |

Use a **local filesystem** honoring advisory file locks, same-directory atomic
rename, exclusive publication, hard links, and file/directory synchronization.
Unsupported operations return their native I/O error; there is no pathname or
weaker-sync fallback. Network/shared filesystems and devices that lie about
flush completion are not certified. Native evidence here is Apple M5, macOS
27.0 build 26A5425a, APFS on the internal Data volume, Rust 1.97.1. Linux evidence
belongs to the parent's exact-head hosted check, not a local cross-compile.

File `sync_all` / `sync_data` and retained directory `sync_all` remain the
standard-library operations used by legacy persistence. Inspection of pinned
Rust 1.97.1 `std/src/sys/fs/unix.rs` shows both sync operations use
`fcntl(F_FULLFSYNC)` on Apple, without a weaker fallback, and fsync/fdatasync on
Linux. The backend does not substitute plain macOS fsync. These requests plus
fault/process tests are **not a physical power-loss qualification**.

## Ownership and lock order

`StoreWriter::acquire` nonblockingly locks the permanent `LOCK` inode. An
independent acquire through an alias or cloned directory fails while ownership
is retained. Cloning an existing `StoreWriter` explicitly shares its owned
lease; no global registry supplies reentrant permission. WAL and audit have
additional file locks, so sharing store ownership does not allow two writers
to the same data file. The builder and recovery adapters compose WAL/audit
using the existing lease, including audit-first builder configuration.

The ordering is `LOCK` → existing WAL inode → `MANIFEST.lock` epoch → replacement
WAL temporary. Missing-WAL recovery is the existing exception: verify recovery
under the shared epoch first, then use a **nonblocking** store/WAL writer open.
It cannot wait in the reverse order. Neither `LOCK` nor `MANIFEST.lock` can be
unlinked, hard-linked, or replaced by managed cleanup/publication methods.

`PersistenceReadGuard` holds the shared epoch from authoritative selection
through all selected artifact use. Rotation, retention, standalone snapshot
publication, direct legacy MANIFEST publication, audit prune, and empty-control
publication use the exclusive side **and the same owned StoreWriter proof**.
`ManifestEpochGuard` can only be constructed from `&StoreWriter` and retains a
clone of that lease until after releasing the epoch. No epoch-only mutation
exception remains. Standalone `Manifest::write_atomic[_in]`,
`SnapshotBuilder::finalize`, and `retention::prune[_in]` acquire writer ownership;
they fail with `WriterLockHeld` if an independent writer owns the directory.
Online callers use `write_atomic_with_authority`, `finalize_with_authority`, or
`prune_with_authority` with the existing lease. Snapshot builders reject a
foreign directory authority before creating an epoch entry or artifact.
Checkpoint/rotation and audit reuse their owned proof; standalone graph export
passes the lease it holds through provider encoding into snapshot finalization.
Even a no-MANIFEST prune of an existing directory acquires the coordination
entries, though it still deletes no data without an authoritative epoch.
Readers remain independent of StoreWriter. Append-only writes may continue
while a read guard exists. Recovery callbacks must not re-enter
same-directory epoch mutation. No guarded internal path calls a public method
that reacquires the same conflicting epoch lock.

`CheckpointOutcome` and `WalWriter::path()` are locators, not leases. Use retained
`directory()` authority, re-read the guarded MANIFEST, then open validated child
names through that capability. A saved absolute path cannot prove store identity.

## Empty-control protocol

`control::EmptyStoreControl::{create_empty, open}` consume retained directory
authority, with exact caller-supplied `CompatibilityIdentity`. That record holds
bounded profile/collation names (96 UTF-8 bytes each), profile version/hash,
Unicode version tuple, and collation version. Persistence imports no upper
engine layer and does not invent compatibility policy.

- `StoreId` is opaque UUID-v4 identity generated by the existing uuid crate.
- `StoreEpoch` starts at one and persists across control reopen.
- `ManifestGeneration` starts at one, advances with checked addition, and never wraps.
- Physical directory identity and process-local DatabaseId are separate domains.
- `publish_empty` advances only empty control; it is not a query commit API.

The distinct `SLEM` immutable manifest and `SLCU` CURRENT selector use version-1
postcard envelopes with BLAKE3 checksums. Each complete record is capped at
4096 bytes before decode/allocation. File reads also cap bytes actually read,
including growth after metadata inspection. A selector binds exact manifest
bytes, StoreId, epoch, generation, and canonical
`MANIFEST-{20-digit-generation}.control` name. Manifests declare empty storage
format `[2, 0]` and retain the previous generation/digest as publication
provenance. **Open validates only CURRENT and the self-contained selected
manifest**, including their checksums, names, StoreId, epoch, generation, format,
compatibility identity and structural adjacent-parent generation. It does not
open or require ancestor files. A parent digest may remain after its file has
been removed; this does not require an epoch reset or alter the selected state.
Unselected regular-file payload corruption is irrelevant to recovery, while
selected corruption still fails. Unknown/special-file directory entries remain
subject to namespace validation. Checksums are not authenticated rollback
protection, and a complete selected pair can legitimately be copied elsewhere.

Reopen and ordinary publication-base validation each perform two control payload
opens regardless of generation. Per-capability counters test generations 1, 32
and 256 before/after history removal. This is **not constant-time overall open**:
directory enumeration/stat validation remains O(number of entries), with an
entry-name vector of the same order. F02-PR06 owns production retention policy;
this slice proves that unselected history can be removed without making it a
permanent reopen dependency.

Publication under writer/exclusive epoch ownership:

1. Validate the current selected state, structural provenance, compatibility and directory contents.
2. Exclusively create a UUID-named `.control.*.tmp`; encode, write, and sync it.
3. Publish the immutable name with safe native no-replace rename. An existing
   generation is accepted only for exactly identical intended bytes; it is
   synced again. Synchronize directory entries.
4. Stage/write/sync CURRENT. Initial creation uses no-replace publication;
   later publication atomically replaces the selector.
5. Synchronize directory entries before returning success.

Failure before selector publication leaves the old complete state (or no
initialized store on first creation). Once selector replacement is attempted,
uncertain I/O is not classified as definitely canceled: the returned
`PublicationUncertain` requires reopen and fences later publication on that
handle. A failed exclusive-create collision cannot overwrite the prior bytes.
Audit prune likewise fences its old inode after uncertain replacement rather
than acknowledging invisible appends to it.

Unselected immutable generations are history or orphans, never implicitly
authoritative and never required solely by the parent provenance link. A failed
update may retry a byte-identical next-generation orphan. Initial creation refuses
unpublished artifacts instead of guessing their StoreId. Missing CURRENT means
`NotInitialized`, not an empty recovered database. A valid old CURRENT plus a
new unselected generation is also a legitimate interrupted publication; the
protocol cannot distinguish that from an external rollback of the selector.
Live handles reject a changed selector as stale. Corrupt/missing selected files,
mixed IDs/epochs/generations, incompatible identity, noncanonical names and
unsupported versions fail before authoritative publication.

Empty control allows only its own files and permanent coordination entries.
Legacy WAL/snapshot/MANIFEST/audit data and unknown entries are rejected. Legacy
data APIs reject CURRENT, immutable-control, and control-staging directories, including attempts to
name a legacy WAL as CURRENT. Existing data encodings remain WAL 3.1, snapshot
1.6, MANIFEST 1, and audit 2; no 1.x migration or new transaction encoding is added.

## Filesystem authority audit

| Surface | Retained authority and remaining path role |
|---|---|
| StoreDirectory/native | One ambient root open; all child open/stat/list/rename/link/unlink are fd-relative. `File` metadata/sync are handle operations. |
| WAL initialization, writer, append, rotation/reset/archive | StoreWriter retains directory/lock; append/rollback/sync retain the WAL file. `writer.path` and outcomes are diagnostics. Temps and archives are validated child names. |
| WalReader/SnapshotReader | Reader child names resolve only through retained directory; streaming/sections use owned files. |
| SnapshotBuilder/discovery/identity comparison | Constructor anchors once; publication requires StoreWriter and a matching directory before obtaining the epoch. Config path is diagnostic. Discovery returns child names internally; identity checks open through the same directory. |
| Legacy MANIFEST and epoch guards | Exclusive guard retains StoreWriter proof; direct path wrappers anchor and acquire once. Read guards retain only StoreDirectory. `dir()` remains diagnostic; `directory()` is operational. |
| Retention/audit | Every prune requires existing or newly acquired StoreWriter proof and an exclusive epoch. Scans/deletes/rewrite/temp cleanup stay relative. Audit owns the composite lease and retained file. |
| Graph builder/recovery | Path inputs anchor at entry; private recovery carries capability through replay, writer and audit attachment. Repeated builder input locators select already-owned authority, not a newly resolved directory. |
| CORE/checkpoint/export | CheckpointTarget carries the capability separately from diagnostic outcomes; standalone export anchors before provider callbacks and retains it through finalize. |
| Empty control | Publication/staging receive the writer-proven exclusive epoch. Reopen reads only CURRENT and its selected manifest, not ancestor payloads. All I/O uses retained StoreDirectory; pure byte decoders never access files. |

The canonical initial locator and path convenience constructors remain for
callers; internal managed operations never open `locator().join(...)`.
Relative `PathBuf` fields used for child names are identifiers, not ambient
authority. F02-PR08 deletes obsolete legacy entry points, not this capability.

Native tests cover real root replacement, already-open files, alias/CWD changes,
file-name and post-preflight symlink rejection, process/clone writer exclusion,
epoch contention, graph checkpoint/recovery, snapshot export, audit and retention,
plus the empty-control publication failure matrix. See `store_control` in
[BENCHMARKS.md](../BENCHMARKS.md) for timing boundaries and commands.
