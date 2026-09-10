//! In-memory property graph runtime.
//!
//! The graph crate owns node/edge storage, label sets, property maps, mixed-edge
//! incidence, built-in label/property indexes, typed mutation validation, and
//! the CORE persistence provider. `SharedGraph` serializes writes through a
//! transaction boundary while readers observe immutable snapshots. selene-db is
//! a single native engine: the higher `selene-gql` layer owns GQL
//! binding/planning and the one frozen native procedure registry.
//!
//! Edges have intrinsic [`selene_core::EdgeDirectionality`].
//! [`Mutator::create_edge`](mutator::Mutator::create_edge) remains directed;
//! [`Mutator::create_mixed_edge`](mutator::Mutator::create_mixed_edge) accepts
//! either kind with one stable identity. Undirected incidence is separate from
//! directed incoming/outgoing adjacency and canonical endpoints are not a
//! source/destination pair. Query/path orientation is owned by F01-PR04.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod adjacency;
mod candidate_set;
pub mod candidate_state;
mod candidate_state_shared;
mod checkpoint;
pub mod chunked_vec;
pub(crate) mod committer;
pub(crate) mod committer_batch;
pub mod compaction;
pub(crate) mod composite_property_index;
pub mod composite_typed_index;
mod consistency;
pub mod core_provider;
pub mod durable_provider;
pub mod error;
pub mod graph;
pub mod graph_types;
pub mod id_allocator;
pub(crate) mod id_map;
pub mod index_provider;
pub mod json_search;
mod json_search_candidates;
pub mod mutator;
pub(crate) mod panic_payload;
pub(crate) mod parallel_scan;
pub(crate) mod property_index;
pub(crate) mod provider_fanout;
pub mod reachability;
mod recover;
pub(crate) mod reentry;
pub(crate) mod schema_index_kind;
pub mod shared;
mod shared_snapshot;
pub mod store;
pub mod text_index;
pub mod text_search;
pub mod type_validator;
mod typed_float_key;
pub mod typed_index;
pub(crate) mod validated_candidates;
pub mod vector_index;
pub mod vector_search;
pub mod write_txn;

pub use adjacency::{AdjacencyEdge, AdjacencyEntry};
pub use candidate_set::{CandidateKind, CandidateSet, Edge, Node};
pub use candidate_state::{
    CANDIDATE_STATE_PROVIDER_TAG, CANDIDATE_STATE_SUB, CandidateStateSpec,
    MaintainedCandidateStateProvider,
};
pub use checkpoint::{CheckpointConfig, CheckpointOutcome};
pub use chunked_vec::ChunkedVec;
pub use committer_batch::CommitBatching;
pub use compaction::{CompactedCore, CompactionReport, CompactionStats, compact_core};
pub use composite_typed_index::{
    CompositeIndexValueError, CompositeKey, CompositeKeyComponent, CompositeTypedIndex,
};
pub use core_provider::{
    CORE_EDGE_SUB, CORE_GTYP_SUB, CORE_META_SUB, CORE_NODE_SUB, CORE_PROVIDER_TAG, CORE_SCMA_SUB,
    CORE_TIDX_SUB, CORE_VIDX_SUB, CoreProvider, DurableState,
};
pub use durable_provider::DurableProvider;
pub use error::{
    CandidateSetError, CandidateSetResult, ExistingStoreEvidence, GraphError, GraphResult,
};
pub use graph::{
    CompositePropertyIndexEntry, GraphMeta, IndexedEntity, PropertyIndexEntry,
    PropertyIndexReadInfo, PropertyIndexStatsRow, SeleneGraph, TextIndexEntry, VectorIndexEntry,
};
pub use graph_types::{
    DropBehavior, EdgeEndpointDef, EdgeTypeDef, GraphTypeDef, MAX_RECORD_TYPE_NESTING, NodeTypeDef,
    PropertyDefaultRecordField, PropertyDefaultValue, PropertyElementType, PropertyTypeDef,
    RecordFieldType, RecordFieldTypeDef, RecordFieldTypes, ValidationMode,
};
pub use id_allocator::IdAllocator;
pub use index_provider::{
    IndexProvider, ProviderError, ProviderTag, SubTag, VectorCandidateStateInfo,
};
pub use json_search::{
    JSON_PATH_SELECTOR_LIMIT, JsonContainmentHit, JsonPathContainmentHit, JsonPathHit,
    JsonPathValueHit, JsonSearchError,
};
pub use json_search_candidates::JsonPathContainmentCandidateOptions;
pub use mutator::Mutator;
pub use reachability::{ReachabilityDirection, ReachabilityError, ReachableNode};
pub use selene_core::JsonPathSelector;
pub use selene_core::{HnswIndexConfig, IvfIndexConfig};
pub use selene_persist::{
    DEFAULT_WAL_FILE_NAME, SectionCompression, SyncPolicy, WalConfig, WalRotationOutcome,
    WalTailReason, WalTailRepair,
};
pub use shared::{SharedGraph, SharedGraphBuilder};
pub use store::{EdgeStore, NodeStore};
pub use text_index::{TextIndex, TextIndexMemoryUsage, TextIndexStats};
pub use text_search::{TextSearchError, TextSearchHit};
pub use type_validator::{EntityId, TypeViolation, validate_change, validate_entity_state};
pub use typed_float_key::{NotNanError, NotNanF32, NotNanF64};
pub use typed_index::{TypedIndex, TypedIndexKind};
pub use vector_index::{
    IVF_REBUILD_MIN_PENDING_RETRAIN_ENTRIES, IVF_REBUILD_PENDING_RETRAIN_BASIS_POINTS, VectorIndex,
    VectorIndexConfig, VectorIndexKind, VectorIndexMaintenancePolicy, VectorIndexMemoryUsage,
    VectorIndexRebuildEntry, VectorIndexRebuildReport, VectorIndexValueError,
};
pub use vector_search::{
    ApproximateVectorExpansionOptions, ApproximateVectorSearchOptions, VectorCandidateSet,
    VectorNeighborDirection, VectorNeighborSearchOptions, VectorNodeSearchHit, VectorSearchError,
};
pub use write_txn::{CommitOutcome, CommitWarning, WriteTxn};

#[cfg(test)]
mod closed_graph_tests;

#[cfg(test)]
mod candidate_set_tests;

#[cfg(test)]
mod candidate_set_validation_tests;
