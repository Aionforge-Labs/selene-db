//! Catalog-backed lexical working sites and exact descriptor dependencies.

use super::AnalysisError;
use crate::{CatalogObjectReference, CatalogPathSegment, IdentifierForm, SourceSpan};
use selene_catalog::{
    CatalogDescriptor, CatalogId, CatalogName, CatalogObjectId, CatalogParent, CatalogPayload,
    CatalogSnapshot, GraphId, SchemaId,
};
use std::collections::BTreeMap;

/// Immutable catalog environment supplied by the ownership root to analysis.
///
/// The ambient graph is a default, not an already-executed graph access. An
/// explicit transaction separately supplies its pinned single-graph authority.
#[derive(Clone)]
pub struct CatalogEnvironment {
    pub(crate) catalog: CatalogSnapshot,
    pub(crate) schema: SchemaId,
    pub(crate) graph: GraphId,
    pub(crate) transaction_graph: Option<GraphId>,
}

impl CatalogEnvironment {
    /// Build an environment from one catalog snapshot and copied working sites.
    #[must_use]
    pub const fn new(
        catalog: CatalogSnapshot,
        schema: SchemaId,
        graph: GraphId,
        transaction_graph: Option<GraphId>,
    ) -> Self {
        Self {
            catalog,
            schema,
            graph,
            transaction_graph,
        }
    }
}

/// One resolved lexical site, preserving its original scope-clause origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingSite {
    /// Scope-tree node containing this working site.
    pub scope: super::ScopeId,
    /// Scope-clause or graph-access origin.
    pub origin: SourceSpan,
    /// Working schema in this lexical region.
    pub schema: SchemaId,
    /// Working graph in this lexical region.
    pub graph: GraphId,
}

/// Precise semantic dependencies, independent of data-snapshot generations.
#[derive(Clone, Debug)]
pub struct CatalogResolution {
    catalog: CatalogId,
    ambient_schema: SchemaId,
    ambient_graph: GraphId,
    selected_graph: GraphId,
    objects: Vec<CatalogDescriptor>,
    declarations: Vec<(CatalogObjectId, Vec<CatalogDescriptor>)>,
    sites: Vec<WorkingSite>,
}

impl CatalogResolution {
    /// The one graph selected by this request (ambient only if no graph was used).
    #[must_use]
    pub const fn selected_graph(&self) -> GraphId {
        self.selected_graph
    }

    /// Exact descriptor dependencies in deterministic typed-ID order.
    #[must_use]
    pub fn objects(&self) -> &[CatalogDescriptor] {
        &self.objects
    }

    /// Resolved lexical sites in traversal order.
    #[must_use]
    pub fn sites(&self) -> &[WorkingSite] {
        &self.sites
    }

    /// Check both descriptor identity and its authoritative namespace binding.
    /// Same-path replacement cannot reuse a stale numeric lookup.
    #[must_use]
    pub fn is_current(&self, catalog: &CatalogSnapshot) -> bool {
        self.catalog == catalog.catalog_id()
            && self.objects.iter().all(|expected| {
                let by_name = match expected.parent() {
                    CatalogParent::Directory(_) => catalog.schema(expected.name()),
                    CatalogParent::Schema(schema) => catalog.schema_object(schema, expected.name()),
                    CatalogParent::Graph(graph) => {
                        catalog.declaration(CatalogObjectId::Graph(graph), expected.name())
                    }
                    CatalogParent::GraphType(graph_type) => {
                        catalog.declaration(CatalogObjectId::GraphType(graph_type), expected.name())
                    }
                    _ => catalog.descriptor(expected.id()),
                };
                by_name == Some(expected) && catalog.descriptor(expected.id()) == Some(expected)
            })
            && self
                .declarations
                .iter()
                .all(|(owner, expected)| catalog.declarations(*owner).eq(expected.iter()))
    }

    /// Recreate the original lexical defaults against a fresh catalog snapshot.
    #[must_use]
    pub fn environment(
        &self,
        catalog: CatalogSnapshot,
        transaction_graph: Option<GraphId>,
    ) -> CatalogEnvironment {
        CatalogEnvironment::new(
            catalog,
            self.ambient_schema,
            self.ambient_graph,
            transaction_graph,
        )
    }
}

pub(crate) struct CatalogResolver {
    environment: CatalogEnvironment,
    pub(crate) schema: SchemaId,
    pub(crate) graph: GraphId,
    used_graph: Option<GraphId>,
    objects: BTreeMap<CatalogObjectId, CatalogDescriptor>,
    declarations: BTreeMap<CatalogObjectId, Vec<CatalogDescriptor>>,
    sites: Vec<WorkingSite>,
}

impl CatalogResolver {
    pub(crate) fn new(environment: CatalogEnvironment) -> Self {
        Self {
            schema: environment.schema,
            graph: environment.graph,
            used_graph: environment.transaction_graph,
            environment,
            objects: BTreeMap::new(),
            declarations: BTreeMap::new(),
            sites: Vec::new(),
        }
    }

    pub(crate) fn select_schema(
        &mut self,
        reference: &CatalogObjectReference,
    ) -> Result<(), AnalysisError> {
        if !reference.absolute || reference.segments.len() != 1 {
            return Err(invalid(
                reference.span,
                "AT requires a supported absolute schema reference",
            ));
        }
        let name = catalog_name(&reference.segments[0], reference.span)?;
        let descriptor = self.environment.catalog.schema(&name).ok_or_else(|| {
            invalid(
                reference.span,
                format!("schema {} does not exist", name.display()),
            )
        })?;
        let CatalogObjectId::Schema(id) = descriptor.id() else {
            return Err(invalid(reference.span, "reference is not a schema"));
        };
        self.objects.insert(descriptor.id(), descriptor.clone());
        self.schema = id;
        Ok(())
    }

    pub(crate) fn resolve_graph(
        &mut self,
        reference: &CatalogObjectReference,
    ) -> Result<GraphId, AnalysisError> {
        let (schema, leaf) = match (reference.absolute, reference.segments.as_slice()) {
            (false, [leaf]) => (self.schema, leaf),
            (true, [schema, leaf]) => {
                let name = catalog_name(schema, reference.span)?;
                let descriptor = self.environment.catalog.schema(&name).ok_or_else(|| {
                    invalid(
                        reference.span,
                        format!("schema {} does not exist", name.display()),
                    )
                })?;
                let CatalogObjectId::Schema(id) = descriptor.id() else {
                    return Err(invalid(reference.span, "graph parent is not a schema"));
                };
                self.objects.insert(descriptor.id(), descriptor.clone());
                (id, leaf)
            }
            _ => {
                return Err(invalid(
                    reference.span,
                    "unsupported catalog graph path shape",
                ));
            }
        };
        self.depend_on(CatalogObjectId::Schema(schema), reference.span)?;
        let name = catalog_name(leaf, reference.span)?;
        let descriptor = self
            .environment
            .catalog
            .schema_object(schema, &name)
            .ok_or_else(|| {
                invalid(
                    reference.span,
                    format!("graph {} does not exist in working schema", name.display()),
                )
            })?;
        let CatalogObjectId::Graph(id) = descriptor.id() else {
            return Err(invalid(
                reference.span,
                format!("{} is {:?}, not a graph", name.display(), descriptor.kind()),
            ));
        };
        self.objects.insert(descriptor.id(), descriptor.clone());
        Ok(id)
    }

    pub(crate) fn use_graph(&mut self, span: SourceSpan) -> Result<(), AnalysisError> {
        if let Some(first) = self.used_graph
            && first != self.graph
        {
            return Err(AnalysisError::MultipleGraphs {
                first,
                requested: self.graph,
                span,
            });
        }
        self.used_graph = Some(self.graph);
        let graph_type = self
            .environment
            .catalog
            .descriptor(CatalogObjectId::Graph(self.graph))
            .and_then(|descriptor| match descriptor.payload() {
                CatalogPayload::Graph { graph_type } => *graph_type,
                _ => None,
            });
        self.depend_on(CatalogObjectId::Graph(self.graph), span)?;
        self.depend_on_declarations(CatalogObjectId::Graph(self.graph));
        if let Some(graph_type) = graph_type {
            self.depend_on(CatalogObjectId::GraphType(graph_type), span)?;
            self.depend_on_declarations(CatalogObjectId::GraphType(graph_type));
        }
        Ok(())
    }

    pub(crate) fn record_site(&mut self, scope: super::ScopeId, origin: SourceSpan) {
        self.sites.push(WorkingSite {
            scope,
            origin,
            schema: self.schema,
            graph: self.graph,
        });
    }

    fn depend_on(&mut self, id: CatalogObjectId, span: SourceSpan) -> Result<(), AnalysisError> {
        let descriptor = self
            .environment
            .catalog
            .descriptor(id)
            .ok_or_else(|| invalid(span, "working catalog identity is no longer present"))?;
        self.objects.insert(id, descriptor.clone());
        Ok(())
    }

    fn depend_on_declarations(&mut self, owner: CatalogObjectId) {
        self.declarations.entry(owner).or_insert_with(|| {
            self.environment
                .catalog
                .declarations(owner)
                .cloned()
                .collect()
        });
    }

    pub(crate) fn finish(self) -> CatalogResolution {
        CatalogResolution {
            catalog: self.environment.catalog.catalog_id(),
            ambient_schema: self.environment.schema,
            ambient_graph: self.environment.graph,
            selected_graph: self.used_graph.unwrap_or(self.environment.graph),
            objects: self.objects.into_values().collect(),
            declarations: self.declarations.into_iter().collect(),
            sites: self.sites,
        }
    }
}

fn catalog_name(
    segment: &CatalogPathSegment,
    span: SourceSpan,
) -> Result<CatalogName, AnalysisError> {
    match segment.form {
        IdentifierForm::Regular => CatalogName::regular(segment.name.as_str()),
        IdentifierForm::Delimited => CatalogName::delimited(segment.name.as_str()),
    }
    .map_err(|error| invalid(span, format!("invalid catalog name: {error}")))
}

fn invalid(span: SourceSpan, message: impl Into<String>) -> AnalysisError {
    AnalysisError::InvalidReference {
        span,
        message: message.into(),
    }
}
