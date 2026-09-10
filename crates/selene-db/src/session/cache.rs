//! Private selected-facade dependency stamp and one-entry plan cache.

use selene_gql::{PreparedCatalogPlan, ProcedureRegistry};

use crate::{
    CatalogGeneration, CatalogReadSnapshot, DatabaseId, GraphId, ProfileIdentity, Result, SchemaId,
    Type, session::Session,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RequestPlanKey {
    pub(crate) source: String,
    pub(crate) parameter_types: Vec<(String, Type)>,
}

/// Stable lexical defaults and non-catalog compilation environment.
///
/// Actual catalog/schema dependencies are validated from the semantic plan.
/// Data publication generations are deliberately not semantic invalidation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DependencyStamp {
    pub(crate) database: DatabaseId,
    pub(crate) schema: CatalogIdentityStamp<SchemaId>,
    pub(crate) graph: GraphDependencyStamp,
    pub(crate) procedure_registry_version: u64,
    pub(crate) profile: ProfileStamp,
    pub(crate) characteristic_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CatalogIdentityStamp<I> {
    id: I,
    created_at: CatalogGeneration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GraphDependencyStamp {
    id: GraphId,
    created_at: CatalogGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProfileStamp {
    profile_id: String,
    source_format_version: u32,
    generator_version: u32,
    canonical_hash: String,
}

impl From<&ProfileIdentity> for ProfileStamp {
    fn from(profile: &ProfileIdentity) -> Self {
        Self {
            profile_id: profile.profile_id().to_owned(),
            source_format_version: profile.source_format_version(),
            generator_version: profile.generator_version(),
            canonical_hash: profile.canonical_hash().to_owned(),
        }
    }
}

struct CacheEntry {
    key: RequestPlanKey,
    stamp: DependencyStamp,
    plan: PreparedCatalogPlan,
}

#[derive(Default)]
pub(crate) struct SessionPlanCache {
    entry: Option<CacheEntry>,
    #[cfg(test)]
    hits: u64,
    #[cfg(test)]
    misses: u64,
}

impl SessionPlanCache {
    pub(crate) fn lookup(
        &mut self,
        key: &RequestPlanKey,
        stamp: &DependencyStamp,
        valid: impl FnOnce(&PreparedCatalogPlan) -> bool,
    ) -> Option<PreparedCatalogPlan> {
        let matched = self
            .entry
            .as_ref()
            .filter(|entry| entry.key == *key && entry.stamp == *stamp)
            .filter(|entry| valid(&entry.plan))
            .map(|entry| entry.plan.clone());
        #[cfg(test)]
        if matched.is_some() {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        matched
    }

    pub(crate) fn insert(
        &mut self,
        key: RequestPlanKey,
        stamp: DependencyStamp,
        plan: PreparedCatalogPlan,
    ) {
        self.entry = Some(CacheEntry { key, stamp, plan });
    }

    pub(crate) fn clear(&mut self) {
        self.entry = None;
    }

    #[cfg(test)]
    pub(crate) const fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

impl Session {
    pub(super) fn capture_dependency_stamp(&self) -> Result<DependencyStamp> {
        let snapshot = CatalogReadSnapshot {
            state: self.inner.state.load_full(),
        };
        self.validate_context_snapshot(&snapshot)?;
        let schema = self.context.current_schema();
        let graph = self.context.current_graph();
        Ok(DependencyStamp {
            database: self.inner.database_id,
            schema: CatalogIdentityStamp {
                id: schema.id,
                created_at: schema.created_at,
            },
            graph: GraphDependencyStamp {
                id: graph.id,
                created_at: graph.created_at,
            },
            procedure_registry_version: self.inner.procedures.registry_version(),
            profile: ProfileStamp::from(self.context.profile_identity()),
            characteristic_epoch: self.context.characteristic_epoch(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> DependencyStamp {
        let profile = ProfileStamp::from(&ProfileIdentity::current());
        DependencyStamp {
            database: DatabaseId::from_raw(1),
            schema: CatalogIdentityStamp {
                id: SchemaId(2),
                created_at: CatalogGeneration::from_raw(3),
            },
            graph: GraphDependencyStamp {
                id: GraphId(4),
                created_at: CatalogGeneration::from_raw(5),
            },
            procedure_registry_version: 10,
            profile,
            characteristic_epoch: 11,
        }
    }

    #[test]
    fn every_execution_dependency_participates_in_equality() {
        let base = stamp();
        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.database = DatabaseId::from_raw(2);
        variants.push(changed);
        let mut changed = base.clone();
        changed.schema.id = SchemaId(changed.schema.id.get() + 1);
        variants.push(changed);
        let mut changed = base.clone();
        changed.schema.created_at = changed.schema.created_at.checked_next().unwrap();
        variants.push(changed);
        let mut changed = base.clone();
        changed.graph.id = GraphId(changed.graph.id.get() + 1);
        variants.push(changed);
        let mut changed = base.clone();
        changed.graph.created_at = changed.graph.created_at.checked_next().unwrap();
        variants.push(changed);
        let mut changed = base.clone();
        changed.procedure_registry_version += 1;
        variants.push(changed);
        let mut changed = base.clone();
        changed.profile.canonical_hash.push('0');
        variants.push(changed);
        let mut changed = base.clone();
        changed.characteristic_epoch += 1;
        variants.push(changed);

        assert!(variants.into_iter().all(|changed| changed != base));
    }
}
