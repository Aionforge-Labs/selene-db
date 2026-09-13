//! Catalog-bound preparation through the immutable analyzer and current-plan adapter.

use super::*;
use crate::analyze::{AnalyzedStatement, catalog::CatalogResolution};

impl PreparedCatalogRequest {
    /// Catalog dependencies resolved from source, when supplied by the facade.
    #[must_use]
    pub fn catalog_resolution(&self) -> Option<&CatalogResolution> {
        self.catalog.as_deref()
    }

    /// Original source for semantic re-preparation after a schema change.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl PreparedCatalogPlan {
    /// Graph identity chosen by semantic analysis rather than session defaults.
    #[must_use]
    pub const fn graph_id(&self) -> selene_core::GraphId {
        self.graph_id
    }

    /// Schema epoch of the selected runtime graph, separate from data generation.
    #[must_use]
    pub const fn schema_version(&self) -> u64 {
        self.schema_version
    }

    /// Catalog dependencies resolved from source, when supplied by the facade.
    #[must_use]
    pub fn catalog_resolution(&self) -> Option<&CatalogResolution> {
        self.catalog.as_deref()
    }
}

impl Session<'_> {
    /// Prepare already catalog-resolved source against its selected graph.
    ///
    /// The source is not parsed or bound again. Closed-schema validation,
    /// request validation, and the one current-plan adapter are shared with the
    /// lower-engine path. This API does not execute or publish anything.
    #[doc(hidden)]
    pub fn prepare_analyzed_catalog_request(
        &mut self,
        source: &str,
        analyzed: AnalyzedStatement,
        registry: &dyn ProcedureRegistry,
        request: RequestExecutionInput,
    ) -> Result<PreparedCatalogRequest, ExecutorError> {
        let snapshot = self.graph().read();
        let graph_id = snapshot.graph_id();
        let graph_generation = snapshot.meta.generation;
        if let Some(resolution) = &analyzed.catalog
            && resolution.selected_graph().get() != graph_id.get()
        {
            return Err(ExecutorError::ImplementationDefined {
                detail: "semantic graph selection disagrees with preparation snapshot",
            });
        }
        if let Some(graph_type) = snapshot.meta.bound_type.as_deref() {
            crate::analyze::schema::validate(&analyzed, graph_type)
                .map_err(|source| ExecutorError::Analysis { source })?;
        }
        super::super::request::validate(&request, &analyzed.parameters, &snapshot)?;
        drop(snapshot);
        let schema_version = self.graph().schema_version();
        let parameters: Arc<[ParameterUse]> = analyzed.parameters.clone().into();
        let catalog = analyzed.catalog.clone().map(Arc::new);
        let (result, request, _, _) = self.with_facade_request(request, false, |session| {
            let lowered = crate::plan::plan_with_caps(&analyzed, registry, &session.caps)
                .map_err(|source| ExecutorError::Plan { source })?;
            let plan = Arc::new(session.optimize_plan(lowered, &analyzed));
            super::super::statement::ensure_source_policy(
                &plan,
                SourceExecutionPolicy::PrepareCatalogSession,
            )?;
            Ok(CatalogSessionOutput::Prepared {
                plan,
                parameter_uses: Arc::clone(&parameters),
            })
        });
        let CatalogSessionOutput::Prepared { plan, .. } = result? else {
            unreachable!("preparation returns the owned plan");
        };
        Ok(PreparedCatalogRequest {
            source: Arc::from(source),
            plan,
            parameter_uses: parameters,
            request,
            graph_id,
            graph_generation,
            schema_version,
            catalog,
        })
    }

    /// Re-resolve a prepared request using fresh catalog metadata and its exact input.
    #[doc(hidden)]
    pub fn reprepare_catalog_request(
        &mut self,
        prepared: PreparedCatalogRequest,
        catalog: selene_catalog::CatalogSnapshot,
        registry: &dyn ProcedureRegistry,
    ) -> Result<PreparedCatalogRequest, ExecutorError> {
        let Some(resolution) = &prepared.catalog else {
            return self.reprepare_source_catalog_request(prepared, registry);
        };
        let environment = resolution.environment(catalog, None);
        let statement =
            crate::parse(&prepared.source).map_err(|source| ExecutorError::Parse { source })?;
        let analyzed = crate::analyze::analyze_with_parameters(
            statement,
            registry,
            Some(environment),
            &prepared.request.parameter_types()?,
        )
        .map_err(|source| ExecutorError::Analysis { source })?;
        self.prepare_analyzed_catalog_request(
            &prepared.source,
            analyzed,
            registry,
            prepared.request,
        )
    }
}
