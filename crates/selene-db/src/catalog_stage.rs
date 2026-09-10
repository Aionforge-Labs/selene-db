//! Detached catalog lifecycle staging shared by Rust and selected GQL paths.

use std::sync::Arc;

use selene_catalog::{
    CatalogDescriptor as LowerDescriptor, CatalogObjectId, CatalogObjectKind, CatalogPayload,
    CatalogTransaction, CreationMetadata, GraphId as LowerGraphId, GraphTypeId as LowerGraphTypeId,
    SchemaId as LowerSchemaId,
};
use selene_graph::SeleneGraph;

use crate::{
    CreateOutcome, CreatePolicy, DropOutcome, DropPolicy, Error, GraphDescriptor,
    GraphTypeDefinition, GraphTypeDescriptor, ObjectPath, Result, SchemaDescriptor, SchemaPath,
    catalog::{core_graph_id, duplicate_outcome, missing_outcome, reject_replace},
    catalog_snapshot::{
        ensure_catalog, find_object, graph_summary, graph_type_summary, next_id, require_schema,
        resolve_binding, schema_summary,
    },
    database::DatabaseInner,
    transaction::DatabaseDraft,
};

/// Mutates only a detached draft; publication remains the caller's decision.
pub(crate) struct CatalogStager<'a> {
    inner: &'a DatabaseInner,
    draft: &'a mut DatabaseDraft,
}

impl<'a> CatalogStager<'a> {
    pub(crate) const fn new(inner: &'a DatabaseInner, draft: &'a mut DatabaseDraft) -> Self {
        Self { inner, draft }
    }

    pub(crate) fn create_schema(
        &mut self,
        path: &SchemaPath,
        policy: CreatePolicy,
    ) -> Result<CreateOutcome<SchemaDescriptor>> {
        reject_replace(policy, "schema")?;
        let state = self.draft.state_view();
        ensure_catalog(&state, path.catalog())?;
        if let Some(existing) = state.catalog.schema(&path.schema.0) {
            return duplicate_outcome(policy, schema_summary(&state, existing)?, path, "schema");
        }
        let raw = next_id(self.draft.high_water.schema, "schema")?;
        let id = LowerSchemaId::new(raw).map_err(Error::from_catalog_invariant)?;
        let mut transaction =
            CatalogTransaction::new(&self.draft.catalog).map_err(Error::from_catalog_invariant)?;
        let descriptor = LowerDescriptor::schema(
            id,
            path.schema.0.clone(),
            self.draft.catalog.root_directory_id(),
            transaction.generation(),
            CreationMetadata::new(transaction.generation(), None),
        )
        .map_err(Error::from_catalog_invariant)?;
        transaction
            .insert(descriptor)
            .map_err(Error::from_catalog_invariant)?;
        self.inner.after_descriptor_staging()?;
        self.draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
        self.draft.high_water.schema = raw;
        self.draft.mark_modified();
        let state = self.draft.state_view();
        let created = state
            .catalog
            .descriptor(CatalogObjectId::Schema(id))
            .ok_or_else(|| Error::catalog_invariant("created schema descriptor is missing"))?;
        Ok(CreateOutcome::Created(schema_summary(&state, created)?))
    }

    pub(crate) fn drop_schema(
        &mut self,
        path: &SchemaPath,
        policy: DropPolicy,
    ) -> Result<DropOutcome<SchemaDescriptor>> {
        let state = self.draft.state_view();
        ensure_catalog(&state, path.catalog())?;
        let Some(descriptor) = state.catalog.schema(&path.schema.0) else {
            return missing_outcome(policy, path, "schema");
        };
        let CatalogObjectId::Schema(id) = descriptor.id() else {
            unreachable!("schema dictionary contains schemas")
        };
        let children = state.catalog.schema_objects(id).map_or(0, Iterator::count);
        if children != 0 {
            return Err(Error::nonempty_schema(path, children));
        }
        let summary = schema_summary(&state, descriptor)?;
        let mut transaction =
            CatalogTransaction::new(&self.draft.catalog).map_err(Error::from_catalog_invariant)?;
        transaction.remove(CatalogObjectId::Schema(id));
        self.inner.after_descriptor_staging()?;
        self.draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
        self.draft.mark_modified();
        Ok(DropOutcome::Dropped(summary))
    }

    pub(crate) fn create_graph_type(
        &mut self,
        path: &ObjectPath,
        definition: GraphTypeDefinition,
        policy: CreatePolicy,
    ) -> Result<CreateOutcome<GraphTypeDescriptor>> {
        let state = self.draft.state_view();
        let schema = require_schema(&state, &path.schema_path())?;
        let mut replaced = None;
        if let Some(existing) = state.catalog.schema_object(schema, &path.object.0) {
            if existing.kind() != CatalogObjectKind::GraphType {
                return Err(Error::wrong_kind(path, "graph type", existing.kind()));
            }
            let summary = graph_type_summary(&state, existing)?;
            if policy != CreatePolicy::OrReplace {
                return duplicate_outcome(policy, summary, path, "graph type");
            }
            let id = graph_type_drop_admission(&state, path, existing)?;
            replaced = Some((id, summary));
        }
        let raw = next_id(self.draft.high_water.graph_type, "graph type")?;
        let id = LowerGraphTypeId::new(raw).map_err(Error::from_catalog_invariant)?;
        let runtime = Arc::new(definition.into_runtime(path.object())?);
        let mut transaction =
            CatalogTransaction::new(&self.draft.catalog).map_err(Error::from_catalog_invariant)?;
        if let Some((dropped, _)) = &replaced {
            transaction.remove_owner(CatalogObjectId::GraphType(*dropped));
            self.draft.graph_types.remove(dropped);
        }
        let descriptor = LowerDescriptor::graph_type(
            id,
            path.object.0.clone(),
            schema,
            transaction.generation(),
            CreationMetadata::new(transaction.generation(), None),
        )
        .map_err(Error::from_catalog_invariant)?;
        transaction
            .insert(descriptor)
            .map_err(Error::from_catalog_invariant)?;
        self.inner.after_descriptor_staging()?;
        self.draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
        self.draft.graph_types.insert(id, runtime);
        self.draft.high_water.graph_type = raw;
        self.draft.mark_modified();
        let state = self.draft.state_view();
        let created = state
            .catalog
            .descriptor(CatalogObjectId::GraphType(id))
            .ok_or_else(|| Error::catalog_invariant("created graph-type descriptor is missing"))?;
        let created = graph_type_summary(&state, created)?;
        Ok(match replaced {
            Some((_, dropped)) => CreateOutcome::Replaced { dropped, created },
            None => CreateOutcome::Created(created),
        })
    }

    pub(crate) fn drop_graph_type(
        &mut self,
        path: &ObjectPath,
        policy: DropPolicy,
    ) -> Result<DropOutcome<GraphTypeDescriptor>> {
        let state = self.draft.state_view();
        let Some(descriptor) = find_object(&state, path)? else {
            return missing_outcome(policy, path, "graph type");
        };
        if descriptor.kind() != CatalogObjectKind::GraphType {
            return Err(Error::wrong_kind(path, "graph type", descriptor.kind()));
        }
        let id = graph_type_drop_admission(&state, path, descriptor)?;
        let summary = graph_type_summary(&state, descriptor)?;
        let mut transaction =
            CatalogTransaction::new(&self.draft.catalog).map_err(Error::from_catalog_invariant)?;
        transaction.remove_owner(CatalogObjectId::GraphType(id));
        self.inner.after_descriptor_staging()?;
        self.draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
        self.draft.graph_types.remove(&id);
        self.draft.mark_modified();
        Ok(DropOutcome::Dropped(summary))
    }

    pub(crate) fn create_graph(
        &mut self,
        path: &ObjectPath,
        graph_type: Option<&ObjectPath>,
        policy: CreatePolicy,
    ) -> Result<CreateOutcome<GraphDescriptor>> {
        let state = self.draft.state_view();
        let schema = require_schema(&state, &path.schema_path())?;
        let mut replaced = None;
        if let Some(existing) = state.catalog.schema_object(schema, &path.object.0) {
            if existing.kind() != CatalogObjectKind::Graph {
                return Err(Error::wrong_kind(path, "graph", existing.kind()));
            }
            let summary = graph_summary(&state, existing)?;
            if policy != CreatePolicy::OrReplace {
                return duplicate_outcome(policy, summary, path, "graph");
            }
            let CatalogObjectId::Graph(id) = existing.id() else {
                unreachable!("kind and typed ID agree")
            };
            self.check_droppable(id, path)?;
            replaced = Some((id, summary));
        }
        let (type_id, runtime) = resolve_binding(&state, schema, graph_type)?;
        let raw = next_id(self.draft.high_water.graph, "graph")?;
        let id = LowerGraphId::new(raw).map_err(Error::from_catalog_invariant)?;
        let mut transaction =
            CatalogTransaction::new(&self.draft.catalog).map_err(Error::from_catalog_invariant)?;
        if let Some((dropped, _)) = &replaced {
            transaction.remove_owner(CatalogObjectId::Graph(*dropped));
            self.draft.remove_graph(*dropped);
        }
        let descriptor = LowerDescriptor::graph(
            id,
            path.object.0.clone(),
            schema,
            transaction.generation(),
            CreationMetadata::new(transaction.generation(), None),
            type_id,
        )
        .map_err(Error::from_catalog_invariant)?;
        transaction
            .insert(descriptor)
            .map_err(Error::from_catalog_invariant)?;
        self.inner.after_descriptor_staging()?;
        let mut graph = SeleneGraph::new(core_graph_id(id));
        graph.meta.bound_type = runtime;
        self.draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
        self.draft.high_water.graph = raw;
        graph
            .bind_catalog(&self.draft.catalog)
            .map_err(Error::from_catalog_invariant)?;
        self.draft.replace_graph(id, graph)?;
        self.inner.after_graph_construction()?;
        if let Some((dropped, _)) = &replaced {
            self.draft.forget_graph(core_graph_id(*dropped));
        }
        let state = self.draft.state_view();
        let created = state
            .catalog
            .descriptor(CatalogObjectId::Graph(id))
            .ok_or_else(|| Error::catalog_invariant("created graph descriptor is missing"))?;
        let created = graph_summary(&state, created)?;
        Ok(match replaced {
            Some((_, dropped)) => CreateOutcome::Replaced { dropped, created },
            None => CreateOutcome::Created(created),
        })
    }

    pub(crate) fn drop_graph(
        &mut self,
        path: &ObjectPath,
        policy: DropPolicy,
    ) -> Result<DropOutcome<GraphDescriptor>> {
        let state = self.draft.state_view();
        let Some(descriptor) = find_object(&state, path)? else {
            return missing_outcome(policy, path, "graph");
        };
        if descriptor.kind() != CatalogObjectKind::Graph {
            return Err(Error::wrong_kind(path, "graph", descriptor.kind()));
        }
        let CatalogObjectId::Graph(id) = descriptor.id() else {
            unreachable!("kind and typed ID agree")
        };
        self.check_droppable(id, path)?;
        let summary = graph_summary(&state, descriptor)?;
        let mut transaction =
            CatalogTransaction::new(&self.draft.catalog).map_err(Error::from_catalog_invariant)?;
        transaction.remove_owner(CatalogObjectId::Graph(id));
        self.inner.after_descriptor_staging()?;
        self.draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
        self.draft.remove_graph(id);
        self.draft.forget_graph(core_graph_id(id));
        Ok(DropOutcome::Dropped(summary))
    }

    fn check_droppable(&self, id: LowerGraphId, path: &ObjectPath) -> Result<()> {
        if let Some(graph) = self.draft.replacement_snapshot(id) {
            return check_empty(path, graph.node_count(), graph.edge_count());
        }
        let current = self.inner.state.load_full();
        if !self.draft.matches_base(&current) {
            return Err(Error::transaction_rollback());
        }
        let instance = current.graphs.get(&id).cloned().ok_or_else(|| {
            Error::catalog_invariant("graph descriptor has no registered runtime instance")
        })?;
        self.inner.observe_blocked_drop(&instance);
        let _lifecycle = instance.lifecycle.write();
        let current = self.inner.state.load_full();
        if !self.draft.matches_base(&current)
            || current
                .graphs
                .get(&id)
                .is_none_or(|registered| !Arc::ptr_eq(registered, &instance))
        {
            return Err(Error::transaction_rollback());
        }
        let graph = instance.graph.read();
        check_empty(path, graph.node_count(), graph.edge_count())
    }
}

fn graph_type_drop_admission(
    state: &crate::database::DatabaseState,
    path: &ObjectPath,
    descriptor: &LowerDescriptor,
) -> Result<LowerGraphTypeId> {
    let CatalogObjectId::GraphType(id) = descriptor.id() else {
        unreachable!("kind and typed ID agree")
    };
    let references = state
        .catalog
        .descriptors()
        .filter(|candidate| {
            matches!(candidate.payload(), CatalogPayload::Graph { graph_type: Some(target) } if *target == id)
        })
        .count();
    if references != 0 {
        return Err(Error::referenced_graph_type(path, references));
    }
    Ok(id)
}

fn check_empty(path: &ObjectPath, nodes: usize, edges: usize) -> Result<()> {
    if nodes == 0 && edges == 0 {
        Ok(())
    } else {
        Err(Error::nonempty_graph(path, nodes, edges))
    }
}

const _: fn() = || {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<DatabaseDraft>();
};
