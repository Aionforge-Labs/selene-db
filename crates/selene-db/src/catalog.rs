//! Database-owned catalog lifecycle service and immutable read snapshots.

use std::sync::Arc;

use selene_catalog::GraphId as LowerGraphId;
use selene_core::GraphId as CoreGraphId;

use crate::{
    CatalogReadSnapshot, Error, GraphDescriptor, GraphTypeDefinition, GraphTypeDescriptor,
    ObjectPath, Result, SchemaDescriptor, SchemaPath,
    database::{DatabaseInner, GraphInstance},
    transaction::{DatabaseDraft, require_committed},
};

/// Duplicate handling for a create operation.
///
/// This enum grows as ISO catalog statements are implemented; match with a
/// wildcard arm when only some policies matter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CreatePolicy {
    /// Report an existing object as an error.
    #[default]
    Strict,
    /// Return [`CreateOutcome::AlreadyExists`] for the requested kind only.
    IfNotExists,
    /// Drop an existing object of the requested kind and create the new one in
    /// the same publication (ISO/IEC 39075:2024 sections 12.4 and 12.6).
    ///
    /// [`Catalog::create_graph`] applies the full graph-drop admission.
    /// [`Catalog::create_graph_type`] applies `DROP GRAPH TYPE` RESTRICT and
    /// refuses a referenced type. `CREATE SCHEMA` has no `OR REPLACE` form.
    OrReplace,
}

/// Missing-object handling for a drop operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DropPolicy {
    /// Report an absent object as an error.
    #[default]
    Strict,
    /// Return [`DropOutcome::NotFound`] without publishing state.
    IfExists,
}

/// Explicit result of a create request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CreateOutcome<T> {
    /// A new object was published.
    Created(T),
    /// The same requested kind already existed at the canonical path.
    AlreadyExists(T),
    /// [`CreatePolicy::OrReplace`] dropped the existing object and published
    /// the new one in a single state swap. The two descriptors never share an
    /// identity.
    Replaced {
        /// The descriptor removed by the replacement.
        dropped: T,
        /// The descriptor published in its place.
        created: T,
    },
}

/// Explicit result of a drop request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DropOutcome<T> {
    /// The object was removed from the published state.
    Dropped(T),
    /// `IF EXISTS` found no object at the path.
    NotFound,
}

/// Shared handle to the database catalog service.
#[derive(Clone)]
pub struct Catalog {
    pub(crate) inner: Arc<DatabaseInner>,
}

impl Catalog {
    pub(crate) const fn new(inner: Arc<DatabaseInner>) -> Self {
        Self { inner }
    }

    /// Load the current immutable outer database state in O(1).
    #[must_use]
    pub fn snapshot(&self) -> CatalogReadSnapshot {
        CatalogReadSnapshot {
            state: self.inner.state.load_full(),
        }
    }

    /// Create a root-owned schema.
    pub fn create_schema(
        &self,
        path: &SchemaPath,
        policy: CreatePolicy,
    ) -> Result<CreateOutcome<SchemaDescriptor>> {
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let outcome = crate::catalog_stage::CatalogStager::new(&self.inner, &mut draft)
                .create_schema(path, policy)?;
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(outcome)
        })
    }

    /// Create and validate a closed graph type.
    pub fn create_graph_type(
        &self,
        path: &ObjectPath,
        definition: GraphTypeDefinition,
        policy: CreatePolicy,
    ) -> Result<CreateOutcome<GraphTypeDescriptor>> {
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let outcome = crate::catalog_stage::CatalogStager::new(&self.inner, &mut draft)
                .create_graph_type(path, definition, policy)?;
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(outcome)
        })
    }

    /// Create an open graph or a graph constrained by a same-schema graph type.
    ///
    /// Under [`CreatePolicy::OrReplace`] an existing graph at the path is
    /// dropped with the same admission as [`Catalog::drop_graph`] and the new
    /// graph, with a fresh identity, is published in the same state swap. A
    /// failure at any point publishes nothing, so the old graph stays
    /// registered and its selected sessions stay valid.
    pub fn create_graph(
        &self,
        path: &ObjectPath,
        graph_type: Option<&ObjectPath>,
        policy: CreatePolicy,
    ) -> Result<CreateOutcome<GraphDescriptor>> {
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let outcome = crate::catalog_stage::CatalogStager::new(&self.inner, &mut draft)
                .create_graph(path, graph_type, policy)?;
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(outcome)
        })
    }

    /// Drop an empty graph under RESTRICT semantics.
    pub fn drop_graph(
        &self,
        path: &ObjectPath,
        policy: DropPolicy,
    ) -> Result<DropOutcome<GraphDescriptor>> {
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let outcome = crate::catalog_stage::CatalogStager::new(&self.inner, &mut draft)
                .drop_graph(path, policy)?;
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(outcome)
        })
    }

    /// Drop an unreferenced graph type under RESTRICT semantics.
    pub fn drop_graph_type(
        &self,
        path: &ObjectPath,
        policy: DropPolicy,
    ) -> Result<DropOutcome<GraphTypeDescriptor>> {
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let outcome = crate::catalog_stage::CatalogStager::new(&self.inner, &mut draft)
                .drop_graph_type(path, policy)?;
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(outcome)
        })
    }

    /// Drop an empty schema under RESTRICT semantics.
    pub fn drop_schema(
        &self,
        path: &SchemaPath,
        policy: DropPolicy,
    ) -> Result<DropOutcome<SchemaDescriptor>> {
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let outcome = crate::catalog_stage::CatalogStager::new(&self.inner, &mut draft)
                .drop_schema(path, policy)?;
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(outcome)
        })
    }
}

pub(crate) fn duplicate_outcome<T>(
    policy: CreatePolicy,
    value: T,
    path: &impl std::fmt::Display,
    kind: &'static str,
) -> Result<CreateOutcome<T>> {
    match policy {
        CreatePolicy::Strict => Err(Error::already_exists(path)),
        CreatePolicy::IfNotExists => Ok(CreateOutcome::AlreadyExists(value)),
        // Graph and graph-type replacement are handled before this point;
        // schema replacement is rejected up front.
        CreatePolicy::OrReplace => Err(Error::unsupported_create_policy(kind)),
    }
}

/// Reject [`CreatePolicy::OrReplace`] for kinds that do not implement it,
/// before any lock is taken and regardless of whether the object exists.
pub(crate) fn reject_replace(policy: CreatePolicy, kind: &'static str) -> Result<()> {
    if policy == CreatePolicy::OrReplace {
        return Err(Error::unsupported_create_policy(kind));
    }
    Ok(())
}

pub(crate) fn missing_outcome<T>(
    policy: DropPolicy,
    path: &impl std::fmt::Display,
    kind: &'static str,
) -> Result<DropOutcome<T>> {
    match policy {
        DropPolicy::Strict => Err(Error::not_found(path, kind)),
        DropPolicy::IfExists => Ok(DropOutcome::NotFound),
    }
}

pub(crate) const fn core_graph_id(id: LowerGraphId) -> CoreGraphId {
    CoreGraphId::new(id.get())
}

impl DatabaseInner {
    pub(crate) fn observe_blocked_drop(&self, instance: &GraphInstance) {
        #[cfg(not(test))]
        let _ = instance;
        #[cfg(test)]
        if let Some(observer) = self.drop_blocked.lock().take() {
            assert!(
                instance.lifecycle.try_write().is_none(),
                "drop observer requires an active request lease"
            );
            observer.send(()).expect("drop observer is listening");
        }
    }

    pub(crate) fn after_descriptor_staging(&self) -> Result<()> {
        #[cfg(test)]
        if *self.failure.lock() == Some(FailurePoint::AfterDescriptorStaging) {
            self.failure.lock().take();
            return Err(Error::injected_failure("after descriptor staging"));
        }
        Ok(())
    }

    pub(crate) fn after_graph_construction(&self) -> Result<()> {
        #[cfg(test)]
        if *self.failure.lock() == Some(FailurePoint::AfterGraphConstruction) {
            self.failure.lock().take();
            return Err(Error::injected_failure("after graph construction"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailurePoint {
    AfterDescriptorStaging,
    AfterGraphConstruction,
    BeforeAuthorityPrepare,
    BeforeAuthorityFlush,
    BeforePublication,
    AfterPublicationAcknowledgement,
    AfterPublicationObserverPanic,
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
