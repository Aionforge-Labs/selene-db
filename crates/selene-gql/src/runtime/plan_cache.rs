//! Session-scoped execution-plan cache.
//!
//! # Why data generation is not a cache dimension
//!
//! Cached plans are the *optimized* plans (`Session::execute_source` optimizes
//! before insert). The cache freshness check compares the stored
//! `schema_version_at_plan` and `profile_identity_at_plan` against the current
//! schema and effective profile. The schema version bumps only on
//! schema-changing commits (CREATE / DROP INDEX, type DDL). The snapshot
//! `generation` counter — which bumps on *every* commit including data-only
//! writes — is deliberately NOT part of the key: optimizer index
//! *selection* depends only on the set of indexes (schema), never on row data,
//! so a chosen access path (LabelIndex / TypedIndexRange / CompositeLookup)
//! stays correct as rows mutate within an epoch. Adding `generation` would
//! evict every cached plan on every data write for zero correctness benefit.
//!
//! FUTURE: wiring live cardinality statistics into the cost model would make
//! plan choice data-dependent. At that point the key MUST also include
//! `generation`; that is out of scope for the structural index-selection
//! optimizer wired today.

use std::{
    borrow::Borrow,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
};

use lru::LruCache;
use selene_core::GraphId;
use selene_profile::ProfileIdentity;

use crate::{ExecutionPlan, ImplDefinedCaps, PipelineOp, SubqueryBody};

/// LRU-cached execution plans keyed by source text.
pub struct PlanCache {
    inner: LruCache<CacheKey, CachedPlan>,
    stats: PlanCacheStats,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    source: Arc<str>,
}

impl Borrow<str> for CacheKey {
    fn borrow(&self) -> &str {
        &self.source
    }
}

struct CachedPlan {
    plan: Arc<ExecutionPlan>,
    schema_version_at_plan: u64,
    registry_version_at_plan: u64,
    profile_identity_at_plan: ProfileIdentity,
}

/// Shared LRU cache for non-CALL source-string execution plans.
///
/// The cache is caller-owned so embedders can share one
/// `Arc<SharedPlanCache>` across short-lived sessions executing against the
/// same graph. Keys include graph identity, schema epoch, procedure-registry
/// epoch, effective profile identity, source text, implementation-defined caps,
/// and the optimizer index-selection mode, matching every setting currently
/// baked into a lowered or optimized plan. Procedure-CALL-containing plans
/// remain owned by [`crate::CallPlanCache`].
pub struct SharedPlanCache {
    inner: Mutex<SharedPlanCacheInner>,
}

struct SharedPlanCacheInner {
    plans: LruCache<SharedPlanCacheKey, Arc<ExecutionPlan>>,
    stats: SharedPlanCacheStats,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SharedPlanCacheKey {
    graph_id: GraphId,
    schema_version: u64,
    registry_version: u64,
    profile_identity: ProfileIdentity,
    source: Arc<str>,
    caps: ImplDefinedCaps,
    index_selection: bool,
}

/// Counters for [`PlanCache`] lookup and eviction behavior.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlanCacheStats {
    /// Successful source + schema-version lookups.
    pub hits: u64,
    /// Source keys not present in the cache.
    pub misses: u64,
    /// Entries found but invalidated because their schema version was stale.
    pub stale_invalidations: u64,
    /// Entries evicted by LRU capacity pressure.
    pub capacity_evictions: u64,
}

/// Counters for [`SharedPlanCache`] lookup and eviction behavior.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SharedPlanCacheStats {
    /// Successful key lookups.
    pub hits: u64,
    /// Keys not present in the cache.
    pub misses: u64,
    /// Entries evicted by LRU capacity pressure.
    pub capacity_evictions: u64,
}

impl PlanCache {
    /// Create an empty plan cache with the given entry capacity.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: LruCache::new(capacity),
            stats: PlanCacheStats::default(),
        }
    }

    /// Lookup a cached plan pinned to its semantic environment.
    ///
    /// The caller pins `schema_version` (graph/index epoch),
    /// `registry_version` (procedure-registry epoch), and `profile_identity`
    /// (effective profile) at analysis. A mismatch on any dimension
    /// invalidates the entry with a typed stale count; executing requests
    /// either reuse the pinned plan or re-analyze under the current
    /// environment. Runtime row data never participates: optimizer index
    /// selection depends only on which indexes exist, so data writes within
    /// an epoch keep cached access paths correct.
    pub(crate) fn get(
        &mut self,
        source: &str,
        schema_version: u64,
        registry_version: u64,
        profile_identity: ProfileIdentity,
    ) -> Option<Arc<ExecutionPlan>> {
        match self.inner.get(source) {
            Some(cached)
                if cached.schema_version_at_plan == schema_version
                    && cached.registry_version_at_plan == registry_version
                    && cached.profile_identity_at_plan == profile_identity =>
            {
                let plan = Arc::clone(&cached.plan);
                self.stats.hits = self.stats.hits.saturating_add(1);
                trace_cache_event("hit", schema_version, source);
                Some(plan)
            }
            Some(_) => {
                self.inner.pop(source);
                self.stats.stale_invalidations = self.stats.stale_invalidations.saturating_add(1);
                trace_cache_event("stale", schema_version, source);
                None
            }
            None => {
                self.stats.misses = self.stats.misses.saturating_add(1);
                trace_cache_event("miss", schema_version, source);
                None
            }
        }
    }

    pub(crate) fn insert(
        &mut self,
        source: Arc<str>,
        plan: Arc<ExecutionPlan>,
        schema_version: u64,
        registry_version: u64,
        profile_identity: ProfileIdentity,
    ) {
        if !is_cacheable(&plan) {
            return;
        }

        let replacing_existing = self.inner.contains(source.as_ref());
        let key = CacheKey { source };
        let cached = CachedPlan {
            plan,
            schema_version_at_plan: schema_version,
            registry_version_at_plan: registry_version,
            profile_identity_at_plan: profile_identity,
        };
        if self.inner.push(key, cached).is_some() && !replacing_existing {
            self.stats.capacity_evictions = self.stats.capacity_evictions.saturating_add(1);
        }
    }

    /// Return a snapshot of the cache counters.
    #[must_use]
    pub const fn stats(&self) -> PlanCacheStats {
        self.stats
    }

    /// Remove all cached plans while preserving accumulated counters.
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl SharedPlanCache {
    /// Create an empty shared source-plan cache with the given entry capacity.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: Mutex::new(SharedPlanCacheInner {
                plans: LruCache::new(capacity),
                stats: SharedPlanCacheStats::default(),
            }),
        }
    }

    pub(crate) fn get(&self, key: SharedPlanCacheLookup<'_>) -> Option<Arc<ExecutionPlan>> {
        let mut inner = self.lock_inner();
        let key = SharedPlanCacheKey::from_lookup(key);
        match inner.plans.get(&key) {
            Some(plan) => {
                let plan = Arc::clone(plan);
                inner.stats.hits = inner.stats.hits.saturating_add(1);
                Some(plan)
            }
            None => {
                inner.stats.misses = inner.stats.misses.saturating_add(1);
                None
            }
        }
    }

    pub(crate) fn insert(&self, key: SharedPlanCacheInsert, plan: Arc<ExecutionPlan>) {
        if !is_cacheable(&plan) {
            return;
        }

        let mut inner = self.lock_inner();
        let key = SharedPlanCacheKey::from_insert(key);
        let replacing_existing = inner.plans.contains(&key);
        if inner.plans.push(key, plan).is_some() && !replacing_existing {
            inner.stats.capacity_evictions = inner.stats.capacity_evictions.saturating_add(1);
        }
    }

    /// Return a snapshot of the cache counters.
    #[must_use]
    pub fn stats(&self) -> SharedPlanCacheStats {
        self.lock_inner().stats
    }

    /// Remove all cached plans while preserving accumulated counters.
    pub fn clear(&self) {
        self.lock_inner().plans.clear();
    }

    fn lock_inner(&self) -> MutexGuard<'_, SharedPlanCacheInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

pub(crate) struct SharedPlanCacheLookup<'a> {
    pub(crate) graph_id: GraphId,
    pub(crate) schema_version: u64,
    pub(crate) registry_version: u64,
    pub(crate) profile_identity: ProfileIdentity,
    pub(crate) source: &'a str,
    pub(crate) caps: ImplDefinedCaps,
    pub(crate) index_selection: bool,
}

pub(crate) struct SharedPlanCacheInsert {
    pub(crate) graph_id: GraphId,
    pub(crate) schema_version: u64,
    pub(crate) registry_version: u64,
    pub(crate) profile_identity: ProfileIdentity,
    pub(crate) source: Arc<str>,
    pub(crate) caps: ImplDefinedCaps,
    pub(crate) index_selection: bool,
}

impl SharedPlanCacheKey {
    fn from_lookup(value: SharedPlanCacheLookup<'_>) -> Self {
        Self {
            graph_id: value.graph_id,
            schema_version: value.schema_version,
            registry_version: value.registry_version,
            profile_identity: value.profile_identity,
            source: Arc::from(value.source),
            caps: value.caps,
            index_selection: value.index_selection,
        }
    }

    fn from_insert(value: SharedPlanCacheInsert) -> Self {
        Self {
            graph_id: value.graph_id,
            schema_version: value.schema_version,
            registry_version: value.registry_version,
            profile_identity: value.profile_identity,
            source: value.source,
            caps: value.caps,
            index_selection: value.index_selection,
        }
    }
}

fn is_cacheable(plan: &ExecutionPlan) -> bool {
    !contains_call(plan)
}

fn contains_call(plan: &ExecutionPlan) -> bool {
    plan.pipeline.iter().any(op_contains_call)
        || plan.subqueries.iter().any(|subquery| match &subquery.body {
            SubqueryBody::Pattern(_) => false,
            SubqueryBody::Plan(plan) => contains_call(plan),
        })
}

fn op_contains_call(op: &PipelineOp) -> bool {
    match op {
        PipelineOp::Call(_) => true,
        PipelineOp::CallSubquery(subquery) => contains_call(&subquery.body),
        PipelineOp::Union { rhs, .. }
        | PipelineOp::Chain(rhs)
        | PipelineOp::CorrelatedChain(rhs)
        | PipelineOp::ExplainPlan { inner: rhs, .. } => contains_call(rhs),
        PipelineOp::Match(_) | PipelineOp::OptionalMatch(_) => false,
        _ => false,
    }
}

fn trace_cache_event(result: &'static str, schema_version: u64, source: &str) {
    #[cfg(feature = "plan-cache-source-tracing")]
    tracing::debug!(
        result,
        schema_version,
        source_len = source.len(),
        source_prefix = %SourcePrefix(source),
        "session plan cache lookup"
    );

    #[cfg(not(feature = "plan-cache-source-tracing"))]
    tracing::debug!(
        result,
        schema_version,
        source_len = source.len(),
        "session plan cache lookup"
    );
}

#[cfg(feature = "plan-cache-source-tracing")]
struct SourcePrefix<'a>(&'a str);

#[cfg(feature = "plan-cache-source-tracing")]
impl std::fmt::Display for SourcePrefix<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut chars = self.0.chars();
        let prefix = chars.by_ref().take(200).collect::<String>();
        if chars.next().is_some() {
            write!(formatter, "{prefix}...")
        } else {
            formatter.write_str(&prefix)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, sync::Arc};

    use selene_core::{DbString, db_string};
    use selene_profile::{ProfileIdentity, current_profile_identity};

    use super::*;
    use crate::{
        BindingTableSchema, EmptyProcedureRegistry, ExprId, ImplDefinedCaps, PipelineOpId,
        PlannedCall, PlannedSubquery, ProcedureHandle, ProcedureMutability, ProcedureOutputSchema,
        ProcedureTier, SourceSpan, StatementCategory, SubqueryBody, SubqueryKind, analyze::analyze,
        parser::parse, plan::plan,
    };

    fn planned(source: &str) -> Arc<ExecutionPlan> {
        let statement = parse(source).expect("test source parses");
        let analyzed =
            analyze(statement, &EmptyProcedureRegistry, None).expect("test source analyzes");
        Arc::new(plan(&analyzed, &EmptyProcedureRegistry).expect("test source plans"))
    }

    fn admitted(value: &str) -> DbString {
        db_string(value).expect("test name admits")
    }

    fn profile() -> ProfileIdentity {
        current_profile_identity()
    }

    fn call_plan() -> Arc<ExecutionPlan> {
        Arc::new(ExecutionPlan {
            category: StatementCategory::ReadOnly,
            pattern_plan: None,
            pipeline: vec![PipelineOp::Call(PlannedCall {
                optional: false,
                procedure: Box::from([admitted("cache"), admitted("call")]),
                handle: ProcedureHandle::new(1),
                args: Vec::new(),
                yield_cols: Vec::new(),
                output_schema: ProcedureOutputSchema::default(),
                yield_schema: Vec::new(),
                tier: ProcedureTier::Graph,
                mutability: ProcedureMutability::Read,
                span: SourceSpan::default(),
            })],
            output_schema: BindingTableSchema {
                columns: Vec::new(),
            },
            impl_defined_caps: ImplDefinedCaps::default(),
            expr_ids: Default::default(),
            subqueries: Default::default(),
            next_expr_id: ExprId::new(0),
            next_pipeline_op_id: PipelineOpId::new(1),
        })
    }

    fn explain_call_plan() -> Arc<ExecutionPlan> {
        let inner = call_plan();
        Arc::new(ExecutionPlan {
            category: StatementCategory::ReadOnly,
            pattern_plan: None,
            pipeline: vec![PipelineOp::ExplainPlan {
                inner: Box::new(inner.as_ref().clone()),
                span: SourceSpan::default(),
            }],
            output_schema: BindingTableSchema {
                columns: Vec::new(),
            },
            impl_defined_caps: ImplDefinedCaps::default(),
            expr_ids: Default::default(),
            subqueries: Default::default(),
            next_expr_id: ExprId::new(0),
            next_pipeline_op_id: PipelineOpId::new(1),
        })
    }

    fn expression_subquery_call_plan() -> Arc<ExecutionPlan> {
        let mut plan = ExecutionPlan {
            category: StatementCategory::ReadOnly,
            pattern_plan: None,
            pipeline: Vec::new(),
            output_schema: BindingTableSchema {
                columns: Vec::new(),
            },
            impl_defined_caps: ImplDefinedCaps::default(),
            expr_ids: Default::default(),
            subqueries: Default::default(),
            next_expr_id: ExprId::new(0),
            next_pipeline_op_id: PipelineOpId::new(0),
        };
        plan.subqueries.insert(
            ExprId::new(7),
            PlannedSubquery {
                kind: SubqueryKind::Value,
                body: SubqueryBody::Plan(Box::new(call_plan().as_ref().clone())),
                outer_binding_refs: Vec::new(),
                span: SourceSpan::default(),
            },
        );
        Arc::new(plan)
    }

    #[test]
    fn plan_cache_basic_hit_miss() {
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        assert!(cache.get("RETURN 1", 0, 0, profile()).is_none());

        cache.insert(Arc::from("RETURN 1"), planned("RETURN 1"), 0, 0, profile());
        assert!(cache.get("RETURN 1", 0, 0, profile()).is_some());

        assert_eq!(
            cache.stats(),
            PlanCacheStats {
                hits: 1,
                misses: 1,
                stale_invalidations: 0,
                capacity_evictions: 0,
            }
        );
    }

    #[test]
    fn plan_cache_lru_evicts_oldest() {
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        cache.insert(Arc::from("RETURN 1"), planned("RETURN 1"), 0, 0, profile());
        cache.insert(Arc::from("RETURN 2"), planned("RETURN 2"), 0, 0, profile());
        cache.insert(Arc::from("RETURN 3"), planned("RETURN 3"), 0, 0, profile());

        assert!(cache.get("RETURN 1", 0, 0, profile()).is_none());
        assert!(cache.get("RETURN 2", 0, 0, profile()).is_some());
        assert!(cache.get("RETURN 3", 0, 0, profile()).is_some());
        assert_eq!(cache.stats().capacity_evictions, 1);
    }

    #[test]
    fn plan_cache_schema_version_mismatch_is_miss() {
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        cache.insert(Arc::from("RETURN 1"), planned("RETURN 1"), 0, 0, profile());

        assert!(cache.get("RETURN 1", 1, 0, profile()).is_none());
        assert_eq!(cache.stats().stale_invalidations, 1);
        assert!(cache.get("RETURN 1", 1, 0, profile()).is_none());
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn plan_cache_profile_identity_mismatch_invalidates_entry() {
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        cache.insert(Arc::from("RETURN 1"), planned("RETURN 1"), 0, 0, profile());
        let synthetic = ProfileIdentity::new("synthetic", 3, 3, "other-hash");

        assert!(cache.get("RETURN 1", 0, 0, synthetic).is_none());
        assert_eq!(cache.stats().stale_invalidations, 1);
        assert!(cache.get("RETURN 1", 0, 0, profile()).is_none());
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn plan_cache_registry_version_mismatch_invalidates_entry() {
        // F03-PR04: procedure signature changes bump the registry epoch. A
        // session-cached plan pinned to the old epoch must invalidate rather
        // than execute with stale procedure authority.
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        cache.insert(Arc::from("RETURN 1"), planned("RETURN 1"), 0, 0, profile());

        assert!(cache.get("RETURN 1", 0, 1, profile()).is_none());
        assert_eq!(cache.stats().stale_invalidations, 1);
        assert!(cache.get("RETURN 1", 0, 1, profile()).is_none());
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn plan_cache_clear_resets_state() {
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        cache.insert(Arc::from("RETURN 1"), planned("RETURN 1"), 0, 0, profile());
        assert!(cache.get("RETURN 1", 0, 0, profile()).is_some());

        cache.clear();
        assert!(cache.get("RETURN 1", 0, 0, profile()).is_none());

        assert_eq!(
            cache.stats(),
            PlanCacheStats {
                hits: 1,
                misses: 1,
                stale_invalidations: 0,
                capacity_evictions: 0,
            }
        );
    }

    #[test]
    fn cache_skips_call_plans() {
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        cache.insert(Arc::from("CALL cache.call()"), call_plan(), 0, 0, profile());

        assert!(cache.get("CALL cache.call()", 0, 0, profile()).is_none());
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn cache_skips_explain_call_plans() {
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        cache.insert(
            Arc::from("EXPLAIN CALL cache.call()"),
            explain_call_plan(),
            0,
            0,
            profile(),
        );

        assert!(
            cache
                .get("EXPLAIN CALL cache.call()", 0, 0, profile())
                .is_none()
        );
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn cache_skips_expression_subqueries_that_contain_calls() {
        let mut cache = PlanCache::new(NonZeroUsize::new(2).unwrap());
        cache.insert(
            Arc::from("RETURN VALUE { CALL cache.call() RETURN 1 LIMIT 1 } AS v"),
            expression_subquery_call_plan(),
            0,
            0,
            profile(),
        );

        assert!(
            cache
                .get(
                    "RETURN VALUE { CALL cache.call() RETURN 1 LIMIT 1 } AS v",
                    0,
                    0,
                    profile()
                )
                .is_none()
        );
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn shared_plan_cache_hits_across_sessions() {
        let cache = SharedPlanCache::new(NonZeroUsize::new(2).unwrap());
        let lookup = |source| SharedPlanCacheLookup {
            graph_id: selene_core::GraphId::new(7),
            schema_version: 0,
            registry_version: 0,
            profile_identity: profile(),
            source,
            caps: ImplDefinedCaps::DEFAULT,
            index_selection: true,
        };

        assert!(cache.get(lookup("RETURN 1")).is_none());
        cache.insert(
            SharedPlanCacheInsert {
                graph_id: selene_core::GraphId::new(7),
                schema_version: 0,
                registry_version: 0,
                profile_identity: profile(),
                source: Arc::from("RETURN 1"),
                caps: ImplDefinedCaps::DEFAULT,
                index_selection: true,
            },
            planned("RETURN 1"),
        );
        assert!(cache.get(lookup("RETURN 1")).is_some());

        assert_eq!(
            cache.stats(),
            SharedPlanCacheStats {
                hits: 1,
                misses: 1,
                capacity_evictions: 0,
            }
        );
    }

    #[test]
    fn shared_plan_cache_keys_planning_settings() {
        let cache = SharedPlanCache::new(NonZeroUsize::new(4).unwrap());
        let base = SharedPlanCacheInsert {
            graph_id: selene_core::GraphId::new(7),
            schema_version: 0,
            registry_version: 0,
            profile_identity: profile(),
            source: Arc::from("RETURN 1"),
            caps: ImplDefinedCaps::DEFAULT,
            index_selection: true,
        };
        cache.insert(base, planned("RETURN 1"));

        assert!(
            cache
                .get(SharedPlanCacheLookup {
                    graph_id: selene_core::GraphId::new(7),
                    schema_version: 0,
                    registry_version: 0,
                    profile_identity: profile(),
                    source: "RETURN 1",
                    caps: ImplDefinedCaps::DEFAULT,
                    index_selection: true,
                })
                .is_some()
        );
        assert!(
            cache
                .get(SharedPlanCacheLookup {
                    graph_id: selene_core::GraphId::new(7),
                    schema_version: 0,
                    registry_version: 0,
                    profile_identity: ProfileIdentity::new("synthetic", 3, 3, "other-hash"),
                    source: "RETURN 1",
                    caps: ImplDefinedCaps::DEFAULT,
                    index_selection: true,
                })
                .is_none()
        );
        assert!(
            cache
                .get(SharedPlanCacheLookup {
                    graph_id: selene_core::GraphId::new(7),
                    schema_version: 0,
                    registry_version: 0,
                    profile_identity: profile(),
                    source: "RETURN 1",
                    caps: ImplDefinedCaps::DEFAULT.with_max_list_length(1),
                    index_selection: true,
                })
                .is_none()
        );
        assert!(
            cache
                .get(SharedPlanCacheLookup {
                    graph_id: selene_core::GraphId::new(7),
                    schema_version: 0,
                    registry_version: 0,
                    profile_identity: profile(),
                    source: "RETURN 1",
                    caps: ImplDefinedCaps::DEFAULT,
                    index_selection: false,
                })
                .is_none()
        );
    }
}
