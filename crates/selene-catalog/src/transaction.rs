//! Pure staging for one immutable catalog snapshot replacement.

use std::collections::{BTreeMap, btree_map::Entry};

use crate::{
    CatalogDescriptor, CatalogGeneration, CatalogId, CatalogObjectId, CatalogResult,
    CatalogSnapshot, CatalogSnapshotBuilder, DirectoryId,
};

/// A storage-neutral draft cloned from one immutable catalog snapshot.
///
/// This type does not serialize writers or publish state. The database-owned
/// lifecycle service performs those operations after it has staged matching
/// runtime instances.
pub struct CatalogTransaction {
    generation: CatalogGeneration,
    catalog_id: CatalogId,
    root_id: DirectoryId,
    descriptors: BTreeMap<CatalogObjectId, CatalogDescriptor>,
}

impl CatalogTransaction {
    /// Clone `base` into a draft for its checked next generation.
    ///
    /// # Errors
    ///
    /// Returns a generation-overflow error when `base` is at `u64::MAX`.
    pub fn new(base: &CatalogSnapshot) -> CatalogResult<Self> {
        Ok(Self {
            generation: base.generation().next()?,
            catalog_id: base.catalog_id(),
            root_id: base.root_directory_id(),
            descriptors: base
                .descriptors()
                .map(|descriptor| (descriptor.id(), descriptor.clone()))
                .collect(),
        })
    }

    /// Return the generation assigned to the completed draft.
    #[must_use]
    pub const fn generation(&self) -> CatalogGeneration {
        self.generation
    }

    /// Inspect the current detached logical records in stable ID order.
    pub fn descriptors(&self) -> impl Iterator<Item = &CatalogDescriptor> {
        self.descriptors.values()
    }

    /// Stage a new descriptor without replacing stable identity.
    ///
    /// # Errors
    ///
    /// Returns [`crate::CatalogError::DuplicateIdentifier`] when the draft
    /// already contains the descriptor's typed ID.
    pub fn insert(&mut self, descriptor: CatalogDescriptor) -> CatalogResult<()> {
        let id = descriptor.id();
        match self.descriptors.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert(descriptor);
                Ok(())
            }
            Entry::Occupied(_) => Err(crate::CatalogError::DuplicateIdentifier { id }),
        }
    }

    /// Stage removal of a descriptor by typed ID.
    pub fn remove(&mut self, id: CatalogObjectId) -> Option<CatalogDescriptor> {
        self.descriptors.remove(&id)
    }

    /// Remove an admitted owner and its own declarations. Foreign dependants
    /// remain and cause whole-draft validation to reject publication.
    pub fn remove_owner(&mut self, id: CatalogObjectId) {
        self.descriptors.retain(|key, descriptor| {
            *key != id
                && match descriptor.parent() {
                    crate::CatalogParent::Graph(owner) => id != CatalogObjectId::Graph(owner),
                    crate::CatalogParent::GraphType(owner) => {
                        id != CatalogObjectId::GraphType(owner)
                    }
                    _ => true,
                }
        });
    }

    /// Validate the complete draft and return its immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns the same structural, namespace, parent, generation, and payload
    /// errors as [`CatalogSnapshotBuilder::build`].
    pub fn build(mut self) -> CatalogResult<CatalogSnapshot> {
        let catalog = self
            .descriptors
            .remove(&CatalogObjectId::Catalog(self.catalog_id))
            .ok_or(crate::CatalogError::InvalidSyntheticRoot)?;
        let root = self
            .descriptors
            .remove(&CatalogObjectId::Directory(self.root_id))
            .ok_or(crate::CatalogError::InvalidSyntheticRoot)?;
        let mut builder = CatalogSnapshotBuilder::new(self.generation, catalog, root)?;
        for descriptor in self.descriptors.into_values() {
            builder.insert(descriptor)?;
        }
        builder.build()
    }
}
