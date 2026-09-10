//! Durable vector-index configuration payloads.

use serde::{Deserialize, Serialize};

/// HNSW construction parameters for native vector indexes.
///
/// `max_neighbors` is the HNSW `M` fanout. Layer zero may store up to
/// `2 * max_neighbors` links, while upper layers store up to `max_neighbors`.
/// `ef_construction` is the candidate beam width used while inserting vectors.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub struct HnswIndexConfig {
    /// HNSW `M` fanout for upper-layer neighbor lists.
    pub max_neighbors: u16,
    /// Candidate beam width used when constructing HNSW links.
    pub ef_construction: u16,
}

impl HnswIndexConfig {
    /// Native engine maximum HNSW neighbor fanout.
    pub const MAX_NEIGHBORS: u16 = 512;
    /// Native engine maximum construction beam width.
    pub const MAX_EF_CONSTRUCTION: u16 = 4096;
    /// Default HNSW `M` fanout.
    pub const DEFAULT_MAX_NEIGHBORS: u16 = 18;
    /// Default HNSW construction beam width.
    pub const DEFAULT_EF_CONSTRUCTION: u16 = 64;
    /// Default HNSW construction configuration.
    pub const DEFAULT: Self = Self {
        max_neighbors: Self::DEFAULT_MAX_NEIGHBORS,
        ef_construction: Self::DEFAULT_EF_CONSTRUCTION,
    };

    /// Construct a configuration without validation.
    ///
    /// The graph layer validates bounds because it owns index memory policy.
    #[must_use]
    pub const fn new(max_neighbors: u16, ef_construction: u16) -> Self {
        Self {
            max_neighbors,
            ef_construction,
        }
    }

    /// Return true when this config is the engine default.
    #[must_use]
    pub const fn is_default(self) -> bool {
        self.max_neighbors == Self::DEFAULT_MAX_NEIGHBORS
            && self.ef_construction == Self::DEFAULT_EF_CONSTRUCTION
    }
}

impl Default for HnswIndexConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// IVF construction parameters for native vector indexes.
///
/// `target_centroids` is the requested inverted-list count used during bulk
/// training and rebuild. The graph layer validates the upper bound and caps the
/// effective count to the number of live vectors being trained, so explicit
/// configuration never creates empty centroids beyond the current corpus.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub struct IvfIndexConfig {
    /// Requested IVF centroid/list count.
    pub target_centroids: u16,
}

impl IvfIndexConfig {
    /// Native engine maximum explicit centroid count.
    pub const MAX_TARGET_CENTROIDS: u16 = 1024;
    /// Construct a configuration without validation.
    ///
    /// The graph layer validates bounds because it owns index memory policy.
    #[must_use]
    pub const fn new(target_centroids: u16) -> Self {
        Self { target_centroids }
    }
}
