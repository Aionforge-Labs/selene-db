//! Graph-layer error types and GQLSTATUS mappings.

use selene_core::{CoreError, DbString, EdgeId, GraphId, NodeId};
use selene_persist::PersistError;
use smallvec::SmallVec;

use crate::index_provider::ProviderError;
use crate::type_validator::TypeViolation;
use crate::typed_index::TypedIndexKind;

/// Result alias for graph operations.
pub type GraphResult<T> = Result<T, GraphError>;

/// Result alias for snapshot-bound candidate-set validation and algebra.
pub type CandidateSetResult<T> = Result<T, CandidateSetError>;

/// Validation error raised by graph-owned candidate-set algebra.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum CandidateSetError {
    /// A candidate set belongs to a different lower graph identity.
    #[error("candidate set belongs to graph {actual}, expected graph {expected}")]
    GraphMismatch {
        /// Graph identity required by the graph performing the algebra.
        expected: GraphId,
        /// Graph identity carried by the candidate set.
        actual: GraphId,
    },
    /// A candidate set was produced from a different immutable generation.
    #[error("candidate set belongs to generation {actual}, expected generation {expected}")]
    GenerationMismatch {
        /// Generation required by the graph performing the algebra.
        expected: u64,
        /// Generation carried by the candidate set.
        actual: u64,
    },
    /// A candidate set has a different physical layout or workspace binding.
    #[error("candidate set belongs to a different snapshot layout")]
    LayoutMismatch,
    /// A candidate's stable ID, typed row, or liveness no longer matches.
    #[error("candidate set contains an entry that is stale for this snapshot")]
    StaleEntry,
}

/// Store-assignment data-exception family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StoreAssignmentException {
    /// String or byte-string assignment would truncate non-padding data.
    StringDataRightTruncation,
    /// Numeric assignment cannot be represented by the target type.
    NumericValueOutOfRange,
}

impl StoreAssignmentException {
    /// Map this store-assignment exception to its ISO GQLSTATUS code.
    #[must_use]
    pub const fn gqlstatus(self) -> &'static str {
        match self {
            Self::StringDataRightTruncation => "22001",
            Self::NumericValueOutOfRange => "22003",
        }
    }
}

/// Error raised while applying ISO store-assignment conversion rules.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("store assignment to property {property} failed: {reason}")]
#[diagnostic(code(SLENE_G_027))]
pub struct StoreAssignmentError {
    /// Property being assigned.
    pub property: DbString,
    /// Data-exception family.
    pub exception: StoreAssignmentException,
    /// Human-readable reason.
    pub reason: String,
}

/// What proved that a persistence directory already holds a committed store.
///
/// No one of these is sufficient, because a store moves between them. A WAL that
/// has never been checkpointed carries its whole dataset as entries; once
/// rotation runs, that data moves into a snapshot and the active WAL is reset
/// to a bare header, so the file looks empty while the directory is full; and a
/// standalone export is a snapshot with no MANIFEST and no WAL at all. Each
/// variant names the evidence that actually fired, so a refusal says which
/// shape was found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExistingStoreEvidence {
    /// The WAL itself carries committed entries past its header.
    WalEntries,
    /// A `MANIFEST` beside the WAL names a published snapshot epoch. This is
    /// the state a directory is left in by every checkpoint.
    PublishedManifest,
    /// A WAL file owns the directory. Presence is the test, not content: a
    /// bare-header WAL still declares an epoch that a standalone snapshot
    /// would preclaim.
    ActiveWal,
    /// A `snapshot.N.snap` with no `MANIFEST` names the directory's whole
    /// dataset. Recovery treats such a directory as a store — it applies the
    /// highest on-disk snapshot and seeds a fresh WAL header from it — so
    /// attaching an unrelated WAL beside one produces a header that disagrees
    /// with the snapshot it will be replayed against.
    StandaloneSnapshot,
}

impl std::fmt::Display for ExistingStoreEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::WalEntries => "the WAL carries committed entries",
            Self::PublishedManifest => "a MANIFEST names a published snapshot",
            Self::ActiveWal => "a WAL file owns this directory",
            Self::StandaloneSnapshot => "a snapshot names this directory's dataset",
        };
        f.write_str(text)
    }
}

/// Error type for graph storage and mutation operations.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum GraphError {
    /// The requested node row does not exist.
    #[error("node not found: {id}")]
    #[diagnostic(code(SLENE_G_001))]
    NodeNotFound {
        /// Missing node ID.
        id: NodeId,
    },

    /// The requested edge row does not exist.
    #[error("edge not found: {id}")]
    #[diagnostic(code(SLENE_G_002))]
    EdgeNotFound {
        /// Missing edge ID.
        id: EdgeId,
    },

    /// The requested node row exists but is not alive.
    #[error("node {id} is not alive")]
    #[diagnostic(code(SLENE_G_003))]
    NodeNotAlive {
        /// Dead node ID.
        id: NodeId,
    },

    /// The requested edge row exists but is not alive.
    #[error("edge {id} is not alive")]
    #[diagnostic(code(SLENE_G_004))]
    EdgeNotAlive {
        /// Dead edge ID.
        id: EdgeId,
    },

    /// The dense row store filled the v1 row-addressable range (max 2^32 rows).
    ///
    /// Post-4c the cap is a *row count*, not an id value: rows append at the
    /// dense end and `u32::MAX` is reserved as `RowIndex::TOMBSTONE`, so the last
    /// addressable row is `u32::MAX - 1`.
    #[error("{kind} row store is full ({rows} rows; max {max_rows})")]
    #[diagnostic(code(SLENE_G_005))]
    RowSpaceExhausted {
        /// `"node"` or `"edge"`.
        kind: &'static str,
        /// The current row count that hit the cap.
        rows: u64,
        /// The maximum addressable row count.
        max_rows: u64,
    },

    /// A monotonic identity or generation counter cannot advance without reuse.
    #[error("{kind} counter exhausted")]
    #[diagnostic(code(SLENE_G_030))]
    CounterExhausted {
        /// Counter whose next value cannot be represented.
        kind: &'static str,
    },

    /// The graph snapshot violates a structural invariant (e.g., row count
    /// exceeds the addressable u32 range).
    #[error("graph snapshot is inconsistent: {reason}")]
    #[diagnostic(code(SLENE_G_006))]
    Inconsistent {
        /// Free-form description of the inconsistency.
        reason: String,
    },

    /// A property index already exists for this `(label, property)`.
    #[error("property index already exists for ({label}, {property})")]
    #[diagnostic(code(SLENE_G_007))]
    PropertyIndexAlreadyExists {
        /// Indexed node label.
        label: DbString,
        /// Indexed property key.
        property: DbString,
    },

    /// The named property index does not exist.
    #[error("property index does not exist for ({label}, {property})")]
    #[diagnostic(code(SLENE_G_008))]
    PropertyIndexNotFound {
        /// Indexed node label.
        label: DbString,
        /// Indexed property key.
        property: DbString,
    },

    /// A value cannot be admitted to the declared property index kind.
    #[error(
        "property index ({label}, {property}) expected {expected_kind:?} but observed {observed}"
    )]
    #[diagnostic(code(SLENE_G_009))]
    IndexValueRejected {
        /// Indexed node label.
        label: DbString,
        /// Indexed property key.
        property: DbString,
        /// Registered index kind.
        expected_kind: TypedIndexKind,
        /// Observed value kind or `"NaN"`.
        observed: &'static str,
    },

    /// A composite property index already exists for this `(label, properties...)`.
    #[error("composite property index already exists for ({label}, {properties:?})")]
    #[diagnostic(code(SLENE_G_020))]
    CompositePropertyIndexAlreadyExists {
        /// Indexed node label.
        label: DbString,
        /// Indexed property keys in declaration order.
        ///
        /// Boxed so this variant does not inflate `GraphError` past clippy's
        /// `result_large_err` byte threshold: an inline `SmallVec<[DbString; 4]>`
        /// is ~104 B (four owned string values plus header), and the variant
        /// otherwise drives every `GraphResult<T>` stack slot over the limit.
        /// The `Box` pushes the allocation onto the cold error-construction
        /// path.
        properties: Box<SmallVec<[DbString; 4]>>,
    },

    /// A vector property index already exists for this `(label, property)`.
    #[error("vector index already exists for ({label}, {property})")]
    #[diagnostic(code(SLENE_G_021))]
    VectorIndexAlreadyExists {
        /// Indexed node label.
        label: DbString,
        /// Indexed vector property key.
        property: DbString,
    },

    /// A vector index was declared with an invalid dimensionality.
    #[error("vector index dimension must be non-zero, observed {dimension}")]
    #[diagnostic(code(SLENE_G_022))]
    VectorIndexInvalidDimension {
        /// Declared vector dimensionality.
        dimension: u32,
    },

    /// A vector index was declared with invalid HNSW construction parameters.
    #[error(
        "invalid HNSW vector index config max_neighbors={max_neighbors}, ef_construction={ef_construction}: {reason}"
    )]
    #[diagnostic(code(SLENE_G_024))]
    VectorIndexInvalidHnswConfig {
        /// Declared HNSW `M` fanout.
        max_neighbors: u16,
        /// Declared HNSW construction beam width.
        ef_construction: u16,
        /// Reason the configuration is rejected.
        reason: &'static str,
    },

    /// A vector index was declared with invalid IVF construction parameters.
    #[error("invalid IVF vector index config target_centroids={target_centroids}: {reason}")]
    #[diagnostic(code(SLENE_G_025))]
    VectorIndexInvalidIvfConfig {
        /// Declared IVF target centroid count.
        target_centroids: u16,
        /// Reason the configuration is rejected.
        reason: &'static str,
    },

    /// A value cannot be admitted to a vector index.
    #[error(
        "vector index ({label}, {property}) expected VECTOR<{expected_dimension}> but observed {observed}"
    )]
    #[diagnostic(code(SLENE_G_023))]
    VectorIndexValueRejected {
        /// Indexed node label.
        label: DbString,
        /// Indexed vector property key.
        property: DbString,
        /// Registered vector dimensionality.
        expected_dimension: u32,
        /// Observed value kind or dimensionality.
        observed: String,
    },

    /// A text index already exists for this `(label, property)`.
    #[error("text index already exists for ({label}, {property})")]
    #[diagnostic(code(SLENE_G_026))]
    TextIndexAlreadyExists {
        /// Indexed node label.
        label: DbString,
        /// Indexed string property key.
        property: DbString,
    },

    /// A closed graph mutation violates its bound graph type.
    #[error(transparent)]
    #[diagnostic(transparent)]
    TypeViolation(#[from] TypeViolation),

    /// A store assignment failed before the graph mutation was applied.
    #[error(transparent)]
    #[diagnostic(transparent)]
    StoreAssignment(Box<StoreAssignmentError>),

    /// A commit-critical durable provider rejected or failed a write.
    #[error("durable provider failed: {reason}")]
    #[diagnostic(code(SLENE_G_015))]
    Durable {
        /// Human-readable durable provider failure reason.
        reason: String,
    },

    /// A commit or checkpoint reached the durable path and then failed,
    /// leaving its outcome undetermined: the work may or may not be present
    /// after the mandated reopen.
    ///
    /// Both operations poison the committer, and the caller's obligation is the
    /// same for either, which is why one variant covers both. It is also
    /// already true of the API: once the committer is poisoned every later
    /// submit — commit, compact, vector-index rebuild, or a second checkpoint —
    /// short-circuits into this variant. Before it covered checkpoints, the
    /// *only* caller that did not see it was the one whose checkpoint caused
    /// the poison.
    ///
    /// # Why this is not [`GraphError::Durable`]
    ///
    /// ISO/IEC 39075:2024 §8.4 `<commit command>` GR 1)b) says that when an
    /// error prevents commitment, the transaction's changes "are canceled" and
    /// *transaction rollback (40000)* is raised. A plain durable failure means
    /// exactly that. This variant is the case where the engine cannot honour
    /// the "are canceled" half, so reporting an unqualified rollback would be a
    /// lie in the dangerous direction — a caller that reads `Err` as "this
    /// transition did not happen" re-drives the write after the reopen and
    /// double-applies it.
    ///
    /// §23.1 Table 8 supplies the honest spelling: class 40 subclass 003,
    /// *statement completion unknown*, which is what
    /// [`GraphError::gqlstatus`] returns here.
    ///
    /// # When it is raised
    ///
    /// Every commit error-acked on a committer poison exit, because in each
    /// case the WAL bytes can outlive the failure:
    ///
    /// - a partial-batch append failure error-acks the run members appended
    ///   before it, whose records are already written;
    /// - a group-flush failure error-acks the whole run, and a failed `fsync`
    ///   does not imply the data is absent from stable storage;
    /// - a publish-tail panic after a *successful* group flush error-acks
    ///   commits that are unambiguously durable and merely unpublished.
    ///
    /// And every checkpoint failure that poisons the committer — anything in
    /// the watermark/rotation phase other than a verified artifact collision.
    /// By then `append_checkpoint_watermark_record` has already consumed a
    /// physical sequence, so the engine cannot prove which side of the MANIFEST
    /// commit point it reached: the new epoch may or may not be published.
    /// Preparation failures happen before any of that and stay their own typed
    /// errors, as does the artifact collision, which leaves the active epoch
    /// unchanged and the writer usable.
    ///
    /// The committer-managed WAL runs under
    /// [`SyncPolicy::OnFlushOnly`](selene_persist::SyncPolicy), so an appended
    /// record is a complete, checksum-valid frame in the page cache. Reopening
    /// does not discard it — only losing the page cache does. Recovery's tail
    /// repair truncates a torn tail, and such a frame is not torn.
    ///
    /// # This over-approximates, deliberately
    ///
    /// Some commits error-acked this way genuinely left nothing behind — an
    /// append that failed on the first durable provider of a one-member run,
    /// for instance, wrote no WAL record at all. They are reported as unknown
    /// anyway, because the committer cannot tell them apart from the members
    /// that did write: there is no flushed-offset watermark to compare against.
    ///
    /// Over-reporting costs a caller one needless read-back after the reopen.
    /// Under-reporting causes a double-apply. Adding that watermark, and
    /// truncating to it on the poison exit, would make the appended-not-flushed
    /// cases genuinely canceled and let them report a definite `40000`; the
    /// publish-tail-panic case is durable and stays `40003` regardless.
    ///
    /// # What a caller must do
    ///
    /// Treat the work as unknown. Quiesce, drop the handle, reopen through
    /// [`crate::SharedGraph::recover`], read back to determine whether it
    /// landed, and only then decide whether to retry. Retrying blind
    /// double-applies. [`GraphError::requires_reopen`] is the supported test.
    #[error("outcome is indeterminate; reopen and reconcile: {reason}")]
    #[diagnostic(code(SLENE_G_029))]
    IndeterminateOutcome {
        /// Where the commit stopped, for operator diagnosis.
        reason: String,
    },

    /// The commit was cancelled at the pre-WAL cut-line (BRIEF-117): the
    /// committer observed the cancellation token set before it appended the
    /// commit to the WAL, so nothing was persisted or published. Past the WAL
    /// append a commit is irrevocable and this is never returned.
    #[error("commit cancelled before durable append")]
    #[diagnostic(code(SLENE_G_019))]
    Cancelled,

    /// A graph was attached to a persistence directory that already holds a
    /// committed store.
    ///
    /// Attaching does not replay: the caller's graph is used as-is and its
    /// commits append to whatever is already there. When that graph does not
    /// already reflect the store, ids restart at 1 and collide with ids the
    /// store allocated, and the directory stops recovering at all. Recovering
    /// is the operation that reads an existing store.
    #[error(
        "{path} already holds a committed store ({evidence}); \
         use SharedGraph::recover to open it"
    )]
    #[diagnostic(code(SLENE_G_028))]
    ExistingStore {
        /// Path of the WAL the caller asked to attach.
        path: std::path::PathBuf,
        /// What was found on disk. A rotated WAL is header-only while its data
        /// lives in a snapshot, so the WAL alone cannot answer this.
        evidence: ExistingStoreEvidence,
    },

    /// Error propagated from selene-core.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Core(#[from] CoreError),

    /// Error propagated from an index provider.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Provider(#[from] ProviderError),

    /// Error propagated from persistence recovery.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Persist(#[from] PersistError),
}

impl GraphError {
    /// Whether this error means the graph handle is unusable and the caller
    /// must reopen through [`crate::SharedGraph::recover`] before trusting or
    /// retrying anything.
    ///
    /// The supported test for the condition, in preference to matching a
    /// variant. Which failures poison the committer is an engine-internal
    /// judgement that has already changed once — a checkpoint's
    /// watermark/rotation phase started reporting it in the same release that
    /// commits did — and a caller branching on this predicate does not have to
    /// track that. It is also the honest shape for the question: "must I
    /// reopen?" is what an embedder acts on, and a variant match is only a
    /// proxy for it.
    ///
    /// Exactly one variant returns `true` today. That is deliberate rather than
    /// incidental: the whole point of [`GraphError::IndeterminateOutcome`] is
    /// that a single condition covers every operation whose durable outcome the
    /// engine cannot determine, so a caller has one thing to handle.
    ///
    /// `false` does not mean "succeeded" — it means the failure was definite
    /// and left the handle usable, so an ordinary retry or a typed recovery is
    /// safe.
    #[must_use]
    pub const fn requires_reopen(&self) -> bool {
        matches!(self, Self::IndeterminateOutcome { .. })
    }

    /// Map this error to its 5-character ISO GQLSTATUS code.
    #[must_use]
    pub const fn gqlstatus(&self) -> &'static str {
        match self {
            Self::NodeNotFound { .. }
            | Self::EdgeNotFound { .. }
            | Self::NodeNotAlive { .. }
            | Self::EdgeNotAlive { .. } => "22G03",
            Self::RowSpaceExhausted { .. } | Self::CounterExhausted { .. } => "53000",
            Self::Inconsistent { .. } => "5GQL0",
            Self::PropertyIndexAlreadyExists { .. }
            | Self::PropertyIndexNotFound { .. }
            | Self::IndexValueRejected { .. }
            | Self::CompositePropertyIndexAlreadyExists { .. }
            | Self::VectorIndexAlreadyExists { .. }
            | Self::VectorIndexInvalidDimension { .. }
            | Self::VectorIndexInvalidHnswConfig { .. }
            | Self::VectorIndexInvalidIvfConfig { .. }
            | Self::VectorIndexValueRejected { .. }
            | Self::TextIndexAlreadyExists { .. } => "22G03",
            Self::TypeViolation(_) => "G2000",
            Self::StoreAssignment(source) => source.exception.gqlstatus(),
            Self::Core(source) => source.gqlstatus(),
            Self::Durable { .. } | Self::ExistingStore { .. } => "5GQL0",
            // ISO §23.1 Table 8: transaction rollback (40) subclass 003,
            // "statement completion unknown". §8.4 GR 1)b) delegates the
            // subclass to the implementation (IE008); this one is
            // standard-defined rather than invented, and it is the only code in
            // the table that says what actually happened.
            Self::IndeterminateOutcome { .. } => "40003",
            Self::Cancelled => "5GQL2",
            Self::Provider(_) | Self::Persist(_) => "5GQL0",
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use selene_core::db_string;

    use super::*;
    use crate::ProviderError;

    #[rstest]
    #[case(GraphError::NodeNotFound { id: NodeId::new(1) }, "22G03")]
    #[case(GraphError::EdgeNotFound { id: EdgeId::new(1) }, "22G03")]
    #[case(GraphError::NodeNotAlive { id: NodeId::new(1) }, "22G03")]
    #[case(GraphError::EdgeNotAlive { id: EdgeId::new(1) }, "22G03")]
    #[case(GraphError::CounterExhausted { kind: "edge identity" }, "53000")]
    #[case(
        GraphError::RowSpaceExhausted { kind: "node", rows: 4_294_967_295, max_rows: 4_294_967_295 },
        "53000"
    )]
    #[case(
        GraphError::Inconsistent { reason: "row index exceeds u32::MAX".to_owned() },
        "5GQL0"
    )]
    #[case(
        GraphError::PropertyIndexAlreadyExists {
            label: db_string("err.label").unwrap(),
            property: db_string("err.property").unwrap(),
        },
        "22G03"
    )]
    #[case(
        GraphError::PropertyIndexNotFound {
            label: db_string("err.label.missing").unwrap(),
            property: db_string("err.property.missing").unwrap(),
        },
        "22G03"
    )]
    #[case(
        GraphError::IndexValueRejected {
            label: db_string("err.label.rejected").unwrap(),
            property: db_string("err.property.rejected").unwrap(),
            expected_kind: TypedIndexKind::I64,
            observed: "String",
        },
        "22G03"
    )]
    #[case(
        GraphError::VectorIndexAlreadyExists {
            label: db_string("err.label.vector.exists").unwrap(),
            property: db_string("err.property.vector.exists").unwrap(),
        },
        "22G03"
    )]
    #[case(GraphError::VectorIndexInvalidDimension { dimension: 0 }, "22G03")]
    #[case(
        GraphError::VectorIndexInvalidHnswConfig {
            max_neighbors: 24,
            ef_construction: 8,
            reason: "ef_construction must be at least max_neighbors",
        },
        "22G03"
    )]
    #[case(
        GraphError::VectorIndexInvalidIvfConfig {
            target_centroids: 0,
            reason: "target_centroids must be greater than zero",
        },
        "22G03"
    )]
    #[case(
        GraphError::VectorIndexValueRejected {
            label: db_string("err.label.vector.rejected").unwrap(),
            property: db_string("err.property.vector.rejected").unwrap(),
            expected_dimension: 3,
            observed: "VECTOR<4>".to_owned(),
        },
        "22G03"
    )]
    #[case(
        GraphError::TextIndexAlreadyExists {
            label: db_string("err.label.text.exists").unwrap(),
            property: db_string("err.property.text.exists").unwrap(),
        },
        "22G03"
    )]
    #[case(
        GraphError::TypeViolation(TypeViolation::UnknownEdgeLabel {
            id: EdgeId::new(1),
            label: db_string("err.edge.label").unwrap(),
        }),
        "G2000"
    )]
    #[case(
        GraphError::StoreAssignment(Box::new(StoreAssignmentError {
            property: db_string("err.assignment.property").unwrap(),
            exception: StoreAssignmentException::StringDataRightTruncation,
            reason: "right truncation".to_owned(),
        })),
        "22001"
    )]
    #[case(GraphError::Core(CoreError::ZeroIdentifier), "0G003")]
    #[case(GraphError::Durable { reason: "wal unavailable".to_owned() }, "5GQL0")]
    #[case(
        GraphError::IndeterminateOutcome { reason: "group flush failed".to_owned() },
        "40003"
    )]
    #[case(GraphError::Cancelled, "5GQL2")]
    #[case(
        GraphError::Provider(ProviderError::Inconsistent { reason: "duplicate provider tag DEMO".to_owned() }),
        "5GQL0"
    )]
    #[case(GraphError::Persist(PersistError::MalformedSnapshotFilename), "5GQL0")]
    fn gqlstatus_for_each_variant(#[case] error: GraphError, #[case] status: &str) {
        assert_eq!(error.gqlstatus(), status);
        assert!(
            selene_core::gqlstatus_name(status).is_some(),
            "GQLSTATUS code {status} emitted by GraphError but not in ALL_GQLSTATUS_NAMES"
        );
    }

    #[test]
    fn core_error_variant_propagates() {
        fn inner() -> Result<(), CoreError> {
            Err(CoreError::ZeroIdentifier)
        }
        fn outer() -> GraphResult<()> {
            inner()?;
            Ok(())
        }
        assert!(matches!(
            outer(),
            Err(GraphError::Core(CoreError::ZeroIdentifier))
        ));
    }

    /// Internal miette `SLENE_G_*` diagnostic codes must be unique across the
    /// graph-crate diagnostic enums (`GraphError`, `TypeViolation`,
    /// `ProviderError`). A reused code makes two semantically different errors
    /// indistinguishable in diagnostics. (GRAPH-42 regression guard.)
    #[test]
    fn internal_diagnostic_codes_are_unique() {
        use miette::Diagnostic;
        use selene_core::{LabelSet, PropertyValueType};

        use crate::graph_types::EdgeEndpointDef;
        use crate::type_validator::EntityId;

        let lbl = db_string("codes.label").unwrap();
        let prop = db_string("codes.property").unwrap();

        // One representative of every code-carrying GraphError variant (the
        // transparent `TypeViolation`/`Core`/`Persist`/`Provider` wrappers carry
        // no own code).
        let graph_errors: Vec<GraphError> = vec![
            GraphError::NodeNotFound { id: NodeId::new(1) },
            GraphError::EdgeNotFound { id: EdgeId::new(1) },
            GraphError::NodeNotAlive { id: NodeId::new(1) },
            GraphError::EdgeNotAlive { id: EdgeId::new(1) },
            GraphError::RowSpaceExhausted {
                kind: "node",
                rows: 1,
                max_rows: 1,
            },
            GraphError::Inconsistent {
                reason: "x".to_owned(),
            },
            GraphError::PropertyIndexAlreadyExists {
                label: lbl.clone(),
                property: prop.clone(),
            },
            GraphError::PropertyIndexNotFound {
                label: lbl.clone(),
                property: prop.clone(),
            },
            GraphError::IndexValueRejected {
                label: lbl.clone(),
                property: prop.clone(),
                expected_kind: TypedIndexKind::I64,
                observed: "String",
            },
            GraphError::CompositePropertyIndexAlreadyExists {
                label: lbl.clone(),
                properties: Box::default(),
            },
            GraphError::VectorIndexAlreadyExists {
                label: lbl.clone(),
                property: prop.clone(),
            },
            GraphError::VectorIndexInvalidDimension { dimension: 0 },
            GraphError::VectorIndexInvalidHnswConfig {
                max_neighbors: 24,
                ef_construction: 8,
                reason: "ef_construction must be at least max_neighbors",
            },
            GraphError::VectorIndexInvalidIvfConfig {
                target_centroids: 0,
                reason: "target_centroids must be greater than zero",
            },
            GraphError::VectorIndexValueRejected {
                label: lbl.clone(),
                property: prop.clone(),
                expected_dimension: 3,
                observed: "VECTOR<4>".to_owned(),
            },
            GraphError::TextIndexAlreadyExists {
                label: lbl.clone(),
                property: prop.clone(),
            },
            GraphError::StoreAssignment(Box::new(StoreAssignmentError {
                property: prop.clone(),
                exception: StoreAssignmentException::StringDataRightTruncation,
                reason: "right truncation".to_owned(),
            })),
            GraphError::Durable {
                reason: "x".to_owned(),
            },
            GraphError::Cancelled,
        ];

        let type_violations: Vec<TypeViolation> = vec![
            TypeViolation::UnknownNodeLabel {
                id: NodeId::new(1),
                labels: LabelSet::new(),
            },
            TypeViolation::UnknownEdgeLabel {
                id: EdgeId::new(1),
                label: lbl.clone(),
            },
            TypeViolation::EdgeEndpointTypeMismatch {
                id: EdgeId::new(1),
                label: lbl.clone(),
                expected_source_type: EdgeEndpointDef::Any,
                observed_source_type: 0,
                expected_target_type: EdgeEndpointDef::Any,
                observed_target_type: 0,
            },
            TypeViolation::MissingRequiredProperty {
                entity_id: EntityId::Node(NodeId::new(1)),
                property: prop.clone(),
                declared_in: lbl.clone(),
            },
            TypeViolation::PropertyTypeMismatch {
                entity_id: EntityId::Node(NodeId::new(1)),
                property: prop.clone(),
                expected: PropertyValueType::Int,
                observed: "String",
            },
            TypeViolation::ExtensionValueRejected {
                entity_id: EntityId::Node(NodeId::new(1)),
                property: prop.clone(),
            },
            TypeViolation::UndeclaredProperty {
                entity_id: EntityId::Node(NodeId::new(1)),
                property: prop.clone(),
            },
            TypeViolation::ImmutablePropertyUpdate {
                entity_id: EntityId::Node(NodeId::new(1)),
                property: prop,
                declared_in: lbl,
            },
        ];

        let provider_errors: Vec<ProviderError> = vec![
            ProviderError::InvalidPayload {
                reason: "x".to_owned(),
            },
            ProviderError::SerializationFailed {
                reason: "x".to_owned(),
            },
            ProviderError::Inconsistent {
                reason: "x".to_owned(),
            },
        ];

        let mut codes: Vec<String> = Vec::new();
        codes.extend(
            graph_errors
                .iter()
                .filter_map(|e| e.code().map(|c| c.to_string())),
        );
        codes.extend(
            type_violations
                .iter()
                .filter_map(|e| e.code().map(|c| c.to_string())),
        );
        codes.extend(
            provider_errors
                .iter()
                .filter_map(|e| e.code().map(|c| c.to_string())),
        );

        let mut seen = std::collections::HashSet::new();
        for code in &codes {
            assert!(
                seen.insert(code.clone()),
                "duplicate internal diagnostic code {code} across graph-crate error enums"
            );
        }
    }
}
