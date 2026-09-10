//! Shared graph construction helpers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use selene_core::GraphId;
use selene_persist::{
    AuditLog, MANIFEST_FILE_NAME, StoreDirectory, SyncPolicy, WAL_FILE_HEADER_LEN, WalConfig,
    WalWriter, find_latest_snapshot_in,
};

use super::SharedGraph;
use crate::committer_batch::CommitBatching;
use crate::error::{ExistingStoreEvidence, GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::graph_types::GraphTypeDef;
use crate::index_provider::IndexProvider;

/// Open `path` as the WAL of a directory that does not yet hold a store.
///
/// Every path that attaches a WAL to a graph the engine did *not* just recover
/// goes through here, so the two cannot drift apart: the builder, and
/// [`SharedGraph::from_graph_with_wal`]. [`SharedGraph::recover`] deliberately
/// does not — it opens its writer only after replaying the store, which is the
/// whole point.
///
/// Three pieces of evidence, because no one of them is sufficient:
///
/// - **WAL entries.** `committed_offset` is seeded to the header length and
///   advances past it only once a valid entry has been scanned, so this is
///   exactly "the log already carries commits".
/// - **A published `MANIFEST` beside it.** Checkpoint rotation archives the
///   entries and resets the active WAL to a bare header, so a checkpointed
///   directory has an *empty-looking* WAL while its dataset sits in a snapshot.
///   Judging by the file alone would bless the state every long-lived directory
///   converges to. The header watermark cannot stand in for this: a brand-new
///   WAL created with a non-zero `snapshot_seq` has the identical header.
/// - **A `snapshot.N.snap` with no `MANIFEST`.** This is what
///   [`SharedGraph::write_snapshot`] exports, and recovery treats it as a
///   store: with no `MANIFEST` to consult it applies the highest on-disk
///   snapshot and seeds a fresh WAL header from that sequence. Attaching an
///   unrelated WAL beside one therefore writes a `snapshot_seq: 0` header that
///   recovery later cross-checks against the snapshot it just applied, and
///   refuses. Recovering the export directory is the operation that gets this
///   right.
///
/// Refusing drops the writer, which releases its lock and leaves the directory
/// exactly as it was found, ready for a recover call.
///
/// # Errors
///
/// Returns [`GraphError::Persist`] when the WAL cannot be opened, and
/// [`GraphError::ExistingStore`] when the directory already holds a store.
pub(super) fn open_fresh_wal(path: &Path, mut config: WalConfig) -> GraphResult<WalWriter> {
    // BRIEF 2: the committer owns fsync. Force OnFlushOnly before opening so the
    // committer's group flush is the single durability barrier. Done before
    // WalWriter::open so open-error timing (e.g. WriterLockHeld) is unchanged
    // for existing .unwrap() call sites.
    config.sync_policy = SyncPolicy::OnFlushOnly;
    let writer = WalWriter::open(path, config)?;
    validate_fresh_wal(writer)
}

fn validate_fresh_wal(writer: WalWriter) -> GraphResult<WalWriter> {
    let refuse = |evidence| {
        Err(GraphError::ExistingStore {
            path: writer.path().to_path_buf(),
            evidence,
        })
    };
    if writer.committed_offset() > WAL_FILE_HEADER_LEN as u64 {
        return refuse(ExistingStoreEvidence::WalEntries);
    }
    // `parent` is empty for a bare relative filename, which resolves against the
    // cwd exactly as the WAL path itself did.
    let dir = writer.directory();
    if dir.contains(MANIFEST_FILE_NAME)? {
        return refuse(ExistingStoreEvidence::PublishedManifest);
    }
    // Ask the same question recovery asks, with the same helper, so the two
    // cannot disagree about whether this directory already holds a dataset.
    if find_latest_snapshot_in(dir)?.is_some() {
        return refuse(ExistingStoreEvidence::StandaloneSnapshot);
    }
    Ok(writer)
}

/// Builder for a [`SharedGraph`] and its fixed provider registry.
pub struct SharedGraphBuilder {
    graph: SeleneGraph,
    providers: Vec<Arc<dyn IndexProvider>>,
    wal_writer: Option<WalWriter>,
    audit_log: Option<AuditLog>,
    input_dir: Option<PathBuf>,
    commit_batching: CommitBatching,
}

impl SharedGraphBuilder {
    /// Construct a builder for an empty graph.
    pub(super) fn new(graph_id: GraphId) -> Self {
        Self {
            graph: SeleneGraph::new(graph_id),
            providers: Vec::new(),
            wal_writer: None,
            audit_log: None,
            input_dir: None,
            commit_batching: CommitBatching::Off,
        }
    }

    /// Register an index provider.
    ///
    /// Providers are retained in registration order, which is the order used
    /// for committed mutation delivery.
    #[must_use]
    pub fn with_provider(mut self, provider: Arc<dyn IndexProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// Open a WAL file and route commits through the CORE durable provider.
    ///
    /// The path is the WAL file path, not a directory. Callers using the
    /// conventional layout should pass `dir.join(selene_persist::DEFAULT_WAL_FILE_NAME)`.
    ///
    /// # SyncPolicy is OVERRIDDEN (v1.2 BRIEF 2 — read this)
    ///
    /// The single per-graph committer thread is the **sole fsync caller** for the
    /// committer-managed WAL: it appends a contiguous run of commits with fsync
    /// deferred, then issues exactly one [`WalWriter::flush`] per run (the R1
    /// fsync-before-publish barrier). To make that the *only* fsync path, this
    /// method **forces `config.sync_policy` to [`SyncPolicy::OnFlushOnly`]**
    /// before opening the WAL — **whatever policy you pass is discarded.** The
    /// fsync cadence is instead controlled by [`Self::with_commit_batching`]:
    /// [`CommitBatching::Off`] (the default) fsyncs once per commit (behaviorally
    /// identical to the old `EveryN(1)`), and [`CommitBatching::On`] coalesces a
    /// contiguous run into one fsync. `config.snapshot_seq` is passed through
    /// verbatim. Durability is unchanged: the committer always flushes before it
    /// publishes or acks, so a commit is durable before it is ever visible.
    ///
    /// # The directory must not already hold a store
    ///
    /// A builder starts from an *empty* graph, and [`WalWriter::open`] positions
    /// an existing WAL for **append without replaying it**. Building over a
    /// directory that already holds data therefore layers a second dataset's
    /// commits onto the first: node ids restart at 1 and collide with ids the
    /// store already allocated, so the directory stops recovering at all
    /// (`D11`) and the data that was there is unreachable. Recovering is the
    /// operation that reads an existing store — [`SharedGraph::recover`] and its
    /// variants — so an existing store is refused here rather than silently
    /// corrupted. Committed WAL entries and a `MANIFEST` naming a published
    /// snapshot each count as existing; see `open_fresh_wal` for why both are
    /// needed.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Persist`] when the WAL cannot be opened, including
    /// when another writer already holds the file lock, and
    /// [`GraphError::ExistingStore`] when the directory already holds a store.
    pub fn with_wal(mut self, path: impl AsRef<Path>, config: WalConfig) -> GraphResult<Self> {
        let path = path.as_ref();
        let writer = if let Some(audit) = &self.audit_log {
            let name = composed_child(
                audit.authority().directory(),
                self.input_dir.as_deref(),
                path,
            )?;
            let mut config = config;
            config.sync_policy = SyncPolicy::OnFlushOnly;
            validate_fresh_wal(WalWriter::open_with_authority(
                audit.authority(),
                &name,
                config,
            )?)?
        } else {
            open_fresh_wal(path, config)?
        };
        self.input_dir
            .get_or_insert_with(|| path.parent().unwrap_or_else(|| Path::new("")).into());
        self.wal_writer = Some(writer);
        Ok(self)
    }

    /// Attach a fresh WAL using a caller-retained directory capability.
    ///
    /// # Errors
    /// Returns locking, existing-store, directory, or WAL validation errors.
    pub fn with_wal_in(
        mut self,
        dir: &StoreDirectory,
        name: &Path,
        mut config: WalConfig,
    ) -> GraphResult<Self> {
        config.sync_policy = SyncPolicy::OnFlushOnly;
        let writer = if let Some(audit) = &self.audit_log {
            if !dir.same_directory(audit.authority().directory())? {
                return Err(GraphError::Inconsistent {
                    reason: "WAL and audit require the same store directory".into(),
                });
            }
            WalWriter::open_with_authority(audit.authority(), name, config)?
        } else {
            WalWriter::open_in(dir, name, config)?
        };
        self.wal_writer = Some(validate_fresh_wal(writer)?);
        Ok(self)
    }

    /// Set the group-commit batching policy for the committer-managed WAL
    /// (v1.2 BRIEF 2). Default [`CommitBatching::Off`].
    ///
    /// With [`CommitBatching::Off`] the committer fsyncs once per commit
    /// (behaviorally identical to BRIEF 1). With [`CommitBatching::On`] it
    /// coalesces up to `max_commits` (capped by aggregate `max_bytes`) contiguous
    /// commits into one fsync — higher throughput + lower tail latency under
    /// fan-in, at the cost of grouping several commits behind one barrier (all
    /// still durable before any of them is acked or published). Has no effect
    /// without [`Self::with_wal`] (no durable provider to flush).
    #[must_use]
    pub fn with_commit_batching(mut self, batching: CommitBatching) -> Self {
        self.commit_batching = batching;
        self
    }

    /// Attach a durable audit log at `path` (conventionally
    /// `dir.join(selene_persist::DEFAULT_AUDIT_FILE_NAME)`).
    ///
    /// Engine-owned audit events committed through this graph are mirrored to
    /// the audit log so they survive WAL-archive pruning (Item 7 / Seam D, D24).
    /// Requires [`Self::with_wal`]: audit mirroring is part of the durable
    /// commit path, so [`Self::build`] errors if an audit log is configured
    /// without a WAL.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Persist`] when the audit log cannot be opened.
    pub fn with_audit_log(mut self, path: impl AsRef<Path>) -> GraphResult<Self> {
        let path = path.as_ref();
        self.audit_log = Some(if let Some(writer) = &self.wal_writer {
            let name = composed_child(writer.directory(), self.input_dir.as_deref(), path)?;
            AuditLog::open_with_authority(writer.authority(), &name)?
        } else {
            AuditLog::open(path)?
        });
        self.input_dir
            .get_or_insert_with(|| path.parent().unwrap_or_else(|| Path::new("")).into());
        Ok(self)
    }

    /// Bind this graph to `type_def` at construction time.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when the builder is already bound
    /// or when `type_def` fails self-consistency validation.
    pub fn bound_to(mut self, type_def: GraphTypeDef) -> GraphResult<Self> {
        if self.graph.meta.bound_type.is_some() {
            return Err(GraphError::Inconsistent {
                reason: "graph builder is already bound to a graph type".to_owned(),
            });
        }
        self.graph.meta.bound_type = Some(Arc::new(type_def.validate()?));
        Ok(self)
    }

    /// Build shared graph state and validate provider registration.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Provider`] when provider tags are duplicated.
    pub fn build(self) -> GraphResult<SharedGraph> {
        SharedGraph::from_graph_with_core_and_durables(
            self.graph,
            self.providers,
            Vec::new(),
            self.wal_writer,
            self.audit_log,
            self.commit_batching,
        )
    }
}

// A repeated input locator selects the already-owned authority, even if the
// caller alias or real parent was renamed. A different input is anchored once
// only to validate that it refers to the same store; never used for child I/O.
fn composed_child(dir: &StoreDirectory, input: Option<&Path>, path: &Path) -> GraphResult<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let name = path.file_name().ok_or_else(|| GraphError::Inconsistent {
        reason: "persistence path must name a child file".into(),
    })?;
    if input != Some(parent)
        && dir.locator() != parent
        && !dir.same_directory(&StoreDirectory::open(parent)?)?
    {
        return Err(GraphError::Inconsistent {
            reason: "WAL and audit require the same store directory".into(),
        });
    }
    Ok(name.into())
}
