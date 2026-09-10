//! Snapshot writing facade for [`SharedGraph`](crate::SharedGraph).

use std::sync::Arc;

use selene_persist::{
    DEFAULT_WAL_FILE_NAME, MANIFEST_FILE_NAME, SnapshotBuilder, SnapshotConfig,
    SnapshotFinalizeOutcome,
};

use crate::error::ExistingStoreEvidence;
use crate::{GraphError, GraphResult, SharedGraph};

/// Refuse to write a standalone snapshot into a directory the engine manages.
///
/// A standalone snapshot is not part of any epoch: it publishes no MANIFEST and
/// rotates no WAL. Dropped into a managed directory it is at best ignored and
/// pruned, and at worst bricks recovery — a `snapshot.N.snap` with no MANIFEST
/// makes recovery cross-check it against the WAL and hard-fail with
/// `WalSnapshotMismatch`, which is the failure a downstream consumer reported.
///
/// Presence is the test, not content. A bare-header `wal.log` — a WAL-backed
/// graph that has not committed yet — declares an epoch whose sequence a
/// standalone write can still preclaim, so "has entries" would be too weak.
///
/// Known limit: this sees the conventional layout only. A graph whose WAL has a
/// non-default filename is already outside the managed contract — `recover`
/// looks for `wal.log` (`recover.rs`) and `checkpoint` refuses any other name
/// (`core_provider.rs`) — so such a directory cannot be recovered or
/// checkpointed either way.
fn reject_managed_directory(dir: &selene_persist::StoreDirectory) -> GraphResult<()> {
    for (name, evidence) in [
        (MANIFEST_FILE_NAME, ExistingStoreEvidence::PublishedManifest),
        (DEFAULT_WAL_FILE_NAME, ExistingStoreEvidence::ActiveWal),
    ] {
        let path = dir.locator().join(name);
        if dir.contains(name)? {
            return Err(GraphError::ExistingStore { path, evidence });
        }
    }
    Ok(())
}

impl SharedGraph {
    /// Write one standalone snapshot containing every registered provider
    /// section.
    ///
    /// This is the graph-layer facade over [`SnapshotBuilder`]. It walks the
    /// fixed provider registry, asks each provider to encode every declared
    /// subsection at one pinned generation, and finalizes the snapshot envelope
    /// from `config`.
    ///
    /// This lower-level envelope writer is not coordinated with the graph
    /// committer or owned WAL, and it publishes no MANIFEST, so the snapshot it
    /// writes is not a recoverable epoch on its own. WAL-backed callers that
    /// need one should use [`SharedGraph::checkpoint`](crate::SharedGraph::checkpoint),
    /// which pins provider generation, snapshot sequence, durability, MANIFEST
    /// commit, and WAL rotation as a single committer work item.
    ///
    /// # The caller must exclude writes, and the target must be unmanaged
    ///
    /// Both halves are now enforced rather than merely documented:
    ///
    /// - A directory already holding a `MANIFEST` or a `wal.log` is refused, so
    ///   a standalone snapshot can no longer be dropped into a live store.
    /// - The section loop encodes at one pinned generation and re-checks that
    ///   the published graph was not replaced, so a commit, compaction, or
    ///   vector-index rebuild landing mid-encode fails loudly instead of
    ///   producing a snapshot torn across sections.
    ///
    /// The second check is an error, not a wait: this call does not quiesce
    /// anything, so a caller racing its own writers must serialize them and
    /// retry.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::ExistingStore`] when the target directory already
    /// holds a store, [`GraphError::Inconsistent`] when the graph was
    /// republished while encoding, [`GraphError::Provider`] when a provider
    /// cannot encode one of its declared sections at the pinned generation, and
    /// [`GraphError::Persist`] when the snapshot envelope cannot be finalized.
    ///
    /// # Panics
    ///
    /// Panics when called from inside a provider callback on the committer
    /// thread, matching every other maintenance entry point.
    pub fn write_snapshot(&self, config: SnapshotConfig) -> GraphResult<SnapshotFinalizeOutcome> {
        crate::shared::reject_provider_callback_reentry("SharedGraph::write_snapshot()");
        let directory = selene_persist::StoreDirectory::open(&config.dir)?;
        reject_managed_directory(&directory)?;
        let writer = selene_persist::StoreWriter::acquire(&directory)?;
        // Close the check/acquire gap with a fresh check under owned authority.
        // Keep that ownership through provider encoding and final publication.
        reject_managed_directory(&directory)?;

        // Pin the *published* graph, never the write-locked one: `seal` bumps
        // the generation under the write lock before the committer publishes,
        // so a write-lock pin would disagree with what the encoders load.
        let pinned = self.read();
        let generation = pinned.meta.generation;

        let mut builder = SnapshotBuilder::new_in(&directory, config);
        {
            let _fanout = crate::reentry::FanoutGuard::enter();
            for provider in self.index_providers() {
                let provider_tag = provider.provider_tag();
                for sub_tag in provider.declared_sub_tags() {
                    let bytes = provider.write_section_at_generation(*sub_tag, generation)?;
                    builder.add_section(provider_tag.0, sub_tag.0, bytes)?;
                }
            }
        }

        // The per-section pin is necessary but not sufficient, for two reasons.
        // `compact` and the vector-index rebuilds republish a graph with
        // `GraphMeta` — and therefore `generation` — copied verbatim while every
        // row is renumbered, so generation equality holds across two different
        // layouts. And `write_section_at_generation` has a default impl that
        // delegates to the unpinned hook, so a provider that does not override
        // it is never checked. Pointer identity of the published graph catches
        // both, because provider state only advances behind a publish.
        if !Arc::ptr_eq(&pinned, &self.read()) {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "standalone snapshot observed a republished graph while encoding provider \
                     sections (pinned generation {generation}); SharedGraph::write_snapshot \
                     requires the caller to exclude writes, compaction, and vector-index \
                     rebuilds for its duration"
                ),
            });
        }
        builder.finalize_with_authority(&writer).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use selene_core::{Change, GraphId};
    use selene_persist::{SectionCompression, SnapshotConfig, SnapshotReader, snapshot_path};

    use crate::SharedGraph;
    use crate::index_provider::{IndexProvider, ProviderError, ProviderTag, SubTag};

    const TEST_PROVIDER: [u8; 4] = *b"TST1";
    const TEST_SUB: [u8; 4] = *b"BODY";
    const TEST_SUB_TAGS: &[SubTag] = &[SubTag(TEST_SUB)];

    struct SnapshotOnlyProvider;

    impl IndexProvider for SnapshotOnlyProvider {
        fn provider_tag(&self) -> ProviderTag {
            ProviderTag(TEST_PROVIDER)
        }

        fn read_section(&self, _sub_tag: SubTag, _bytes: &[u8]) -> Result<(), ProviderError> {
            Ok(())
        }

        fn write_section(&self, sub_tag: SubTag) -> Result<Vec<u8>, ProviderError> {
            assert_eq!(sub_tag, SubTag(TEST_SUB));
            Ok(b"provider-body".to_vec())
        }

        fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
            Ok(())
        }

        fn declared_sub_tags(&self) -> &[SubTag] {
            TEST_SUB_TAGS
        }
    }

    #[test]
    fn write_snapshot_includes_registered_provider_sections() {
        let dir = temp_dir("shared-snapshot");
        let provider = Arc::new(SnapshotOnlyProvider);
        let shared = SharedGraph::builder(GraphId::new(82_001))
            .with_provider(provider as Arc<dyn IndexProvider>)
            .build()
            .unwrap();

        let outcome = shared
            .write_snapshot(SnapshotConfig {
                dir: dir.clone(),
                sequence: 7,
                compression: SectionCompression::None,
                fsync: false,
            })
            .unwrap();

        assert_eq!(outcome.snapshot_seq, 7);
        assert!(outcome.section_count > 1);
        let mut reader = SnapshotReader::open(&snapshot_path(&dir, 7)).unwrap();
        assert_eq!(
            reader.read_section(TEST_PROVIDER, TEST_SUB).unwrap(),
            b"provider-body"
        );
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "selene-graph-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
#[path = "shared_snapshot/guard_tests.rs"]
mod guard_tests;
