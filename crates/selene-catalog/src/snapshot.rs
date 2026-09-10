//! Validated immutable catalog read snapshots.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    mem::size_of,
    sync::Arc,
};

use crate::{
    CatalogDescriptor, CatalogError, CatalogGeneration, CatalogId, CatalogName, CatalogObjectId,
    CatalogObjectKind, CatalogParent, CatalogPayload, CatalogResult, CreationMetadata, DirectoryId,
    GraphTypeId, SchemaId,
};

/// One-shot constructor for a validated immutable catalog snapshot.
///
/// This is a construction boundary, not a writer transaction or lifecycle API.
/// M02-PR03 owns serialized mutation and publication.
pub struct CatalogSnapshotBuilder {
    generation: CatalogGeneration,
    catalog_id: CatalogId,
    root_id: DirectoryId,
    descriptors: BTreeMap<CatalogObjectId, CatalogDescriptor>,
}

impl CatalogSnapshotBuilder {
    /// Seed a snapshot with its one catalog and one synthetic root directory.
    ///
    /// # Errors
    ///
    /// Returns a descriptor or root-relationship error when the seeds are not
    /// the required catalog/root pair.
    pub fn new(
        generation: CatalogGeneration,
        catalog: CatalogDescriptor,
        root: CatalogDescriptor,
    ) -> CatalogResult<Self> {
        catalog.validate()?;
        root.validate()?;
        let CatalogObjectId::Catalog(catalog_id) = catalog.id() else {
            return Err(CatalogError::InvalidSyntheticRoot);
        };
        let CatalogObjectId::Directory(root_id) = root.id() else {
            return Err(CatalogError::InvalidSyntheticRoot);
        };
        if root.parent() != CatalogParent::Catalog(catalog_id) {
            return Err(CatalogError::InvalidSyntheticRoot);
        }
        let mut builder = Self {
            generation,
            catalog_id,
            root_id,
            descriptors: BTreeMap::new(),
        };
        builder.insert(catalog)?;
        builder.insert(root)?;
        Ok(builder)
    }

    /// Insert one already-constructed descriptor into this one-shot builder.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::DuplicateIdentifier`] for a repeated typed ID.
    pub fn insert(&mut self, descriptor: CatalogDescriptor) -> CatalogResult<()> {
        descriptor.validate()?;
        let id = descriptor.id();
        match self.descriptors.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert(descriptor);
            }
            Entry::Occupied(_) => return Err(CatalogError::DuplicateIdentifier { id }),
        }
        Ok(())
    }

    /// Reject a child-directory insertion under the selected depth-zero profile.
    ///
    /// The arguments make the rejected operation explicit without constructing
    /// an invalid directory descriptor.
    ///
    /// # Errors
    ///
    /// Always returns [`CatalogError::UnsupportedDirectoryDepth`] with maximum
    /// depth zero.
    pub fn insert_child_directory(
        &mut self,
        _id: DirectoryId,
        _parent: DirectoryId,
        _name: CatalogName,
        _creation: CreationMetadata,
    ) -> CatalogResult<()> {
        Err(CatalogError::UnsupportedDirectoryDepth { maximum_depth: 0 })
    }

    /// Validate all relationships and finish the immutable snapshot.
    ///
    /// # Errors
    ///
    /// Rejects invalid root shape, generations, parents, duplicate canonical
    /// names, and missing or cross-schema payload references.
    pub fn build(self) -> CatalogResult<CatalogSnapshot> {
        let state = self.validate_and_index()?;
        Ok(CatalogSnapshot {
            state: Arc::new(state),
        })
    }

    fn validate_and_index(self) -> CatalogResult<CatalogState> {
        let catalogs = self
            .descriptors
            .values()
            .filter(|descriptor| descriptor.kind() == CatalogObjectKind::Catalog)
            .count();
        let directories = self
            .descriptors
            .values()
            .filter(|descriptor| descriptor.kind() == CatalogObjectKind::Directory)
            .count();
        if catalogs != 1 || directories != 1 {
            return Err(CatalogError::InvalidRootCardinality {
                catalogs,
                directories,
            });
        }
        let catalog_key = CatalogObjectId::Catalog(self.catalog_id);
        let root_key = CatalogObjectId::Directory(self.root_id);
        let Some(catalog) = self.descriptors.get(&catalog_key) else {
            return Err(CatalogError::InvalidSyntheticRoot);
        };
        let Some(root) = self.descriptors.get(&root_key) else {
            return Err(CatalogError::InvalidSyntheticRoot);
        };
        if catalog.parent() != CatalogParent::None
            || root.parent() != CatalogParent::Catalog(self.catalog_id)
            || !root.name().is_synthetic_root()
        {
            return Err(CatalogError::InvalidSyntheticRoot);
        }

        let mut schema_names = BTreeMap::new();
        let mut object_names: BTreeMap<SchemaId, BTreeMap<CatalogName, CatalogObjectId>> =
            BTreeMap::new();
        let mut declaration_names: BTreeMap<
            CatalogObjectId,
            BTreeMap<CatalogName, CatalogObjectId>,
        > = BTreeMap::new();
        for descriptor in self.descriptors.values() {
            if descriptor.generation() > self.generation {
                return Err(CatalogError::DescriptorGenerationAfterSnapshot {
                    object: descriptor.id(),
                    descriptor: descriptor.generation(),
                    snapshot: self.generation,
                });
            }
            match (descriptor.kind(), descriptor.parent()) {
                (CatalogObjectKind::Catalog, CatalogParent::None)
                | (CatalogObjectKind::Directory, CatalogParent::Catalog(_)) => {}
                (CatalogObjectKind::Schema, CatalogParent::Directory(parent)) => {
                    if parent != self.root_id {
                        return Err(CatalogError::MissingParent {
                            object: descriptor.id(),
                            parent: CatalogObjectId::Directory(parent),
                        });
                    }
                    let CatalogObjectId::Schema(schema_id) = descriptor.id() else {
                        unreachable!("descriptor kind validation ran")
                    };
                    if let Some(existing) =
                        schema_names.insert(descriptor.name().clone(), schema_id)
                    {
                        return Err(CatalogError::DuplicateCanonicalName {
                            existing: CatalogObjectId::Schema(existing),
                            incoming: descriptor.id(),
                            canonical: descriptor.name().canonical().to_owned(),
                        });
                    }
                    object_names.entry(schema_id).or_default();
                }
                (_, CatalogParent::Schema(schema_id)) => {
                    let parent = CatalogObjectId::Schema(schema_id);
                    if !self.descriptors.contains_key(&parent) {
                        return Err(CatalogError::MissingParent {
                            object: descriptor.id(),
                            parent,
                        });
                    }
                    let names = object_names.entry(schema_id).or_default();
                    if let Some(existing) = names.insert(descriptor.name().clone(), descriptor.id())
                    {
                        return Err(CatalogError::DuplicateCanonicalName {
                            existing,
                            incoming: descriptor.id(),
                            canonical: descriptor.name().canonical().to_owned(),
                        });
                    }
                }
                (_, CatalogParent::Graph(id)) => {
                    index_declaration_name(
                        &self.descriptors,
                        &mut declaration_names,
                        CatalogObjectId::Graph(id),
                        descriptor,
                    )?;
                }
                (CatalogObjectKind::Procedure, CatalogParent::Catalog(id)) => {
                    index_declaration_name(
                        &self.descriptors,
                        &mut declaration_names,
                        CatalogObjectId::Catalog(id),
                        descriptor,
                    )?;
                }
                (_, CatalogParent::GraphType(id)) => {
                    index_declaration_name(
                        &self.descriptors,
                        &mut declaration_names,
                        CatalogObjectId::GraphType(id),
                        descriptor,
                    )?;
                }
                _ => unreachable!("descriptor parent validation ran"),
            }
        }

        for descriptor in self.descriptors.values() {
            let CatalogPayload::Graph {
                graph_type: Some(graph_type),
            } = descriptor.payload()
            else {
                continue;
            };
            validate_graph_type_reference(&self.descriptors, descriptor, *graph_type)?;
        }

        crate::dependencies::validate(&self.descriptors)?;
        Ok(CatalogState {
            generation: self.generation,
            catalog_id: self.catalog_id,
            root_id: self.root_id,
            descriptors: self.descriptors,
            schema_names,
            object_names,
            declaration_names,
        })
    }
}

fn validate_graph_type_reference(
    descriptors: &BTreeMap<CatalogObjectId, CatalogDescriptor>,
    graph: &CatalogDescriptor,
    graph_type: GraphTypeId,
) -> CatalogResult<()> {
    let target = CatalogObjectId::GraphType(graph_type);
    let Some(graph_type_descriptor) = descriptors.get(&target) else {
        return Err(CatalogError::MissingPayloadReference {
            object: graph.id(),
            target,
        });
    };
    if graph.parent() != graph_type_descriptor.parent() {
        return Err(CatalogError::CrossSchemaPayloadReference {
            object: graph.id(),
            target,
        });
    }
    Ok(())
}

struct CatalogState {
    generation: CatalogGeneration,
    catalog_id: CatalogId,
    root_id: DirectoryId,
    descriptors: BTreeMap<CatalogObjectId, CatalogDescriptor>,
    schema_names: BTreeMap<CatalogName, SchemaId>,
    object_names: BTreeMap<SchemaId, BTreeMap<CatalogName, CatalogObjectId>>,
    declaration_names: BTreeMap<CatalogObjectId, BTreeMap<CatalogName, CatalogObjectId>>,
}

/// Immutable generation-bound catalog state for lock-free readers.
///
/// Cloning a snapshot clones one `Arc`; descriptors and dictionaries remain
/// shared and immutable.
#[derive(Clone)]
pub struct CatalogSnapshot {
    state: Arc<CatalogState>,
}

impl CatalogSnapshot {
    /// Resolve a declaration in its owner's namespace, independent of primary schema names.
    #[must_use]
    pub fn declaration(
        &self,
        owner: CatalogObjectId,
        name: &CatalogName,
    ) -> Option<&CatalogDescriptor> {
        self.descriptor(*self.state.declaration_names.get(&owner)?.get(name)?)
    }

    /// Iterate only the declarations owned by a graph or graph type.
    pub fn declarations(&self, owner: CatalogObjectId) -> impl Iterator<Item = &CatalogDescriptor> {
        self.state
            .declaration_names
            .get(&owner)
            .into_iter()
            .flat_map(|names| names.values())
            .filter_map(|id| self.descriptor(*id))
    }

    /// Return the generation that bounds every descriptor in this snapshot.
    #[must_use]
    pub fn generation(&self) -> CatalogGeneration {
        self.state.generation
    }

    /// Return the one catalog identity.
    #[must_use]
    pub fn catalog_id(&self) -> CatalogId {
        self.state.catalog_id
    }

    /// Return the synthetic root-directory identity.
    #[must_use]
    pub fn root_directory_id(&self) -> DirectoryId {
        self.state.root_id
    }

    /// Look up a descriptor by typed stable identity.
    #[must_use]
    pub fn descriptor(&self, id: CatalogObjectId) -> Option<&CatalogDescriptor> {
        self.state.descriptors.get(&id)
    }

    /// Look up a root-owned schema by canonical name.
    #[must_use]
    pub fn schema(&self, name: &CatalogName) -> Option<&CatalogDescriptor> {
        let id = self.state.schema_names.get(name)?;
        self.descriptor(CatalogObjectId::Schema(*id))
    }

    /// Look up a primary object in a schema's shared canonical-name namespace.
    #[must_use]
    pub fn schema_object(
        &self,
        schema: SchemaId,
        name: &CatalogName,
    ) -> Option<&CatalogDescriptor> {
        let id = self.state.object_names.get(&schema)?.get(name)?;
        self.descriptor(*id)
    }

    /// Iterate all descriptors in deterministic kind-and-ID order.
    pub fn descriptors(&self) -> impl Iterator<Item = &CatalogDescriptor> {
        self.state.descriptors.values()
    }

    /// Iterate root schemas in canonical Unicode-scalar order.
    pub fn schemas(&self) -> impl Iterator<Item = &CatalogDescriptor> {
        self.state
            .schema_names
            .values()
            .filter_map(|id| self.descriptor(CatalogObjectId::Schema(*id)))
    }

    /// Iterate one schema's primary objects in shared canonical-name order.
    #[must_use]
    pub fn schema_objects(
        &self,
        schema: SchemaId,
    ) -> Option<impl Iterator<Item = &CatalogDescriptor>> {
        Some(
            self.state
                .object_names
                .get(&schema)?
                .values()
                .filter_map(|id| self.descriptor(*id)),
        )
    }

    /// Return whether two handles share the same immutable state allocation.
    #[must_use]
    pub fn shares_state_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Return reproducible lower-bound structural memory accounting.
    ///
    /// The result includes inline key/value sizes and owned string capacities.
    /// It excludes declaration payload heap allocations, allocator metadata,
    /// `BTreeMap` node slack, and `Arc` control blocks. In particular this is not
    /// total retained-memory accounting for native signatures/configurations.
    #[must_use]
    pub fn memory_accounting(&self) -> CatalogMemoryAccounting {
        let descriptor_bytes = self
            .state
            .descriptors
            .values()
            .map(|descriptor| {
                size_of::<CatalogObjectId>()
                    + size_of::<CatalogDescriptor>()
                    + descriptor.heap_bytes()
            })
            .sum();
        let schema_bytes = self
            .state
            .schema_names
            .keys()
            .map(|name| size_of::<CatalogName>() + size_of::<SchemaId>() + name.heap_bytes())
            .sum::<usize>();
        let object_entry_count = self
            .state
            .object_names
            .values()
            .map(BTreeMap::len)
            .sum::<usize>()
            + self
                .state
                .declaration_names
                .values()
                .map(BTreeMap::len)
                .sum::<usize>();
        let object_bytes = self
            .state
            .object_names
            .values()
            .chain(self.state.declaration_names.values())
            .flat_map(BTreeMap::keys)
            .map(|name| size_of::<CatalogName>() + size_of::<CatalogObjectId>() + name.heap_bytes())
            .sum::<usize>();
        CatalogMemoryAccounting {
            descriptor_count: self.state.descriptors.len(),
            descriptor_bytes,
            dictionary_entry_count: self.state.schema_names.len() + object_entry_count,
            dictionary_bytes: schema_bytes + object_bytes,
        }
    }
}

fn index_declaration_name(
    descriptors: &BTreeMap<CatalogObjectId, CatalogDescriptor>,
    names: &mut BTreeMap<CatalogObjectId, BTreeMap<CatalogName, CatalogObjectId>>,
    owner: CatalogObjectId,
    descriptor: &CatalogDescriptor,
) -> CatalogResult<()> {
    if !descriptors.contains_key(&owner) {
        return Err(CatalogError::MissingParent {
            object: descriptor.id(),
            parent: owner,
        });
    }
    if let Some(existing) = names
        .entry(owner)
        .or_default()
        .insert(descriptor.name().clone(), descriptor.id())
    {
        return Err(CatalogError::DuplicateCanonicalName {
            existing,
            incoming: descriptor.id(),
            canonical: descriptor.name().canonical().to_owned(),
        });
    }
    Ok(())
}

/// Reproducible lower-bound memory accounting for a catalog snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogMemoryAccounting {
    descriptor_count: usize,
    descriptor_bytes: usize,
    dictionary_entry_count: usize,
    dictionary_bytes: usize,
}

impl CatalogMemoryAccounting {
    /// Return the number of descriptors included in the accounting.
    #[must_use]
    pub const fn descriptor_count(self) -> usize {
        self.descriptor_count
    }

    /// Return accounted inline and string-capacity descriptor bytes.
    #[must_use]
    pub const fn descriptor_bytes(self) -> usize {
        self.descriptor_bytes
    }

    /// Return root-schema plus schema-object dictionary entry count.
    #[must_use]
    pub const fn dictionary_entry_count(self) -> usize {
        self.dictionary_entry_count
    }

    /// Return accounted inline and key-string-capacity dictionary bytes.
    #[must_use]
    pub const fn dictionary_bytes(self) -> usize {
        self.dictionary_bytes
    }
}
