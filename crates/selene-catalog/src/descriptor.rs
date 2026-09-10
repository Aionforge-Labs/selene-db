//! Immutable, storage-neutral catalog descriptors.

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    BindingTableId, CatalogError, CatalogGeneration, CatalogId, CatalogName, CatalogObjectId,
    CatalogObjectKind, CatalogResult, ConstraintDeclaration, ConstraintId, DeclarationMetadata,
    DirectoryId, GraphId, GraphTypeId, IndexDeclaration, IndexId, NativeDeclaration, ProcedureId,
    SchemaId,
};

/// Deterministic metadata recorded when a descriptor is created.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreationMetadata {
    generation: CatalogGeneration,
    principal: Option<String>,
}

impl CreationMetadata {
    /// Construct creation metadata without introducing a clock contract.
    #[must_use]
    pub const fn new(generation: CatalogGeneration, principal: Option<String>) -> Self {
        Self {
            generation,
            principal,
        }
    }

    /// Return the catalog generation in which the object was created.
    #[must_use]
    pub const fn generation(&self) -> CatalogGeneration {
        self.generation
    }

    /// Return the optional opaque creation principal.
    #[must_use]
    pub fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.principal.as_ref().map_or(0, String::capacity)
    }
}

/// Parent relationship carried by a catalog descriptor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogParent {
    /// The catalog ownership root has no parent.
    None,
    /// The synthetic directory belongs to the catalog.
    Catalog(CatalogId),
    /// A schema belongs to the synthetic root directory.
    Directory(DirectoryId),
    /// A primary catalog object belongs to a schema.
    Schema(SchemaId),
    /// A declaration belongs to one graph.
    Graph(GraphId),
    /// A declaration belongs to one graph type.
    GraphType(GraphTypeId),
}

impl CatalogParent {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::None => "no",
            Self::Catalog(_) => "catalog",
            Self::Directory(_) => "directory",
            Self::Schema(_) => "schema",
            Self::Graph(_) => "graph",
            Self::GraphType(_) => "graph_type",
        }
    }
}

/// Deliberately minimal, storage-neutral descriptor payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogPayload {
    /// Catalog marker.
    Catalog,
    /// Synthetic root-directory marker.
    RootDirectory,
    /// Schema marker.
    Schema,
    /// Graph references an optional catalog-owned constraining graph type.
    Graph {
        /// Optional constraining graph-type identity.
        graph_type: Option<GraphTypeId>,
    },
    /// Graph-type marker. Runtime definitions remain database-owned.
    GraphType,
    /// Binding-table marker.
    BindingTable,
    /// Declarative native registration, never a code pointer.
    Procedure(NativeDeclaration),
    /// An analyzed native index declaration, not its accelerator.
    Index(IndexDeclaration),
    /// A semantic constraint declaration, not caller-asserted enforcement proof.
    Constraint(ConstraintDeclaration),
}

impl CatalogPayload {
    /// Shared declaration metadata, when this payload is a registration.
    #[must_use]
    pub fn declaration_metadata(&self) -> Option<&DeclarationMetadata> {
        match self {
            Self::Index(index) => Some(&index.metadata),
            Self::Constraint(constraint) => Some(&constraint.metadata),
            Self::Procedure(native) => Some(&native.metadata),
            _ => None,
        }
    }

    /// Return the descriptor kind represented by this payload.
    #[must_use]
    pub const fn kind(&self) -> CatalogObjectKind {
        match self {
            Self::Catalog => CatalogObjectKind::Catalog,
            Self::RootDirectory => CatalogObjectKind::Directory,
            Self::Schema => CatalogObjectKind::Schema,
            Self::Graph { .. } => CatalogObjectKind::Graph,
            Self::GraphType => CatalogObjectKind::GraphType,
            Self::BindingTable => CatalogObjectKind::BindingTable,
            Self::Procedure(_) => CatalogObjectKind::Procedure,
            Self::Index(_) => CatalogObjectKind::Index,
            Self::Constraint(_) => CatalogObjectKind::Constraint,
        }
    }
}

/// Immutable normal form for one supported catalog object.
///
/// `PartialEq` compares every field, including typed identity, generation,
/// creation metadata, display spelling, and payload. Use [`Self::same_identity`]
/// when only stable object identity is relevant.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogDescriptor {
    id: CatalogObjectId,
    kind: CatalogObjectKind,
    name: CatalogName,
    parent: CatalogParent,
    generation: CatalogGeneration,
    creation: CreationMetadata,
    payload: CatalogPayload,
}

impl CatalogDescriptor {
    /// Construct and validate a descriptor normal form.
    ///
    /// # Errors
    ///
    /// Rejects kind/ID/payload disagreement, an invalid parent variant, misuse
    /// of the synthetic name, or creation metadata newer than the descriptor.
    pub fn new(
        id: CatalogObjectId,
        kind: CatalogObjectKind,
        name: CatalogName,
        parent: CatalogParent,
        generation: CatalogGeneration,
        creation: CreationMetadata,
        payload: CatalogPayload,
    ) -> CatalogResult<Self> {
        let descriptor = Self {
            id,
            kind,
            name,
            parent,
            generation,
            creation,
            payload,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Construct a catalog-root descriptor.
    pub fn catalog(
        id: CatalogId,
        name: CatalogName,
        generation: CatalogGeneration,
        creation: CreationMetadata,
    ) -> CatalogResult<Self> {
        Self::new(
            CatalogObjectId::Catalog(id),
            CatalogObjectKind::Catalog,
            name,
            CatalogParent::None,
            generation,
            creation,
            CatalogPayload::Catalog,
        )
    }

    /// Construct the one synthetic root-directory descriptor.
    pub fn root_directory(
        id: DirectoryId,
        catalog: CatalogId,
        generation: CatalogGeneration,
        creation: CreationMetadata,
    ) -> CatalogResult<Self> {
        Self::new(
            CatalogObjectId::Directory(id),
            CatalogObjectKind::Directory,
            CatalogName::synthetic_root(),
            CatalogParent::Catalog(catalog),
            generation,
            creation,
            CatalogPayload::RootDirectory,
        )
    }

    /// Construct a root-owned schema descriptor.
    pub fn schema(
        id: SchemaId,
        name: CatalogName,
        root: DirectoryId,
        generation: CatalogGeneration,
        creation: CreationMetadata,
    ) -> CatalogResult<Self> {
        Self::new(
            CatalogObjectId::Schema(id),
            CatalogObjectKind::Schema,
            name,
            CatalogParent::Directory(root),
            generation,
            creation,
            CatalogPayload::Schema,
        )
    }

    /// Construct a schema-owned graph descriptor.
    pub fn graph(
        id: GraphId,
        name: CatalogName,
        schema: SchemaId,
        generation: CatalogGeneration,
        creation: CreationMetadata,
        graph_type: Option<GraphTypeId>,
    ) -> CatalogResult<Self> {
        Self::new(
            CatalogObjectId::Graph(id),
            CatalogObjectKind::Graph,
            name,
            CatalogParent::Schema(schema),
            generation,
            creation,
            CatalogPayload::Graph { graph_type },
        )
    }

    /// Construct a schema-owned graph-type descriptor.
    pub fn graph_type(
        id: GraphTypeId,
        name: CatalogName,
        schema: SchemaId,
        generation: CatalogGeneration,
        creation: CreationMetadata,
    ) -> CatalogResult<Self> {
        Self::new(
            CatalogObjectId::GraphType(id),
            CatalogObjectKind::GraphType,
            name,
            CatalogParent::Schema(schema),
            generation,
            creation,
            CatalogPayload::GraphType,
        )
    }

    /// Construct a schema-owned binding-table descriptor marker.
    pub fn binding_table(
        id: BindingTableId,
        name: CatalogName,
        schema: SchemaId,
        generation: CatalogGeneration,
        creation: CreationMetadata,
    ) -> CatalogResult<Self> {
        Self::schema_object(
            CatalogObjectId::BindingTable(id),
            name,
            schema,
            generation,
            creation,
            CatalogPayload::BindingTable,
        )
    }

    /// Construct a native declaration under an engine, schema, or graph owner.
    pub fn procedure(
        id: ProcedureId,
        name: CatalogName,
        owner: CatalogParent,
        generation: CatalogGeneration,
        creation: CreationMetadata,
        declaration: NativeDeclaration,
    ) -> CatalogResult<Self> {
        Self::new(
            CatalogObjectId::Procedure(id),
            CatalogObjectKind::Procedure,
            name,
            owner,
            generation,
            creation,
            CatalogPayload::Procedure(declaration),
        )
    }

    /// Construct a graph/type-owned index declaration.
    pub fn index(
        id: IndexId,
        name: CatalogName,
        owner: CatalogParent,
        generation: CatalogGeneration,
        creation: CreationMetadata,
        declaration: IndexDeclaration,
    ) -> CatalogResult<Self> {
        Self::new(
            CatalogObjectId::Index(id),
            CatalogObjectKind::Index,
            name,
            owner,
            generation,
            creation,
            CatalogPayload::Index(declaration),
        )
    }

    /// Construct a graph/type-owned constraint declaration.
    pub fn constraint(
        id: ConstraintId,
        name: CatalogName,
        owner: CatalogParent,
        generation: CatalogGeneration,
        creation: CreationMetadata,
        declaration: ConstraintDeclaration,
    ) -> CatalogResult<Self> {
        Self::new(
            CatalogObjectId::Constraint(id),
            CatalogObjectKind::Constraint,
            name,
            owner,
            generation,
            creation,
            CatalogPayload::Constraint(declaration),
        )
    }

    fn schema_object(
        id: CatalogObjectId,
        name: CatalogName,
        schema: SchemaId,
        generation: CatalogGeneration,
        creation: CreationMetadata,
        payload: CatalogPayload,
    ) -> CatalogResult<Self> {
        Self::new(
            id,
            id.kind(),
            name,
            CatalogParent::Schema(schema),
            generation,
            creation,
            payload,
        )
    }

    /// Return the stable typed identity.
    #[must_use]
    pub const fn id(&self) -> CatalogObjectId {
        self.id
    }

    /// Return the descriptor kind.
    #[must_use]
    pub const fn kind(&self) -> CatalogObjectKind {
        self.kind
    }

    /// Return canonical and display naming metadata.
    #[must_use]
    pub const fn name(&self) -> &CatalogName {
        &self.name
    }

    /// Return the typed owner/parent relationship.
    #[must_use]
    pub const fn parent(&self) -> CatalogParent {
        self.parent
    }

    /// Return the generation of this descriptor revision.
    #[must_use]
    pub const fn generation(&self) -> CatalogGeneration {
        self.generation
    }

    /// Return deterministic creation metadata.
    #[must_use]
    pub const fn creation(&self) -> &CreationMetadata {
        &self.creation
    }

    /// Return the storage-neutral kind-specific payload.
    #[must_use]
    pub const fn payload(&self) -> &CatalogPayload {
        &self.payload
    }

    /// Return whether two descriptors identify the same catalog object.
    #[must_use]
    pub fn same_identity(&self, other: &Self) -> bool {
        self.id == other.id
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.name.heap_bytes() + self.creation.heap_bytes()
    }

    pub(crate) fn validate(&self) -> CatalogResult<()> {
        if self.kind != self.id.kind() || self.kind != self.payload.kind() {
            return Err(CatalogError::DescriptorKindMismatch {
                declared: self.kind,
                identifier: self.id.kind(),
                payload: self.payload.kind(),
            });
        }
        let valid_parent = matches!(
            (self.kind, self.parent),
            (CatalogObjectKind::Catalog, CatalogParent::None)
                | (CatalogObjectKind::Directory, CatalogParent::Catalog(_))
                | (CatalogObjectKind::Schema, CatalogParent::Directory(_))
                | (
                    CatalogObjectKind::Graph
                        | CatalogObjectKind::GraphType
                        | CatalogObjectKind::BindingTable
                        | CatalogObjectKind::Procedure,
                    CatalogParent::Schema(_)
                )
                | (
                    CatalogObjectKind::Index | CatalogObjectKind::Constraint,
                    CatalogParent::Graph(_) | CatalogParent::GraphType(_)
                )
                | (
                    CatalogObjectKind::Procedure,
                    CatalogParent::Catalog(_)
                        | CatalogParent::Graph(_)
                        | CatalogParent::GraphType(_)
                )
        );
        if !valid_parent {
            return Err(CatalogError::InvalidParentKind {
                object: self.kind,
                parent: self.parent.name(),
            });
        }
        if self.kind == CatalogObjectKind::Directory {
            if !self.name.is_synthetic_root() {
                return Err(CatalogError::SyntheticRootNameRequired);
            }
        } else if self.name.is_synthetic_root() {
            return Err(CatalogError::UserNameRequired { kind: self.kind });
        }
        if self.creation.generation() > self.generation {
            return Err(CatalogError::CreationGenerationAfterDescriptor {
                creation: self.creation.generation(),
                descriptor: self.generation,
            });
        }
        match &self.payload {
            CatalogPayload::Index(index) => index.validate()?,
            CatalogPayload::Constraint(constraint) => constraint.validate()?,
            CatalogPayload::Procedure(native) => native.validate()?,
            _ => {}
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for CatalogDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireDescriptor {
            id: CatalogObjectId,
            kind: CatalogObjectKind,
            name: CatalogName,
            parent: CatalogParent,
            generation: CatalogGeneration,
            creation: CreationMetadata,
            payload: CatalogPayload,
        }

        let wire = WireDescriptor::deserialize(deserializer)?;
        Self::new(
            wire.id,
            wire.kind,
            wire.name,
            wire.parent,
            wire.generation,
            wire.creation,
            wire.payload,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl PartialEq for CatalogDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && self.name.metadata_eq(&other.name)
            && self.parent == other.parent
            && self.generation == other.generation
            && self.creation == other.creation
            && self.payload == other.payload
    }
}

impl Eq for CatalogDescriptor {}
