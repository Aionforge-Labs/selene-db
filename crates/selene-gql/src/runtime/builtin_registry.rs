//! Concrete native built-in procedure registry.
//!
//! [`BuiltinProcedureRegistry`] is the single production implementation of the
//! [`ProcedureRegistry`] trait (D16, decision (c)). It binds `CALL algo.*`
//! directly over the `selene-algorithms` native API — no `ExternalGraphProcedure`
//! indirection, no loadable-pack machinery. The registry is **frozen at
//! construction**: it allocates a fixed set of opaque handles, builds the
//! name→metadata and handle→dispatch tables once, and exposes
//! [`registry_version`](ProcedureRegistry::registry_version) as a constant `0`
//! so the shared CALL plan cache ([`crate::CallPlanCache`]) key stays stable
//! across statements.
//!
//! STEP 2 registers the 19 `algo.*` procedures. The 49 platform
//! built-ins (`selene.health`, `selene.feature_status`, `selene.verify`,
//! `selene.compaction_stats`,
//! `selene.create_index`, `selene.drop_index`, `selene.vector_search_nodes`,
//! `selene.vector_search_nodes_batch`, `selene.vector_score_nodes`,
//! `selene.vector_score_nodes_batch`, `selene.vector_score_neighbors`,
//! `selene.vector_score_neighbors_batch`,
//! `selene.vector_score_candidate_state`,
//! `selene.vector_score_candidate_state_nodes`,
//! `selene.vector_score_candidate_state_expanded`,
//! `selene.vector_score_candidate_state_expanded_batch`,
//! `selene.vector_candidate_states`,
//! `selene.reachable_nodes`,
//! `selene.vector_score_expanded_candidates`,
//! `selene.vector_score_expanded_candidates_batch`,
//! `selene.vector_search_nodes_ann`,
//! `selene.vector_search_nodes_ann_batch`,
//! `selene.vector_search_expanded_candidates_ann`,
//! `selene.vector_search_candidate_state_expanded_ann`,
//! `selene.vector_search_expanded_candidates_ann_batch`,
//! `selene.vector_index_stats`, `selene.text_index_stats`,
//! `selene.json_contains_nodes`, `selene.json_path_exists_nodes`,
//! `selene.json_path_contains_nodes`, `selene.json_path_value_nodes`,
//! `selene.json_contains_candidate_nodes`,
//! `selene.json_path_exists_candidate_nodes`,
//! `selene.json_path_contains_candidate_nodes`,
//! `selene.json_path_value_candidate_nodes`,
//! `selene.rebuild_vector_indexes`,
//! `selene.rebuild_recommended_vector_indexes`, `selene.compact`,
//! `selene.create_vector_index`,
//! `selene.drop_vector_index`, `selene.create_text_index`,
//! `selene.drop_text_index`, `selene.text_search_nodes`,
//! `selene.text_score_nodes`, `selene.text_score_nodes_batch`,
//! `selene.text_score_candidate_state`,
//! `selene.text_score_candidate_state_nodes`,
//! `selene.text_score_candidate_state_expanded_batch`,
//! `selene.reciprocal_rank_fusion`) are registered here,
//! bringing the total to 69;
//! the registry's tables and
//! `iter_handles` are
//! already shaped to carry both.

use std::collections::HashMap;

#[path = "native_declarations.rs"]
mod native_declarations;

use selene_core::{DbString, GraphId, Value, db_string};

use crate::ProcedureContext;
use crate::procedure_registry::{
    ProcedureError, ProcedureHandle, ProcedureMetadata, ProcedureRegistry, ProcedureResult,
};
use crate::runtime::builtins::{BUILTIN_SPECS, BuiltinKind};
use crate::runtime::native_algorithms::{ALGO_SPECS, AlgoKind, AlgorithmCatalogs, forget_graph};

/// Frozen registry version. Construction-once registries keep `0` so the shared
/// CALL plan cache never invalidates against a version bump (risk #5).
const REGISTRY_VERSION: u64 = 0;

/// What an opaque [`ProcedureHandle`] dispatches to.
#[derive(Clone, Copy, Debug)]
enum Dispatch {
    /// A native `algo.*` procedure.
    Algo(AlgoKind),
    /// A native `selene.*` platform built-in.
    Builtin(BuiltinKind),
}

/// Concrete native procedure registry — the single production
/// [`ProcedureRegistry`] impl.
#[derive(Debug)]
pub struct BuiltinProcedureRegistry {
    /// `name → metadata`, used by plan-time [`lookup`](ProcedureRegistry::lookup).
    by_name: HashMap<Box<[DbString]>, ProcedureMetadata>,
    /// `handle → dispatch`, used by runtime [`execute`](ProcedureRegistry::execute).
    by_handle: HashMap<ProcedureHandle, Dispatch>,
    /// `(name, metadata)` pairs in registration order for
    /// [`iter_handles`](ProcedureRegistry::iter_handles) (SHOW PROCEDURES).
    ordered: Vec<(Vec<DbString>, ProcedureMetadata)>,
    /// Engine-internal, per-`GraphId`, ephemeral projection catalogs.
    catalogs: AlgorithmCatalogs,
    /// Canonical immutable declarations normalized from the same closed specs.
    declarations: Vec<selene_catalog::CatalogDescriptor>,
}

impl BuiltinProcedureRegistry {
    /// Construct the frozen native registry with the platform procedure set.
    ///
    /// # Panics
    ///
    /// Panics only if a fixed, static procedure-name segment exceeds the
    /// per-string byte cap (IL013). The native registry's name set is a closed
    /// compile-time constant, so this would be a source invariant bug, not an
    /// operational input.
    #[must_use]
    pub fn new() -> Self {
        let mut by_name = HashMap::new();
        let mut by_handle = HashMap::new();
        let mut ordered = Vec::new();
        let mut declarations = Vec::new();

        // Handles are 1-based and assigned in registration order: the 19
        // `algo.*` procedures first (handles 1..=19), then the 50 `selene.*`
        // platform built-ins (handles 20..=69), continuing the same monotonic
        // sequence. `next_handle` carries the running 1-based handle value.
        let mut next_handle = 1_u64;
        for spec in &ALGO_SPECS {
            let handle = ProcedureHandle::new(next_handle);
            next_handle += 1;
            let name = procedure_name_segments(spec.name);
            let mut metadata = spec.kind.metadata(handle, spec.description);
            let declaration = native_declarations::descriptor(handle.raw(), &name, &metadata);
            metadata.declaration = Some(std::sync::Arc::new(declaration.clone()));
            declarations.push(declaration);

            by_handle.insert(handle, Dispatch::Algo(spec.kind));
            by_name.insert(name.clone().into_boxed_slice(), metadata.clone());
            ordered.push((name, metadata));
        }
        for spec in &BUILTIN_SPECS {
            let handle = ProcedureHandle::new(next_handle);
            next_handle += 1;
            let name = procedure_name_segments(spec.name);
            let mut metadata = spec
                .kind
                .metadata(handle, spec.description, spec.since_version);
            let declaration = native_declarations::descriptor(handle.raw(), &name, &metadata);
            metadata.declaration = Some(std::sync::Arc::new(declaration.clone()));
            declarations.push(declaration);

            by_handle.insert(handle, Dispatch::Builtin(spec.kind));
            by_name.insert(name.clone().into_boxed_slice(), metadata.clone());
            ordered.push((name, metadata));
        }

        Self {
            by_name,
            by_handle,
            ordered,
            catalogs: AlgorithmCatalogs::default(),
            declarations,
        }
    }

    /// Canonical frozen native declaration inventory. Runtime tables are derived
    /// dispatch adapters; changing catalog payloads cannot install callable code.
    pub fn declarations(&self) -> impl Iterator<Item = &selene_catalog::CatalogDescriptor> {
        self.declarations.iter()
    }

    /// Reject a facade publication that would misdescribe its frozen code bindings.
    #[doc(hidden)]
    pub fn validate_catalog(
        &self,
        catalog: &selene_catalog::CatalogSnapshot,
    ) -> selene_catalog::CatalogResult<()> {
        let engine = selene_catalog::CatalogObjectId::Catalog(catalog.catalog_id());
        let actual: Vec<_> = catalog.declarations(engine).collect();
        if actual.len() != self.declarations.len()
            || self
                .declarations
                .iter()
                .any(|expected| catalog.descriptor(expected.id()) != Some(expected))
        {
            return Err(selene_catalog::CatalogError::InvalidDeclaration {
                reason: "unsupported_native_activation",
            });
        }
        for descriptor in catalog.descriptors() {
            if matches!(descriptor.payload(), selene_catalog::CatalogPayload::Procedure(native)
                if native.metadata.state == selene_catalog::DeclarationState::Ready)
                && descriptor.parent()
                    != selene_catalog::CatalogParent::Catalog(catalog.catalog_id())
                && !matches!(
                    (descriptor.parent(), descriptor.payload()),
                    (
                        selene_catalog::CatalogParent::Graph(_),
                        selene_catalog::CatalogPayload::Procedure(
                            selene_catalog::NativeDeclaration {
                                binding: selene_catalog::NativeBinding::CandidateState(_),
                                ..
                            }
                        )
                    )
                )
            {
                return Err(selene_catalog::CatalogError::InvalidDeclaration {
                    reason: "unsupported_native_activation",
                });
            }
        }
        Ok(())
    }

    /// Reclaim the ephemeral projection catalog for a dropped graph.
    ///
    /// Projections are derived, never-persisted state scoped per `GraphId`; an
    /// embedder calls this when a graph is dropped so a later `GraphId` reuse
    /// cannot observe stale projections. Returns `true` if state was present.
    pub fn forget_graph(&self, graph_id: GraphId) -> bool {
        forget_graph(&self.catalogs, graph_id)
    }
}

impl Default for BuiltinProcedureRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcedureRegistry for BuiltinProcedureRegistry {
    fn lookup(&self, name: &[DbString]) -> Option<ProcedureMetadata> {
        self.by_name.get(name).cloned()
    }

    fn registry_version(&self) -> u64 {
        REGISTRY_VERSION
    }

    fn iter_handles(&self) -> Box<dyn Iterator<Item = (Vec<DbString>, ProcedureMetadata)> + '_> {
        Box::new(
            self.ordered
                .iter()
                .map(|(name, metadata)| (name.clone(), metadata.clone())),
        )
    }

    fn execute(
        &self,
        handle: ProcedureHandle,
        args: &[Value],
        ctx: &mut ProcedureContext<'_, '_>,
    ) -> Result<ProcedureResult, ProcedureError> {
        let _span = tracing::span!(
            tracing::Level::INFO,
            "selene.procedure.dispatch",
            procedure = tracing::field::Empty
        )
        .entered();

        let Some(dispatch) = self.by_handle.get(&handle).copied() else {
            return Err(ProcedureError::UnknownProcedure { name: Box::new([]) });
        };

        match dispatch {
            Dispatch::Algo(kind) => {
                tracing::Span::current()
                    .record("procedure", tracing::field::display(procedure_name(kind)));
                // Algorithm procedures are read-only graph-tier; a mutation-tier
                // context is a tier mismatch (mirrors the pack registry's
                // tier-checked dispatch).
                let ProcedureContext::Graph(graph_ctx) = ctx else {
                    return Err(ProcedureError::TierMismatch {
                        expected: crate::ProcedureTier::Graph,
                        actual: ctx.tier(),
                    });
                };
                kind.execute(&self.catalogs, graph_ctx, args)
            }
            Dispatch::Builtin(kind) => {
                tracing::Span::current()
                    .record("procedure", tracing::field::display(builtin_name(kind)));
                // Each built-in declares its own tier (read-only graph-tier for
                // health/feature_status/verify; mutation-tier for
                // create_index/drop_index). Route through the matching context;
                // a mismatch is a tier error (the planner's plan-time tier check
                // already rejects the common cases, but the registry stays
                // self-consistent — and the mutation-tier built-ins write only
                // through `MutationContext::mutator`, never bypassing the funnel).
                match (kind.tier(), ctx) {
                    (crate::ProcedureTier::Graph, ProcedureContext::Graph(graph_ctx)) => {
                        kind.execute_graph(graph_ctx, args)
                    }
                    (crate::ProcedureTier::Mutation, ProcedureContext::Mutation(mut_ctx)) => {
                        kind.execute_mutation(mut_ctx, args)
                    }
                    (
                        crate::ProcedureTier::Maintenance,
                        ProcedureContext::Maintenance(maintenance_ctx),
                    ) => kind.execute_maintenance(maintenance_ctx, args),
                    (expected, ctx) => Err(ProcedureError::TierMismatch {
                        expected,
                        actual: ctx.tier(),
                    }),
                }
            }
        }
    }
}

/// Best-effort dotted name for tracing, derived from the static spec table.
fn procedure_name(kind: AlgoKind) -> String {
    ALGO_SPECS
        .iter()
        .find(|spec| spec.kind == kind)
        .map_or_else(String::new, |spec| spec.name.join("."))
}

/// Best-effort dotted name for tracing a platform built-in.
fn builtin_name(kind: BuiltinKind) -> String {
    BUILTIN_SPECS
        .iter()
        .find(|spec| spec.kind == kind)
        .map_or_else(String::new, |spec| spec.name.join("."))
}

fn procedure_name_segments(raw: &'static [&'static str]) -> Vec<DbString> {
    raw.iter()
        .map(|segment| {
            db_string(segment).expect("static procedure name segment fits DB string cap")
        })
        .collect()
}

#[cfg(test)]
#[path = "builtin_registry_surface_tests.rs"]
mod surface_tests;

#[cfg(test)]
#[path = "builtin_registry_tests.rs"]
mod tests;
