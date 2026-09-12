//! Procedure execution context tiers.

use std::sync::Arc;

use selene_core::{BindingTableId, CancellationChecker, DbString};
use selene_graph::{
    CANDIDATE_STATE_PROVIDER_TAG, CompactionReport, CompactionStats, GraphResult, IndexProvider,
    Mutator, ProviderError, ProviderTag, SeleneGraph, SharedGraph, VectorCandidateSet,
    VectorCandidateStateInfo, VectorIndexMaintenancePolicy, VectorIndexRebuildReport,
};

use crate::{BindingTable, BindingTableRegistry, ImplDefinedCaps, ProcedureError, ProcedureTier};

/// Read-tier procedure context.
pub struct GraphContext<'a> {
    snapshot: &'a SeleneGraph,
    caps: &'a ImplDefinedCaps,
    providers: &'a [Arc<dyn IndexProvider>],
    cancellation: CancellationChecker<'a>,
    binding_tables: Arc<BindingTableRegistry>,
}

impl<'a> GraphContext<'a> {
    pub(crate) fn new(
        snapshot: &'a SeleneGraph,
        caps: &'a ImplDefinedCaps,
        providers: &'a [Arc<dyn IndexProvider>],
        cancellation: CancellationChecker<'a>,
        binding_tables: Arc<BindingTableRegistry>,
    ) -> Self {
        Self {
            snapshot,
            caps,
            providers,
            cancellation,
            binding_tables,
        }
    }

    /// Borrow the graph snapshot visible to this procedure call.
    #[must_use]
    pub const fn snapshot(&self) -> &'a SeleneGraph {
        self.snapshot
    }

    /// Borrow implementation-defined executor caps.
    #[must_use]
    pub const fn impl_defined_caps(&self) -> &'a ImplDefinedCaps {
        self.caps
    }

    /// Look up a registered index provider by its fixed provider tag.
    #[must_use]
    pub fn index_provider_by_tag(&self, tag: ProviderTag) -> Option<Arc<dyn IndexProvider>> {
        self.providers
            .iter()
            .find(|provider| provider.provider_tag() == tag)
            .map(Arc::clone)
    }

    /// Look up a named maintained vector candidate set for this snapshot generation.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the candidate-state provider is present
    /// but cannot prove it has applied through the graph snapshot generation.
    pub fn vector_candidate_set(
        &self,
        name: &DbString,
    ) -> Result<Option<VectorCandidateSet>, ProviderError> {
        let Some(provider) = self.index_provider_by_tag(ProviderTag(CANDIDATE_STATE_PROVIDER_TAG))
        else {
            return Ok(None);
        };
        provider.vector_candidate_set(name, self.snapshot.meta.generation)
    }

    /// Resolve maintained candidates against this exact pinned graph snapshot.
    /// Unavailable names return `None`; stale provider or candidate identity is an error.
    pub fn node_candidate_set(
        &self,
        name: &DbString,
    ) -> Result<Option<selene_graph::CandidateSet<selene_graph::Node>>, ProviderError> {
        let Some(provider) = self.index_provider_by_tag(ProviderTag(CANDIDATE_STATE_PROVIDER_TAG))
        else {
            return Ok(None);
        };
        self.snapshot
            .maintained_node_candidates(provider.as_ref(), name)
    }

    /// List maintained vector candidate-state descriptors for this snapshot generation.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the candidate-state provider is present
    /// but cannot prove it has applied through the graph snapshot generation.
    pub fn vector_candidate_state_infos(
        &self,
    ) -> Result<Vec<VectorCandidateStateInfo>, ProviderError> {
        let Some(provider) = self.index_provider_by_tag(ProviderTag(CANDIDATE_STATE_PROVIDER_TAG))
        else {
            return Ok(Vec::new());
        };
        provider.vector_candidate_state_infos(self.snapshot.meta.generation)
    }

    /// Build the cancellation checker visible to this procedure call.
    #[must_use]
    pub const fn cancellation_checker(&self) -> CancellationChecker<'a> {
        self.cancellation
    }

    /// Register a binding table for this procedure call's statement.
    pub fn register_binding_table(
        &self,
        table: Arc<BindingTable>,
    ) -> Result<BindingTableId, ProcedureError> {
        self.binding_tables
            .register(table)
            .map_err(ProcedureError::from)
    }
}

/// Mutation-tier procedure context.
pub struct MutationContext<'a, 'g> {
    mutator: Mutator<'a, 'g>,
    caps: &'a ImplDefinedCaps,
    cancellation: CancellationChecker<'a>,
    binding_tables: Arc<BindingTableRegistry>,
}

impl<'a, 'g> MutationContext<'a, 'g> {
    pub(crate) fn new(
        mutator: Mutator<'a, 'g>,
        caps: &'a ImplDefinedCaps,
        cancellation: CancellationChecker<'a>,
        binding_tables: Arc<BindingTableRegistry>,
    ) -> Self {
        Self {
            mutator,
            caps,
            cancellation,
            binding_tables,
        }
    }

    /// Construct a mutation context for external procedure test harnesses.
    #[cfg(any(test, feature = "test-harness"))]
    #[must_use]
    pub fn for_test(mutator: Mutator<'a, 'g>, caps: &'a ImplDefinedCaps) -> Self {
        Self::new(
            mutator,
            caps,
            CancellationChecker::disabled(),
            Arc::new(BindingTableRegistry::new()),
        )
    }

    /// Borrow the transaction-local working graph.
    #[must_use]
    pub fn snapshot(&self) -> &SeleneGraph {
        self.mutator.read()
    }

    /// Borrow the mutation funnel.
    pub fn mutator(&mut self) -> &mut Mutator<'a, 'g> {
        &mut self.mutator
    }

    /// Look up a registered index provider through the held write transaction.
    #[must_use]
    pub fn index_provider_by_tag(&self, tag: ProviderTag) -> Option<Arc<dyn IndexProvider>> {
        self.mutator.index_provider_by_tag(tag)
    }

    /// Borrow implementation-defined executor caps.
    #[must_use]
    pub const fn impl_defined_caps(&self) -> &'a ImplDefinedCaps {
        self.caps
    }

    /// Build the cancellation checker visible to this procedure call.
    #[must_use]
    pub const fn cancellation_checker(&self) -> CancellationChecker<'a> {
        self.cancellation
    }

    /// Register a binding table for this procedure call's statement.
    pub fn register_binding_table(
        &self,
        table: Arc<BindingTable>,
    ) -> Result<BindingTableId, ProcedureError> {
        self.binding_tables
            .register(table)
            .map_err(ProcedureError::from)
    }
}

/// Engine-maintenance procedure context.
pub struct MaintenanceContext<'a, 'g> {
    graph: &'g SharedGraph,
    caps: &'a ImplDefinedCaps,
    cancellation: CancellationChecker<'a>,
    binding_tables: Arc<BindingTableRegistry>,
}

impl<'a, 'g> MaintenanceContext<'a, 'g> {
    pub(crate) fn new(
        graph: &'g SharedGraph,
        caps: &'a ImplDefinedCaps,
        cancellation: CancellationChecker<'a>,
        binding_tables: Arc<BindingTableRegistry>,
    ) -> Self {
        Self {
            graph,
            caps,
            cancellation,
            binding_tables,
        }
    }

    /// Borrow implementation-defined executor caps.
    #[must_use]
    pub const fn impl_defined_caps(&self) -> &'a ImplDefinedCaps {
        self.caps
    }

    /// Build the cancellation checker visible to this procedure call.
    #[must_use]
    pub const fn cancellation_checker(&self) -> CancellationChecker<'a> {
        self.cancellation
    }

    /// Rebuild all registered vector indexes from primary graph values.
    ///
    /// # Errors
    ///
    /// Returns graph errors raised by strict rebuild validation.
    pub fn rebuild_vector_indexes(&self) -> GraphResult<VectorIndexRebuildReport> {
        self.graph.rebuild_vector_indexes()
    }

    /// Rebuild vector indexes whose diagnostics recommend maintenance.
    ///
    /// # Errors
    ///
    /// Returns graph errors raised by strict rebuild validation.
    pub fn rebuild_recommended_vector_indexes(&self) -> GraphResult<VectorIndexRebuildReport> {
        self.graph.rebuild_recommended_vector_indexes()
    }

    /// Maintain recommended vector indexes under a caller-supplied policy.
    ///
    /// # Errors
    ///
    /// Returns graph errors raised by strict rebuild validation.
    pub fn maintain_vector_indexes(
        &self,
        policy: VectorIndexMaintenancePolicy,
    ) -> GraphResult<VectorIndexRebuildReport> {
        self.graph.maintain_vector_indexes(policy)
    }

    /// Return current graph compaction pressure.
    #[must_use]
    pub fn compaction_stats(&self) -> CompactionStats {
        self.graph.compaction_stats()
    }

    /// Compact dead rows out of the live graph.
    ///
    /// # Errors
    ///
    /// Returns graph errors raised by compaction consistency validation.
    pub fn compact(&self) -> GraphResult<CompactionReport> {
        self.graph.compact()
    }

    /// Register a binding table for this procedure call's statement.
    pub fn register_binding_table(
        &self,
        table: Arc<BindingTable>,
    ) -> Result<BindingTableId, ProcedureError> {
        self.binding_tables
            .register(table)
            .map_err(ProcedureError::from)
    }
}

/// Tier-tagged procedure context passed through [`crate::ProcedureRegistry`].
#[non_exhaustive]
pub enum ProcedureContext<'a, 'g> {
    /// Read-only graph-tier context.
    Graph(GraphContext<'a>),
    /// Mutation-tier context.
    Mutation(MutationContext<'a, 'g>),
    /// Engine-maintenance context.
    Maintenance(MaintenanceContext<'a, 'g>),
}

impl ProcedureContext<'_, '_> {
    /// Return the context tier represented by this enum variant.
    #[must_use]
    pub const fn tier(&self) -> ProcedureTier {
        match self {
            Self::Graph(_) => ProcedureTier::Graph,
            Self::Mutation(_) => ProcedureTier::Mutation,
            Self::Maintenance(_) => ProcedureTier::Maintenance,
        }
    }

    /// Register a binding table for the currently executing procedure call.
    pub fn register_binding_table(
        &self,
        table: Arc<BindingTable>,
    ) -> Result<BindingTableId, ProcedureError> {
        match self {
            Self::Graph(ctx) => ctx.register_binding_table(table),
            Self::Mutation(ctx) => ctx.register_binding_table(table),
            Self::Maintenance(ctx) => ctx.register_binding_table(table),
        }
    }
}
