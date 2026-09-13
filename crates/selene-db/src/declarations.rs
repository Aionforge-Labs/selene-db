//! Facade declaration inspection and graph-owned registration administration.

mod constraints;
mod expression;

use selene_catalog::{
    CatalogDescriptor, CatalogObjectId, CatalogObjectKind, CatalogParent, CatalogPayload,
    CatalogTransaction, CreationMetadata,
};

use crate::{
    Catalog, CatalogGeneration, CatalogReadSnapshot, CreateOutcome, CreatePolicy, DropOutcome,
    DropPolicy, Error, GraphId, GraphTypeId, ObjectPath, PathSegment, Result,
    catalog::{duplicate_outcome, missing_outcome},
    catalog_snapshot::{find_object, next_id},
    transaction::{DatabaseDraft, require_committed},
};

/// Stable logical dependency identity; this carries no runtime handle.
pub use selene_catalog::CatalogObjectId as DeclarationReference;
/// Validated logical catalog input types for the future complete-transaction codec.
pub use selene_catalog::{CatalogLogicalChange, CatalogLogicalRecords};
/// Logical declaration data intentionally shared with the storage-neutral catalog.
/// These types contain no lower runtime handles, graph rows, or callable code.
pub use selene_catalog::{
    ConstraintDeclaration, ConstraintKind, DeclarationDependency, DeclarationMetadata,
    DeclarationProfile, DeclarationState, ElementKind, IndexConfiguration, IndexDeclaration,
    IndexJsonSelector, NativeBinding, NativeCandidateState, NativeDeclaration, NativeDefault,
    NativeEffect, NativeField, NativeParameter, NativeProcedure, NativeProjection, NativeType,
    PropertyTarget, ReservedIndexExpression,
};
/// Bounded expression descriptors shared with durable index declarations.
pub use selene_core::scalar_index_expression::{
    ScalarIndexExpression, ScalarIndexOperation, ScalarIndexSelector,
};
/// Declarative index configuration types intentionally available without an
/// application dependency on a lower engine crate.
pub use selene_core::{
    HnswIndexConfig, IvfIndexConfig, SchemaPropertyIndexKind as ScalarIndexKind,
    SchemaVectorIndexKind as VectorIndexKind,
};

/// Typed identity of a logical registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclarationId {
    /// Native index identity.
    Index(u64),
    /// Constraint identity.
    Constraint(u64),
    /// Native registration identity.
    Native(u64),
}

/// Stable owner of a declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclarationOwner {
    /// Static engine inventory, outside user schema namespaces.
    Engine,
    /// Graph-local registration.
    Graph(GraphId),
    /// Graph-type-local registration.
    GraphType(GraphTypeId),
}

/// Storage-neutral declaration definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeclarationDefinition {
    /// Analyzed native index.
    Index(IndexDeclaration),
    /// Semantic constraint.
    Constraint(ConstraintDeclaration),
    /// Symbolic native/provider/projection registration.
    Native(NativeDeclaration),
}

impl DeclarationDefinition {
    fn into_payload(self) -> CatalogPayload {
        match self {
            Self::Index(value) => CatalogPayload::Index(value),
            Self::Constraint(value) => CatalogPayload::Constraint(value),
            Self::Native(value) => CatalogPayload::Procedure(value),
        }
    }
}

fn candidate_state(payload: &CatalogPayload) -> bool {
    matches!(
        payload,
        CatalogPayload::Procedure(NativeDeclaration {
            binding: NativeBinding::CandidateState(_),
            ..
        })
    )
}

/// Immutable logical declaration summary. Ready is durable admission state, not
/// current accelerator completeness; data updates can invalidate a physical probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclarationDescriptor {
    /// Stable registration identity.
    pub id: DeclarationId,
    /// Stable owner.
    pub owner: DeclarationOwner,
    /// Analyzed catalog name with retained display form.
    pub name: PathSegment,
    /// Descriptor revision.
    pub generation: CatalogGeneration,
    /// Logical data, without derived accelerator payloads.
    pub definition: DeclarationDefinition,
}

impl DeclarationDescriptor {
    /// Pin this declaration's identity and revision as a shared dependency.
    /// This is metadata, not permission to activate a runtime implementation.
    pub fn dependency(&self) -> Result<DeclarationDependency> {
        let id = match self.id {
            DeclarationId::Index(raw) => CatalogObjectId::Index(
                selene_catalog::IndexId::new(raw).map_err(Error::from_catalog_invariant)?,
            ),
            DeclarationId::Constraint(raw) => CatalogObjectId::Constraint(
                selene_catalog::ConstraintId::new(raw).map_err(Error::from_catalog_invariant)?,
            ),
            DeclarationId::Native(raw) => CatalogObjectId::Procedure(
                selene_catalog::ProcedureId::new(raw).map_err(Error::from_catalog_invariant)?,
            ),
        };
        Ok(DeclarationDependency {
            id,
            generation: selene_catalog::CatalogGeneration::new(self.generation.get())
                .map_err(Error::from_catalog_invariant)?,
        })
    }
}

fn summary(descriptor: &CatalogDescriptor) -> Option<DeclarationDescriptor> {
    let (id, definition) = match (descriptor.id(), descriptor.payload()) {
        (CatalogObjectId::Index(id), CatalogPayload::Index(value)) => (
            DeclarationId::Index(id.get()),
            DeclarationDefinition::Index(value.clone()),
        ),
        (CatalogObjectId::Constraint(id), CatalogPayload::Constraint(value)) => (
            DeclarationId::Constraint(id.get()),
            DeclarationDefinition::Constraint(value.clone()),
        ),
        (CatalogObjectId::Procedure(id), CatalogPayload::Procedure(value)) => (
            DeclarationId::Native(id.get()),
            DeclarationDefinition::Native(value.clone()),
        ),
        _ => return None,
    };
    let owner = match descriptor.parent() {
        CatalogParent::Graph(id) => DeclarationOwner::Graph(GraphId(id.get())),
        CatalogParent::GraphType(id) => DeclarationOwner::GraphType(GraphTypeId(id.get())),
        CatalogParent::Catalog(_) => DeclarationOwner::Engine,
        _ => return None,
    };
    Some(DeclarationDescriptor {
        id,
        owner,
        definition,
        name: PathSegment(descriptor.name().clone()),
        generation: CatalogGeneration::from_lower(descriptor.generation()),
    })
}

impl CatalogReadSnapshot {
    /// Export logical descriptors and allocation bounds, without graph data,
    /// physical rows, provider handles, or a claim of filesystem durability.
    pub fn logical_catalog(&self) -> Result<CatalogLogicalRecords> {
        let water = self.state.high_water;
        let high_water = std::collections::BTreeMap::from([
            (
                CatalogObjectKind::Catalog,
                self.state.catalog.catalog_id().get(),
            ),
            (
                CatalogObjectKind::Directory,
                self.state.catalog.root_directory_id().get(),
            ),
            (CatalogObjectKind::Schema, water.schema),
            (CatalogObjectKind::Graph, water.graph),
            (CatalogObjectKind::GraphType, water.graph_type),
            (CatalogObjectKind::Index, water.index),
            (CatalogObjectKind::Constraint, water.constraint),
            (CatalogObjectKind::Procedure, water.procedure),
        ]);
        CatalogLogicalRecords::new(
            self.state.catalog.generation(),
            high_water,
            self.state.catalog.descriptors().cloned().collect(),
        )
        .map_err(Error::from_catalog_invariant)
    }

    /// Logical catalog-only changes between retained outer publications.
    #[must_use]
    pub fn logical_catalog_changes_from(&self, previous: &Self) -> Vec<CatalogLogicalChange> {
        self.state
            .catalog
            .logical_changes_from(&previous.state.catalog)
    }

    /// Inspect graph/type declarations in owner-local canonical name order.
    pub fn declarations(&self, owner: &ObjectPath) -> Result<Vec<DeclarationDescriptor>> {
        let descriptor = find_object(&self.state, owner)?
            .ok_or_else(|| Error::not_found(owner, "declaration owner"))?;
        owner_parent(descriptor, owner)?;
        Ok(self
            .state
            .catalog
            .declarations(descriptor.id())
            .filter_map(summary)
            .collect())
    }

    /// Inspect the frozen known-code native inventory, without creating user schemas.
    #[must_use]
    pub fn native_procedures(&self) -> Vec<DeclarationDescriptor> {
        self.state
            .catalog
            .declarations(CatalogObjectId::Catalog(self.state.catalog.catalog_id()))
            .filter_map(summary)
            .collect()
    }
}

impl Catalog {
    /// Declare a registration, rebuilding graph-owned candidate states atomically.
    ///
    /// Existing supported index CALLs build and admit ready declarations through
    /// the graph mutation funnel. Ready candidate-state declarations are rebuilt
    /// from authoritative values before publication, including replacement. Other
    /// ready declarations cannot be asserted by callers. This API cannot replace
    /// an active constraint with metadata that disables its enforcement.
    pub fn declare(
        &self,
        owner: &ObjectPath,
        name: &PathSegment,
        definition: DeclarationDefinition,
        policy: CreatePolicy,
    ) -> Result<CreateOutcome<DeclarationDescriptor>> {
        let payload = definition.into_payload();
        if payload
            .declaration_metadata()
            .is_some_and(|metadata| metadata.state == DeclarationState::Ready)
            && !candidate_state(&payload)
        {
            return Err(declaration_error("caller_asserted_activation"));
        }
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let owner_descriptor = find_object(&base, owner)?
                .ok_or_else(|| Error::not_found(owner, "declaration owner"))?;
            let parent = owner_parent(owner_descriptor, owner)?;
            if candidate_state(&payload)
                && payload
                    .declaration_metadata()
                    .is_some_and(|m| m.state == DeclarationState::Ready)
                && !matches!(parent, CatalogParent::Graph(_))
            {
                return Err(declaration_error("candidate_state_requires_graph_owner"));
            }
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let existing = base.catalog.declaration(owner_descriptor.id(), &name.0);
            if let Some(existing) = existing {
                if existing.kind() != payload.kind() {
                    return Err(Error::wrong_kind(
                        name,
                        &payload.kind().to_string(),
                        existing.kind(),
                    ));
                }
                let existing_summary = summary(existing)
                    .ok_or_else(|| Error::catalog_invariant("invalid declaration summary"))?;
                if policy != CreatePolicy::OrReplace {
                    return duplicate_outcome(policy, existing_summary, name, "declaration");
                }
                if existing
                    .payload()
                    .declaration_metadata()
                    .is_some_and(|metadata| metadata.state == DeclarationState::Ready)
                    && !candidate_state(existing.payload())
                {
                    return Err(declaration_error(
                        "active_declaration_requires_runtime_mutation",
                    ));
                }
            }
            let mut transaction =
                CatalogTransaction::new(&draft.catalog).map_err(Error::from_catalog_invariant)?;
            if let Some(existing) = existing {
                transaction.remove(existing.id());
            }
            let watermark = match payload.kind() {
                CatalogObjectKind::Index => &mut draft.high_water.index,
                CatalogObjectKind::Constraint => &mut draft.high_water.constraint,
                CatalogObjectKind::Procedure => &mut draft.high_water.procedure,
                _ => unreachable!("declaration payload"),
            };
            let raw = next_id(*watermark, "declaration")?;
            let id = match payload.kind() {
                CatalogObjectKind::Index => CatalogObjectId::Index(
                    selene_catalog::IndexId::new(raw).map_err(Error::from_catalog_invariant)?,
                ),
                CatalogObjectKind::Constraint => CatalogObjectId::Constraint(
                    selene_catalog::ConstraintId::new(raw)
                        .map_err(Error::from_catalog_invariant)?,
                ),
                CatalogObjectKind::Procedure => CatalogObjectId::Procedure(
                    selene_catalog::ProcedureId::new(raw).map_err(Error::from_catalog_invariant)?,
                ),
                _ => unreachable!("declaration payload"),
            };
            let descriptor = CatalogDescriptor::new(
                id,
                payload.kind(),
                name.0.clone(),
                parent,
                transaction.generation(),
                CreationMetadata::new(transaction.generation(), None),
                payload,
            )
            .map_err(Error::from_catalog_invariant)?;
            let created = summary(&descriptor)
                .ok_or_else(|| Error::catalog_invariant("invalid declaration summary"))?;
            transaction
                .insert(descriptor)
                .map_err(Error::from_catalog_invariant)?;
            draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
            *watermark = raw;
            stage_owner_binding(&mut draft, &base, parent)?;
            draft.mark_modified();
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(match existing.and_then(summary) {
                Some(dropped) => CreateOutcome::Replaced { dropped, created },
                None => CreateOutcome::Created(created),
            })
        })
    }

    /// Remove an inactive declaration, expression index or candidate state with dependency RESTRICT.
    /// Other active index removal remains in the supported mutation CALLs; active
    /// uniqueness cannot be disabled by dropping descriptive metadata.
    pub fn drop_declaration(
        &self,
        owner: &ObjectPath,
        name: &PathSegment,
        policy: DropPolicy,
    ) -> Result<DropOutcome<DeclarationDescriptor>> {
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let owner_descriptor = find_object(&base, owner)?
                .ok_or_else(|| Error::not_found(owner, "declaration owner"))?;
            let parent = owner_parent(owner_descriptor, owner)?;
            let Some(existing) = base.catalog.declaration(owner_descriptor.id(), &name.0) else {
                return missing_outcome(policy, name, "declaration");
            };
            if existing
                .payload()
                .declaration_metadata()
                .is_some_and(|metadata| metadata.state == DeclarationState::Ready)
                && !candidate_state(existing.payload())
                && !matches!(existing.payload(), CatalogPayload::Index(index) if matches!(index.configuration, IndexConfiguration::Expression { .. }))
            {
                return Err(declaration_error(
                    "active_declaration_requires_runtime_mutation",
                ));
            }
            let dropped = summary(existing)
                .ok_or_else(|| Error::catalog_invariant("invalid declaration summary"))?;
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let mut transaction =
                CatalogTransaction::new(&draft.catalog).map_err(Error::from_catalog_invariant)?;
            transaction.remove(existing.id());
            draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
            stage_owner_binding(&mut draft, &base, parent)?;
            draft.mark_modified();
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(DropOutcome::Dropped(dropped))
        })
    }
}

fn owner_parent(descriptor: &CatalogDescriptor, path: &ObjectPath) -> Result<CatalogParent> {
    match descriptor.id() {
        CatalogObjectId::Graph(id) => Ok(CatalogParent::Graph(id)),
        CatalogObjectId::GraphType(id) => Ok(CatalogParent::GraphType(id)),
        _ => Err(Error::wrong_kind(
            path,
            "graph or graph type",
            descriptor.kind(),
        )),
    }
}

fn stage_owner_binding(
    draft: &mut DatabaseDraft,
    base: &crate::database::DatabaseState,
    parent: CatalogParent,
) -> Result<()> {
    if let CatalogParent::Graph(id) = parent {
        let mut graph = base
            .graphs
            .get(&id)
            .ok_or_else(|| Error::catalog_invariant("missing owner graph"))?
            .graph
            .read()
            .as_ref()
            .clone();
        graph.meta.generation = graph
            .meta
            .generation
            .checked_add(1)
            .ok_or_else(|| declaration_error("graph_generation_exhausted"))?;
        graph
            .bind_catalog(&draft.catalog)
            .map_err(Error::from_catalog_invariant)?;
        draft.replace_graph(id, graph)?;
    }
    Ok(())
}

fn declaration_error(reason: &'static str) -> Error {
    Error::from_catalog_invariant(selene_catalog::CatalogError::InvalidDeclaration { reason })
}
