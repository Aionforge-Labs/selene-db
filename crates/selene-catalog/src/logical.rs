//! Validated logical catalog serialization inputs, not a WAL or recovery engine.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{
    CatalogDescriptor, CatalogError, CatalogGeneration, CatalogObjectId, CatalogObjectKind,
    CatalogResult, CatalogSnapshot, CatalogSnapshotBuilder,
};

/// Logical descriptor image plus allocation high-water marks, including deleted IDs.
/// Decoding validates the complete dependency graph but cannot activate native code.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "WireRecords")]
pub struct CatalogLogicalRecords {
    generation: CatalogGeneration,
    high_water: BTreeMap<CatalogObjectKind, u64>,
    descriptors: Vec<CatalogDescriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRecords {
    generation: CatalogGeneration,
    high_water: BTreeMap<CatalogObjectKind, u64>,
    descriptors: Vec<CatalogDescriptor>,
}

impl TryFrom<WireRecords> for CatalogLogicalRecords {
    type Error = CatalogError;
    fn try_from(wire: WireRecords) -> CatalogResult<Self> {
        Self::new(wire.generation, wire.high_water, wire.descriptors)
    }
}

impl CatalogLogicalRecords {
    /// Validate a deterministic descriptor image and its monotonic allocation bounds.
    pub fn new(
        generation: CatalogGeneration,
        high_water: BTreeMap<CatalogObjectKind, u64>,
        mut descriptors: Vec<CatalogDescriptor>,
    ) -> CatalogResult<Self> {
        descriptors.sort_by_key(CatalogDescriptor::id);
        let records = Self {
            generation,
            high_water,
            descriptors,
        };
        records.reconstruct()?;
        Ok(records)
    }

    /// Borrow records in deterministic typed-ID order.
    #[must_use]
    pub fn descriptors(&self) -> &[CatalogDescriptor] {
        &self.descriptors
    }

    /// Allocation bounds retain deleted identity domains; they are not generations.
    #[must_use]
    pub fn high_water(&self) -> &BTreeMap<CatalogObjectKind, u64> {
        &self.high_water
    }

    /// Reconstruct only logical metadata, validating every parent and dependency.
    /// Ready flags still require production runtime admission before publication.
    pub fn reconstruct(&self) -> CatalogResult<CatalogSnapshot> {
        if self.descriptors.iter().any(|descriptor| {
            self.high_water
                .get(&descriptor.kind())
                .copied()
                .unwrap_or(0)
                < descriptor.id().get()
        }) {
            return Err(CatalogError::InvalidDeclaration {
                reason: "invalid_high_water",
            });
        }
        let catalog = self
            .descriptors
            .iter()
            .find(|descriptor| descriptor.kind() == CatalogObjectKind::Catalog)
            .ok_or(CatalogError::InvalidSyntheticRoot)?;
        let root = self
            .descriptors
            .iter()
            .find(|descriptor| descriptor.kind() == CatalogObjectKind::Directory)
            .ok_or(CatalogError::InvalidSyntheticRoot)?;
        let mut builder =
            CatalogSnapshotBuilder::new(self.generation, catalog.clone(), root.clone())?;
        for descriptor in &self.descriptors {
            if descriptor.id() != catalog.id() && descriptor.id() != root.id() {
                builder.insert(descriptor.clone())?;
            }
        }
        // Duplicate root identities must not disappear while extracting the seeds.
        if self
            .descriptors
            .windows(2)
            .any(|pair| pair[0].id() == pair[1].id())
        {
            return Err(CatalogError::InvalidDeclaration {
                reason: "duplicate_logical_identity",
            });
        }
        builder.build()
    }
}

/// Deterministic logical descriptor event for the future complete-transaction codec.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CatalogLogicalChange {
    /// Fresh identity allocation.
    Created(CatalogDescriptor),
    /// Same identity, checked descriptor revision change.
    Replaced {
        /// Required old descriptor generation.
        previous: CatalogGeneration,
        /// Replacement logical descriptor.
        descriptor: CatalogDescriptor,
    },
    /// Removal retains stable identity and its last revision, not a physical row.
    Dropped {
        /// Deleted identity.
        id: CatalogObjectId,
        /// Deleted descriptor revision.
        generation: CatalogGeneration,
    },
}

impl CatalogSnapshot {
    /// Produce logical catalog events relative to a retained snapshot. No WAL is
    /// written and no graph data is encoded by this metadata-only operation.
    #[must_use]
    pub fn logical_changes_from(&self, previous: &Self) -> Vec<CatalogLogicalChange> {
        let mut changes = Vec::new();
        for old in previous.descriptors() {
            if self.descriptor(old.id()).is_none() {
                changes.push(CatalogLogicalChange::Dropped {
                    id: old.id(),
                    generation: old.generation(),
                });
            }
        }
        for descriptor in self.descriptors() {
            match previous.descriptor(descriptor.id()) {
                None => changes.push(CatalogLogicalChange::Created(descriptor.clone())),
                Some(old) if old != descriptor => changes.push(CatalogLogicalChange::Replaced {
                    previous: old.generation(),
                    descriptor: descriptor.clone(),
                }),
                Some(_) => {}
            }
        }
        changes
    }
}
