//! Bounded, storage-neutral empty-control identities.

use serde::{Deserialize, Serialize};

use crate::{ControlError, PersistResult};

/// Opaque durable store identity, distinct from directory inodes and process handles.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct StoreId(pub(super) [u8; 16]);

impl StoreId {
    /// Validate canonical UUID-v4 bytes for a format-2 codec identity.
    /// This constructs no store and grants no filesystem authority.
    pub fn from_bytes(bytes: [u8; 16]) -> PersistResult<Self> {
        if bytes[6] >> 4 != 4 || bytes[8] >> 6 != 2 {
            return Err(ControlError::Lineage.into());
        }
        Ok(Self(bytes))
    }
    pub(super) fn fresh() -> Self {
        Self(*uuid::Uuid::new_v4().as_bytes())
    }

    /// Canonical UUID bytes of this durable identity.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl std::fmt::Display for StoreId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        uuid::Uuid::from_bytes(self.0).fmt(f)
    }
}

/// Nonzero durable store epoch. Empty control starts at one and preserves it on reopen.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct StoreEpoch(pub(super) u64);

impl StoreEpoch {
    /// Construct a checked nonzero epoch for a format-2 codec context.
    pub fn new(value: u64) -> PersistResult<Self> {
        if value == 0 {
            return Err(ControlError::Lineage.into());
        }
        Ok(Self(value))
    }
    /// The validated nonzero epoch number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Nonzero immutable control manifest generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ManifestGeneration(pub(super) u64);

impl ManifestGeneration {
    /// The validated nonzero generation number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(super) fn next(self) -> PersistResult<Self> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| ControlError::GenerationExhausted.into())
    }

    pub(super) fn name(self) -> String {
        format!("MANIFEST-{:020}.control", self.0)
    }
}

/// Caller-supplied profile, Unicode, and collation identity required to reopen safely.
///
/// Persistence does not import higher engine layers or invent their compatibility
/// rules. The caller supplies the expected exact identity. Names are bounded to
/// 96 UTF-8 bytes; version tuples and hashes have fixed width. Storage format 2
/// here describes **empty control only**, not a format-2 transaction codec.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityIdentity {
    pub(super) profile: String,
    pub(super) profile_version: u32,
    pub(super) profile_hash: [u8; 32],
    pub(super) unicode_version: [u16; 3],
    pub(super) collation: String,
    pub(super) collation_version: u32,
}

impl CompatibilityIdentity {
    /// Construct bounded compatibility metadata from the owning runtime's authority.
    ///
    /// # Errors
    /// Rejects empty, overlong, or control-character-bearing identity names.
    pub fn new(
        profile: &str,
        profile_version: u32,
        profile_hash: [u8; 32],
        unicode_version: [u16; 3],
        collation: &str,
        collation_version: u32,
    ) -> PersistResult<Self> {
        let identity = Self {
            profile: profile.into(),
            profile_version,
            profile_hash,
            unicode_version,
            collation: collation.into(),
            collation_version,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub(super) fn validate(&self) -> PersistResult<()> {
        for name in [&self.profile, &self.collation] {
            if name.is_empty() || name.len() > 96 || name.chars().any(char::is_control) {
                return Err(ControlError::Compatibility.into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct Parent {
    pub generation: ManifestGeneration,
    pub digest: [u8; 32],
}

/// Immutable manifest for an empty store; this cannot represent graph/catalog data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EmptyManifest {
    pub(super) store_id: StoreId,
    pub(super) epoch: StoreEpoch,
    pub(super) generation: ManifestGeneration,
    pub(super) format: [u16; 2],
    pub(super) identity: CompatibilityIdentity,
    pub(super) parent: Option<Parent>,
}

impl EmptyManifest {
    /// Durable store identity.
    #[must_use]
    pub const fn store_id(&self) -> StoreId {
        self.store_id
    }
    /// Durable store epoch.
    #[must_use]
    pub const fn epoch(&self) -> StoreEpoch {
        self.epoch
    }
    /// Selected immutable manifest generation.
    #[must_use]
    pub const fn generation(&self) -> ManifestGeneration {
        self.generation
    }
    /// Profile and comparison identity passed by the owning runtime.
    #[must_use]
    pub fn compatibility(&self) -> &CompatibilityIdentity {
        &self.identity
    }

    pub(super) fn validate(&self) -> PersistResult<()> {
        if self.format != [2, 0] {
            return Err(ControlError::UnsupportedVersion.into());
        }
        if self.store_id.0 == [0; 16] || self.epoch.0 == 0 || self.generation.0 == 0 {
            return Err(ControlError::Lineage.into());
        }
        match &self.parent {
            None if self.generation.0 == 1 => {}
            Some(parent)
                if parent.generation.0 != 0
                    && parent.generation.0.checked_add(1) == Some(self.generation.0) => {}
            _ => return Err(ControlError::Lineage.into()),
        }
        self.identity.validate()
    }
}

/// CURRENT selector binding exact immutable manifest bytes to identity and generation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CurrentSelector {
    pub(super) store_id: StoreId,
    pub(super) epoch: StoreEpoch,
    pub(super) generation: ManifestGeneration,
    pub(super) manifest_name: String,
    pub(super) digest: [u8; 32],
}

impl CurrentSelector {
    /// Validated immutable manifest basename; not filesystem authority.
    pub fn manifest_name(&self) -> &str {
        &self.manifest_name
    }
    /// Digest of the exact selected manifest bytes.
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
    /// Selected control generation, not transaction sequence.
    pub fn generation(&self) -> ManifestGeneration {
        self.generation
    }

    pub(super) fn from_manifest(manifest: &EmptyManifest, digest: [u8; 32]) -> Self {
        Self {
            store_id: manifest.store_id,
            epoch: manifest.epoch,
            generation: manifest.generation,
            manifest_name: manifest.generation.name(),
            digest,
        }
    }

    pub(super) fn validate(&self) -> PersistResult<()> {
        if self.store_id.0 == [0; 16]
            || self.epoch.0 == 0
            || self.generation.0 == 0
            || self.manifest_name != self.generation.name()
        {
            return Err(ControlError::Lineage.into());
        }
        crate::store_directory::validate_name(std::path::Path::new(&self.manifest_name))
    }
}
