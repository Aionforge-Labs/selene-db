//! `write_snapshot` is fail-closed on the directory and pinned on the graph.
//!
//! Two independent defects are pinned here.
//!
//! **The directory.** A standalone snapshot publishes no MANIFEST and rotates no
//! WAL, so it is not an epoch. Dropped into a managed directory it either bricks
//! recovery — a `snapshot.N.snap` with no MANIFEST makes recovery cross-check it
//! against the WAL and hard-fail — or is silently ignored and then pruned.
//!
//! **The cut.** The section loop used the unpinned `write_section` hook, and
//! `CoreProvider` re-loads the published graph on every call, so a commit
//! landing between two of CORE's eight sections produced an envelope torn across
//! generations with nothing to detect it.
//!
//! The subtle half is that a generation pin alone does not close the second one:
//! `compact` republishes with `GraphMeta` copied verbatim, so the generation is
//! unchanged across a full row renumbering.
//! `write_snapshot_rejects_a_compaction_published_mid_encode` is the one test
//! that separates a pointer-identity check from a generation check — a
//! generation check passes the other seven.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, GraphId, LabelSet, PropertyMap, db_string};
use selene_persist::{
    DEFAULT_WAL_FILE_NAME, SectionCompression, SnapshotConfig, WalConfig, WalWriter, snapshot_path,
};

use crate::error::ExistingStoreEvidence;
use crate::index_provider::{IndexProvider, ProviderError, ProviderTag, SubTag};
use crate::{GraphError, SharedGraph};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-graph-wsnap-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    dir
}

fn config(dir: &Path, sequence: u64) -> SnapshotConfig {
    SnapshotConfig {
        dir: dir.to_path_buf(),
        sequence,
        compression: SectionCompression::None,
        fsync: false,
    }
}

fn commit_node(shared: &SharedGraph, label: &str) {
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(db_string(label).unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
    }
    txn.commit().unwrap();
}

fn expect_existing_store(
    result: crate::GraphResult<selene_persist::SnapshotFinalizeOutcome>,
    expected: ExistingStoreEvidence,
) {
    let Err(error) = result else {
        panic!("write_snapshot must refuse a managed directory");
    };
    assert!(
        matches!(
            &error,
            GraphError::ExistingStore { evidence, .. } if *evidence == expected
        ),
        "expected ExistingStore({expected:?}), got {error:?}"
    );
}

/// A checkpointed directory. Its MANIFEST names the live epoch; a standalone
/// snapshot beside it is ignored and then pruned, so the write silently
/// produces nothing durable.
#[test]
fn write_snapshot_refuses_a_directory_with_a_published_manifest() {
    let dir = temp_dir("manifest");
    let graph_id = GraphId::new(82_101);
    {
        let shared = SharedGraph::builder(graph_id)
            .with_wal(dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default())
            .unwrap()
            .build()
            .unwrap();
        commit_node(&shared, "wsnap.manifest");
        shared
            .checkpoint(crate::CheckpointConfig::default())
            .unwrap();
    }

    let standalone = SharedGraph::builder(graph_id).build().unwrap();
    expect_existing_store(
        standalone.write_snapshot(config(&dir, 900)),
        ExistingStoreEvidence::PublishedManifest,
    );

    assert!(
        !snapshot_path(&dir, 900).exists(),
        "a refusal must not leave a snapshot behind"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A bare-header WAL — a WAL-backed graph that has not committed yet. Presence
/// must be the test: a guard keyed on "the WAL has entries" would admit exactly
/// this directory, and its header still declares the epoch whose sequence a
/// standalone write preclaims.
#[test]
fn write_snapshot_refuses_a_directory_with_a_bare_header_wal() {
    let dir = temp_dir("bare-wal");
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    drop(WalWriter::open(&path, WalConfig::default()).unwrap());

    let standalone = SharedGraph::builder(GraphId::new(82_102)).build().unwrap();
    expect_existing_store(
        standalone.write_snapshot(config(&dir, 900)),
        ExistingStoreEvidence::ActiveWal,
    );

    assert!(!snapshot_path(&dir, 900).exists());
    let _ = std::fs::remove_dir_all(dir);
}

/// The end-to-end regression for the downstream report. Before this guard the
/// stray snapshot made the directory unrecoverable: recovery picked the highest
/// on-disk sequence, found no MANIFEST to vouch for it, cross-checked it against
/// the WAL and hard-failed with `WalSnapshotMismatch`.
#[test]
fn write_snapshot_refusal_leaves_the_live_store_recoverable() {
    let dir = temp_dir("stays-recoverable");
    let graph_id = GraphId::new(82_103);
    {
        let shared = SharedGraph::builder(graph_id)
            .with_wal(dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default())
            .unwrap()
            .build()
            .unwrap();
        commit_node(&shared, "wsnap.live");

        expect_existing_store(
            shared.write_snapshot(config(&dir, 500)),
            ExistingStoreEvidence::ActiveWal,
        );
    }

    let recovered = SharedGraph::recover(&dir, graph_id).unwrap();
    assert_eq!(recovered.read().node_count(), 1);
    drop(recovered);
    let _ = std::fs::remove_dir_all(dir);
}

/// The guard must key on the store, not on emptiness: writing two standalone
/// snapshots into one export directory is a supported, tested flow.
#[test]
fn write_snapshot_still_publishes_beside_an_earlier_snapshot() {
    let dir = temp_dir("two-snapshots");
    let shared = SharedGraph::builder(GraphId::new(82_104)).build().unwrap();

    shared.write_snapshot(config(&dir, 90)).unwrap();
    shared.write_snapshot(config(&dir, 100)).unwrap();

    assert!(snapshot_path(&dir, 90).exists());
    assert!(snapshot_path(&dir, 100).exists());
    let _ = std::fs::remove_dir_all(dir);
}

const PIN_PROVIDER: [u8; 4] = *b"TST2";
const PIN_SUB: [u8; 4] = *b"BODY";
const PIN_SUB_TAGS: &[SubTag] = &[SubTag(PIN_SUB)];

/// Records which encode hook the loop used, and the generation it was handed.
#[derive(Default)]
struct HookRecordingProvider {
    unpinned_calls: std::sync::atomic::AtomicUsize,
    seen_generation: std::sync::atomic::AtomicU64,
}

impl IndexProvider for HookRecordingProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(PIN_PROVIDER)
    }

    fn read_section(&self, _sub_tag: SubTag, _bytes: &[u8]) -> Result<(), ProviderError> {
        Ok(())
    }

    fn write_section(&self, _sub_tag: SubTag) -> Result<Vec<u8>, ProviderError> {
        self.unpinned_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(b"unpinned".to_vec())
    }

    fn write_section_at_generation(
        &self,
        _sub_tag: SubTag,
        generation: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        self.seen_generation
            .store(generation, std::sync::atomic::Ordering::SeqCst);
        Ok(b"pinned".to_vec())
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        Ok(())
    }

    fn declared_sub_tags(&self) -> &[SubTag] {
        PIN_SUB_TAGS
    }
}

/// The loop must use the pinned hook and hand it the published generation.
/// Kills "left `write_section` in place" and "passed 0 or a stale generation".
#[test]
fn write_snapshot_encodes_through_the_pinned_hook() {
    use std::sync::atomic::Ordering::SeqCst;

    let dir = temp_dir("pinned-hook");
    let provider = Arc::new(HookRecordingProvider::default());
    let shared = SharedGraph::builder(GraphId::new(82_105))
        .with_provider(Arc::clone(&provider) as Arc<dyn IndexProvider>)
        .build()
        .unwrap();
    commit_node(&shared, "wsnap.pin");
    commit_node(&shared, "wsnap.pin");
    let expected = shared.read().meta.generation;
    assert!(expected > 0, "the fixture must advance the generation");

    shared.write_snapshot(config(&dir, 7)).unwrap();

    assert_eq!(
        provider.unpinned_calls.load(SeqCst),
        0,
        "the unpinned write_section hook must not be reached"
    );
    assert_eq!(provider.seen_generation.load(SeqCst), expected);
    let _ = std::fs::remove_dir_all(dir);
}

/// A commit landing mid-encode must fail the write, not tear the envelope.
///
/// The injector deliberately uses the DEFAULT `write_section_at_generation`,
/// which delegates to the unpinned hook and so is never generation-checked.
/// That is what makes this a test of the end-of-loop identity re-check rather
/// than of the per-section pin.
#[test]
fn write_snapshot_rejects_a_commit_published_mid_encode() {
    let dir = temp_dir("commit-mid-encode");
    let injector = Arc::new(CommitInjectingProvider {
        graph: std::sync::Mutex::new(None),
        injection: Injection::Commit,
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    // Registered after CORE so CORE's sections encode first, exactly as the
    // torn-envelope case would.
    let shared = Arc::new(
        SharedGraph::builder(GraphId::new(82_106))
            .with_provider(Arc::clone(&injector) as Arc<dyn IndexProvider>)
            .build()
            .unwrap(),
    );
    *injector.graph.lock().unwrap() = Some(Arc::downgrade(&shared));

    let error = shared
        .write_snapshot(config(&dir, 7))
        .expect_err("a commit published mid-encode must fail the snapshot");
    assert!(
        matches!(&error, GraphError::Inconsistent { reason } if reason.contains("republished")),
        "expected the republish refusal, got {error:?}"
    );
    assert!(!snapshot_path(&dir, 7).exists());
    let _ = std::fs::remove_dir_all(dir);
}

const INJECT_PROVIDER: [u8; 4] = *b"TST3";
const INJECT_SUB_TAGS: &[SubTag] = &[SubTag(*b"BODY")];

/// What a racing thread does to the graph while a section is being encoded.
#[derive(Clone, Copy)]
enum Injection {
    /// Publishes a new commit, which advances the generation.
    Commit,
    /// Publishes a compacted graph, which renumbers every row while copying
    /// `GraphMeta` — and therefore the generation — verbatim.
    Compact,
}

/// Republishes the graph from inside its own section encode, on another thread
/// so the committer is reachable, simulating a racing writer.
struct CommitInjectingProvider {
    graph: std::sync::Mutex<Option<std::sync::Weak<SharedGraph>>>,
    injection: Injection,
    fired: std::sync::atomic::AtomicBool,
}

impl IndexProvider for CommitInjectingProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(INJECT_PROVIDER)
    }

    fn read_section(&self, _sub_tag: SubTag, _bytes: &[u8]) -> Result<(), ProviderError> {
        Ok(())
    }

    fn write_section(&self, _sub_tag: SubTag) -> Result<Vec<u8>, ProviderError> {
        use std::sync::atomic::Ordering::SeqCst;

        if self.fired.swap(true, SeqCst) {
            return Ok(b"injected".to_vec());
        }
        let weak = self.graph.lock().unwrap().clone();
        if let Some(shared) = weak.and_then(|weak| weak.upgrade()) {
            let injection = self.injection;
            // Another thread: the encode runs under a FanoutGuard, and
            // begin_write refuses re-entry on the same thread.
            std::thread::scope(|scope| {
                scope.spawn(|| match injection {
                    Injection::Commit => commit_node(&shared, "wsnap.injected"),
                    Injection::Compact => {
                        shared.compact().unwrap();
                    }
                });
            });
        }
        Ok(b"injected".to_vec())
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        Ok(())
    }

    fn declared_sub_tags(&self) -> &[SubTag] {
        INJECT_SUB_TAGS
    }
}

/// The test that separates a pointer-identity check from a generation check.
///
/// `compact` republishes a graph whose every row is renumbered but whose
/// `GraphMeta` — including `generation` — is copied verbatim, so an end-of-loop
/// check written as `pinned.meta.generation == self.read().meta.generation`
/// passes across it and publishes an envelope whose CORE sections describe two
/// different row layouts. Only `Arc::ptr_eq` catches this.
#[test]
fn write_snapshot_rejects_a_compaction_published_mid_encode() {
    let dir = temp_dir("compact-mid-encode");
    let injector = Arc::new(CommitInjectingProvider {
        graph: std::sync::Mutex::new(None),
        injection: Injection::Compact,
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    let shared = Arc::new(
        SharedGraph::builder(GraphId::new(82_107))
            .with_provider(Arc::clone(&injector) as Arc<dyn IndexProvider>)
            .build()
            .unwrap(),
    );
    *injector.graph.lock().unwrap() = Some(Arc::downgrade(&shared));

    // Compaction only republishes when there is a dead row to reclaim.
    let victim = {
        let mut txn = shared.begin_write();
        let id = txn
            .mutator()
            .create_node(
                LabelSet::single(db_string("wsnap.compact").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        txn.commit().unwrap();
        id
    };
    let mut txn = shared.begin_write();
    txn.mutator().delete_node(victim).unwrap();
    txn.commit().unwrap();

    let generation_before = shared.read().meta.generation;

    let error = shared
        .write_snapshot(config(&dir, 7))
        .expect_err("a compaction published mid-encode must fail the snapshot");
    assert!(
        matches!(&error, GraphError::Inconsistent { reason } if reason.contains("republished")),
        "expected the republish refusal, got {error:?}"
    );
    assert_eq!(
        shared.read().meta.generation,
        generation_before,
        "compaction must leave the generation unchanged, or this test proves nothing"
    );
    assert!(!snapshot_path(&dir, 7).exists());
    let _ = std::fs::remove_dir_all(dir);
}
