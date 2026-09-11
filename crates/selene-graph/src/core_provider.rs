//! Core graph snapshot provider for persistence integration.

mod recovery_state;
mod sections;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use selene_core::{Change, HlcTimestamp, Origin};
use selene_persist::{
    AuditLog, AuditRecord, DEFAULT_WAL_FILE_NAME, RecoveryError, RecoveryProvider, RecoveryResult,
    SnapshotBuilder, WalRotationOutcome, WalWriter,
};

use crate::core_provider::recovery_state::RecoveryState;
use crate::core_provider::sections::{
    encode_composite_schemas, encode_edges, encode_graph_types, encode_meta, encode_nodes,
    encode_schemas, encode_text_schemas, encode_vector_schemas,
};
use crate::durable_provider::DurableProvider;
use crate::error::{GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::index_provider::{IndexProvider, ProviderError, ProviderTag, SubTag};

pub(crate) use recovery_state::logical_graph;

pub(crate) fn decode_schema_property(
    property: &selene_core::PropertyDef,
) -> Result<crate::PropertyTypeDef, ProviderError> {
    recovery_state::decode_schema_property(property)
}

pub(crate) fn decode_schema_edge_endpoint(
    graph_type: &crate::GraphTypeDef,
    endpoint: &selene_core::EdgeEndpointDef,
    role: &str,
) -> Result<crate::EdgeEndpointDef, ProviderError> {
    recovery_state::decode_schema_edge_endpoint(graph_type, endpoint, role)
}

/// Core graph provider tag used in snapshot section tables.
pub const CORE_PROVIDER_TAG: [u8; 4] = *b"CORE";
/// Core metadata subsection tag under [`CORE_PROVIDER_TAG`].
pub const CORE_META_SUB: [u8; 4] = *b"META";
/// Core graph-type subsection tag under [`CORE_PROVIDER_TAG`].
pub const CORE_GTYP_SUB: [u8; 4] = *b"GTYP";
/// Core node-column subsection tag under [`CORE_PROVIDER_TAG`].
pub const CORE_NODE_SUB: [u8; 4] = *b"NODE";
/// Core edge-column subsection tag under [`CORE_PROVIDER_TAG`].
pub const CORE_EDGE_SUB: [u8; 4] = *b"EDGE";
/// Core schema subsection tag under [`CORE_PROVIDER_TAG`].
pub const CORE_SCMA_SUB: [u8; 4] = *b"SCMA";
/// Core composite-property-index schema subsection tag under [`CORE_PROVIDER_TAG`].
pub const CORE_CPIX_SUB: [u8; 4] = *b"CPIX";
/// Core vector-index schema subsection tag under [`CORE_PROVIDER_TAG`].
pub const CORE_VIDX_SUB: [u8; 4] = *b"VIDX";
/// Core text-index schema subsection tag under [`CORE_PROVIDER_TAG`].
pub const CORE_TIDX_SUB: [u8; 4] = *b"TIDX";

const CORE_SUB_TAGS: &[SubTag] = &[
    SubTag(CORE_GTYP_SUB),
    SubTag(CORE_META_SUB),
    SubTag(CORE_NODE_SUB),
    SubTag(CORE_EDGE_SUB),
    SubTag(CORE_SCMA_SUB),
    SubTag(CORE_CPIX_SUB),
    SubTag(CORE_VIDX_SUB),
    SubTag(CORE_TIDX_SUB),
];

/// Shared provider implementation for live snapshots and recovery replay.
///
/// Live instances hold the exact [`ArcSwap`] used by [`crate::SharedGraph`].
/// Recovery instances accumulate snapshot sections and WAL changes until
/// [`CoreProvider::finish_recovery`] materializes a [`SeleneGraph`].
pub struct CoreProvider {
    inner: Mutex<CoreInner>,
}

/// WAL-owned target for one ordered graph checkpoint.
#[derive(Clone, Debug)]
pub(crate) struct CheckpointTarget {
    /// Directory that owns `wal.log`, snapshots, archives, and the MANIFEST.
    pub(crate) dir: PathBuf,
    /// Retained authority; `dir` is only the outcome locator.
    pub(crate) directory: selene_persist::StoreDirectory,
    /// Current durable WAL high-water sequence.
    pub(crate) sequence: u64,
}

enum CoreInner {
    Live {
        snapshot: Arc<ArcSwap<SeleneGraph>>,
        durable: Option<DurableState>,
    },
    Recovery {
        state: Box<RecoveryState>,
    },
}

/// Durable WAL state owned by a live [`CoreProvider`].
///
/// The HLC counter is seeded from [`WalWriter::last_sequence`]. On a fresh WAL
/// this is zero and the first physical commit or checkpoint-marker entry
/// receives `HlcTimestamp::new(1, 0)`; after reopening, the next timestamp
/// advances past the recovered WAL sequence.
pub struct DurableState {
    writer: Mutex<WalWriter>,
    next_hlc: AtomicU64,
    // Keep optional audit authority off the common WAL-only CoreInner layout.
    audit: Option<Box<Mutex<AuditLog>>>,
}

impl DurableState {
    /// Construct durable state from an already-open WAL writer.
    #[must_use]
    pub fn new(writer: WalWriter) -> Self {
        let last_sequence = writer.last_sequence();
        Self {
            writer: Mutex::new(writer),
            next_hlc: AtomicU64::new(last_sequence),
            audit: None,
        }
    }

    /// Attach an audit log so engine-owned events committed through this
    /// provider are mirrored to it (Item 7 / D24).
    ///
    /// Mirroring is **WAL-first, audit-after**: the WAL append is the source of
    /// truth and gates the commit; the audit write runs only after it succeeds
    /// and is best-effort (a failure is logged, never failing the commit). The
    /// event also remains in the WAL, so a failed mirror degrades to the
    /// pre-Item-7 WAL-only behavior rather than losing the event. Per the donor
    /// lesson "audit lag is recoverable, fiction is not," the audit can only lag
    /// the WAL, never lead it.
    /// # Errors
    /// Rejects an audit component owned by another physical store directory.
    pub fn with_audit_log(mut self, audit: AuditLog) -> GraphResult<Self> {
        if !self
            .writer
            .get_mut()
            .directory()
            .same_directory(audit.authority().directory())?
        {
            return Err(GraphError::Inconsistent {
                reason: "WAL and audit must share store writer authority".into(),
            });
        }
        self.audit = Some(Box::new(Mutex::new(audit)));
        Ok(self)
    }

    /// Append one engine-owned event to the attached audit log, if any.
    ///
    /// The D24 audit log is the durable "events" surface in the
    /// snapshot=state / WAL=changes / audit=events split, with retention
    /// independent of the WAL lineage. Appends are **best-effort and audit-after**:
    /// the caller stamps the wall clock here, the record is serialized by the
    /// caller into an opaque `kind`-tagged payload, and an append failure is
    /// logged and skipped rather than propagated, so the audit can only lag a
    /// committed change, never lead it (the donor lesson "audit lag is
    /// recoverable, fiction is not"). Returns `false` when no audit log is
    /// attached or the append failed.
    ///
    /// The pack-lifecycle producer that previously fed this surface was removed
    /// in the extension teardown; the framework remains wired (persisted,
    /// reattached on recovery) for future user-action audit events (D24).
    pub fn append_audit_event(&self, kind: u16, payload: Vec<u8>) -> bool {
        let Some(audit) = &self.audit else {
            return false;
        };
        let record = AuditRecord {
            recorded_at_unix_nanos: unix_nanos_now(),
            kind,
            payload,
        };
        let mut log = audit.lock();
        match log.append(&record) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(%error, "audit: failed to append engine event");
                false
            }
        }
    }
}

/// Current wall-clock time as nanoseconds since the Unix epoch, saturating.
fn unix_nanos_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl CoreProvider {
    /// Construct a live provider bound to a shared graph snapshot pointer.
    #[must_use]
    pub fn new_for_live(snapshot: Arc<ArcSwap<SeleneGraph>>) -> Arc<Self> {
        Self::new_for_live_with_wal(snapshot, None)
    }

    /// Construct a live provider with optional commit-critical WAL state.
    #[must_use]
    pub fn new_for_live_with_wal(
        snapshot: Arc<ArcSwap<SeleneGraph>>,
        durable: Option<DurableState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(CoreInner::Live { snapshot, durable }),
        })
    }

    /// Construct a recovery-mode provider with an empty accumulator.
    #[must_use]
    pub fn new_for_recovery() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(CoreInner::Recovery {
                state: Box::new(RecoveryState::new()),
            }),
        })
    }

    /// Drain the recovery accumulator into a graph snapshot.
    ///
    /// `expected_graph_id` is the caller-asserted graph identity, and recovery
    /// fails if anything on disk disagrees with it. A snapshot declares its
    /// identity in `CORE/META`. A WAL declares its identity in the graph id
    /// stamped on every replayed schema record, which is all a WAL-only
    /// directory has; a WAL carrying no schema record declares no identity, so
    /// there the caller's assertion is taken on trust and used with default
    /// scalar metadata fields.
    ///
    /// # Errors
    ///
    /// Returns [`crate::GraphError::Provider`] if this provider was
    /// constructed for live mode, if META or a replayed schema record
    /// disagrees with `expected_graph_id`, or if the accumulated
    /// section/changelog state cannot be materialized into graph columns.
    pub fn finish_recovery(
        self: Arc<Self>,
        expected_graph_id: selene_core::GraphId,
        expected_bound_type: Option<Arc<crate::graph_types::GraphTypeDef>>,
    ) -> GraphResult<SeleneGraph> {
        let mut inner = self.inner.lock();
        match &mut *inner {
            CoreInner::Live { .. } => {
                Err(inconsistent("finish_recovery called on live-mode CoreProvider").into())
            }
            CoreInner::Recovery { state } => {
                let state = std::mem::take(&mut **state);
                state.into_graph(expected_graph_id, expected_bound_type)
            }
        }
    }

    /// Resolve the target owned by this provider's live conventional WAL.
    ///
    /// # Errors
    ///
    /// Rejects recovery-mode providers, live providers without a WAL, and WAL
    /// paths whose filename is not `wal.log`. Sequence zero is a valid base:
    /// the coordinated checkpoint watermark advances it before rotation.
    pub(crate) fn checkpoint_target(&self) -> GraphResult<CheckpointTarget> {
        let inner = self.inner.lock();
        match &*inner {
            CoreInner::Live {
                durable: Some(durable),
                ..
            } => checkpoint_target_for_writer(&durable.writer.lock()),
            CoreInner::Live { durable: None, .. } => Err(checkpoint_unavailable(
                "ordered checkpoint requires a graph opened with a WAL",
            )),
            CoreInner::Recovery { .. } => Err(checkpoint_unavailable(
                "ordered checkpoint is unavailable on a recovery-mode CORE provider",
            )),
        }
    }

    /// Append a physical checkpoint watermark and rotate a populated snapshot.
    ///
    /// The writer target is revalidated under its mutex immediately before the
    /// crash-safe MANIFEST rotation. The builder sequence must be exactly one
    /// greater than the writer high-water mark. The marker consumes the CORE
    /// HLC counter so later commit timestamps remain monotonic.
    ///
    /// # Errors
    ///
    /// Returns the same availability errors as [`Self::checkpoint_target`] and
    /// propagates snapshot, archive, MANIFEST, and active-WAL reset failures.
    pub(crate) fn rotate_checkpoint_with_watermark(
        &self,
        builder: SnapshotBuilder,
    ) -> GraphResult<WalRotationOutcome> {
        let mut inner = self.inner.lock();
        match &mut *inner {
            CoreInner::Live {
                durable: Some(durable),
                ..
            } => {
                let seconds = durable
                    .next_hlc
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                let mut writer = durable.writer.lock();
                writer
                    .rotate_with_checkpoint_watermark(builder, HlcTimestamp::new(seconds, 0))
                    .map_err(Into::into)
            }
            CoreInner::Live { durable: None, .. } => Err(checkpoint_unavailable(
                "ordered checkpoint requires a graph opened with a WAL",
            )),
            CoreInner::Recovery { .. } => Err(checkpoint_unavailable(
                "ordered checkpoint is unavailable on a recovery-mode CORE provider",
            )),
        }
    }

    fn read_section_inner(&self, sub_tag: SubTag, bytes: &[u8]) -> Result<(), ProviderError> {
        let mut inner = self.inner.lock();
        match &mut *inner {
            CoreInner::Live { .. } => Err(inconsistent(
                "read_section called on live-mode CoreProvider",
            )),
            CoreInner::Recovery { state } => state.read_section(sub_tag, bytes),
        }
    }

    fn live_snapshot(&self, operation: &'static str) -> Result<Arc<SeleneGraph>, ProviderError> {
        // The snapshot is an `Arc<ArcSwap<SeleneGraph>>`: `load_full()` clones the
        // Arc lock-free. Hold `inner` ONLY long enough to grab that Arc, then drop
        // the guard so the (potentially ≤1 GiB) rkyv encode runs lock-free and
        // never blocks the commit hot path that shares this Mutex.
        let inner = self.inner.lock();
        match &*inner {
            CoreInner::Live { snapshot, .. } => Ok(snapshot.load_full()),
            CoreInner::Recovery { .. } => Err(inconsistent(format!(
                "{operation} called on recovery-mode CoreProvider"
            ))),
        }
    }

    fn write_section_inner(&self, sub_tag: SubTag) -> Result<Vec<u8>, ProviderError> {
        let graph = self.live_snapshot("write_section")?;
        encode_section(&graph, sub_tag)
    }

    fn write_section_at_generation_inner(
        &self,
        sub_tag: SubTag,
        generation: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        let graph = self.live_snapshot("write_section_at_generation")?;
        if graph.meta.generation != generation {
            return Err(inconsistent(format!(
                "CORE snapshot generation {} does not match checkpoint generation {generation}",
                graph.meta.generation
            )));
        }
        encode_section(&graph, sub_tag)
    }

    fn on_change_inner(&self, change: &Change) -> Result<(), ProviderError> {
        let mut inner = self.inner.lock();
        match &mut *inner {
            CoreInner::Live { .. } => Ok(()),
            CoreInner::Recovery { state } => state.apply_change(change),
        }
    }
}

fn encode_section(graph: &SeleneGraph, sub_tag: SubTag) -> Result<Vec<u8>, ProviderError> {
    match sub_tag.0 {
        CORE_GTYP_SUB => encode_graph_types(graph),
        CORE_META_SUB => encode_meta(&graph.meta, graph.meta.generation),
        CORE_NODE_SUB => encode_nodes(graph),
        CORE_EDGE_SUB => encode_edges(graph),
        CORE_SCMA_SUB => encode_schemas(graph),
        CORE_CPIX_SUB => encode_composite_schemas(graph),
        CORE_VIDX_SUB => encode_vector_schemas(graph),
        CORE_TIDX_SUB => encode_text_schemas(graph),
        _ => Err(invalid_sub_tag(sub_tag)),
    }
}

fn checkpoint_target_for_writer(writer: &WalWriter) -> GraphResult<CheckpointTarget> {
    if writer.path().file_name() != Some(OsStr::new(DEFAULT_WAL_FILE_NAME)) {
        return Err(checkpoint_unavailable(format!(
            "ordered checkpoint requires the active WAL filename {DEFAULT_WAL_FILE_NAME:?}, observed {:?}",
            writer.path()
        )));
    }
    let sequence = writer.last_sequence();
    let directory = writer.directory().clone();
    let dir = directory.locator().to_path_buf();
    Ok(CheckpointTarget {
        dir,
        directory,
        sequence,
    })
}

fn checkpoint_unavailable(reason: impl Into<String>) -> GraphError {
    GraphError::Inconsistent {
        reason: reason.into(),
    }
}

impl IndexProvider for CoreProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(CORE_PROVIDER_TAG)
    }

    fn read_section(&self, sub_tag: SubTag, bytes: &[u8]) -> Result<(), ProviderError> {
        self.read_section_inner(sub_tag, bytes)
    }

    fn write_section(&self, sub_tag: SubTag) -> Result<Vec<u8>, ProviderError> {
        self.write_section_inner(sub_tag)
    }

    fn write_section_at_generation(
        &self,
        sub_tag: SubTag,
        generation: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        self.write_section_at_generation_inner(sub_tag, generation)
    }

    fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
        self.on_change_inner(change)
    }

    fn declared_sub_tags(&self) -> &[SubTag] {
        CORE_SUB_TAGS
    }
}

impl DurableProvider for CoreProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(CORE_PROVIDER_TAG)
    }

    fn next_timestamp(&self) -> HlcTimestamp {
        let inner = self.inner.lock();
        match &*inner {
            CoreInner::Live {
                durable: Some(durable),
                ..
            } => {
                let seconds = durable
                    .next_hlc
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                HlcTimestamp::new(seconds, 0)
            }
            CoreInner::Live { durable: None, .. } | CoreInner::Recovery { .. } => {
                HlcTimestamp::zero()
            }
        }
    }

    fn write_commit(
        &self,
        principal: Option<&Arc<[u8]>>,
        changes: &[Change],
        timestamp: HlcTimestamp,
    ) -> Result<u64, ProviderError> {
        let mut inner = self.inner.lock();
        match &mut *inner {
            CoreInner::Live {
                durable: Some(durable),
                ..
            } => {
                // Cheap refcount bump — the caller already owns the bytes as an
                // `Arc<[u8]>`; no slice re-allocation or copy.
                let principal = principal.cloned();
                // WAL-first: the append gates the commit (its error fails it).
                let sequence = {
                    let mut writer = durable.writer.lock();
                    writer
                        .append(timestamp, Origin::Local, principal, changes)
                        .map_err(durable_error)?
                };
                Ok(sequence)
            }
            CoreInner::Live { durable: None, .. } => Ok(0),
            CoreInner::Recovery { .. } => Err(inconsistent(
                "write_commit called on recovery-mode CoreProvider",
            )),
        }
    }

    fn flush(&self) -> Result<Option<u64>, ProviderError> {
        let mut inner = self.inner.lock();
        match &mut *inner {
            CoreInner::Live {
                durable: Some(durable),
                ..
            } => {
                let mut writer = durable.writer.lock();
                writer.flush().map_err(durable_error)?;
                Ok(Some(writer.last_sequence()))
            }
            CoreInner::Live { durable: None, .. } | CoreInner::Recovery { .. } => Ok(None),
        }
    }
}

impl RecoveryProvider for CoreProvider {
    fn provider_tag(&self) -> [u8; 4] {
        CORE_PROVIDER_TAG
    }

    fn read_section(&self, sub: [u8; 4], bytes: &[u8]) -> RecoveryResult<()> {
        self.read_section_inner(SubTag(sub), bytes)
            .map_err(box_provider_error)
    }

    // `on_changes` is the sole entry point WAL replay invokes; the per-change
    // `RecoveryProvider::on_change` default is unused for this provider, so it is
    // deliberately not overridden.
    fn on_changes(&self, changes: &[Change]) -> RecoveryResult<()> {
        for change in changes {
            self.on_change_inner(change).map_err(box_provider_error)?;
        }
        Ok(())
    }
}

pub(crate) fn invalid_payload(reason: impl Into<String>) -> ProviderError {
    ProviderError::InvalidPayload {
        reason: reason.into(),
    }
}

fn durable_error(error: impl std::error::Error) -> ProviderError {
    ProviderError::SerializationFailed {
        reason: error.to_string(),
    }
}

pub(crate) fn serialization_failed(reason: impl Into<String>) -> ProviderError {
    ProviderError::SerializationFailed {
        reason: reason.into(),
    }
}

pub(crate) fn inconsistent(reason: impl Into<String>) -> ProviderError {
    ProviderError::Inconsistent {
        reason: reason.into(),
    }
}

fn invalid_sub_tag(sub_tag: SubTag) -> ProviderError {
    invalid_payload(format!("unknown CORE sub-tag {sub_tag}"))
}

fn box_provider_error(error: ProviderError) -> RecoveryError {
    Box::new(error)
}

#[cfg(test)]
#[path = "core_provider/tests.rs"]
mod tests;
