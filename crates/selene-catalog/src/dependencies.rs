//! Whole-snapshot dependency checks, including iterative cycle detection.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    CatalogDescriptor, CatalogError, CatalogObjectId, CatalogPayload, CatalogResult,
    DeclarationState, IndexConfiguration,
};

pub(crate) fn validate(
    descriptors: &BTreeMap<CatalogObjectId, CatalogDescriptor>,
) -> CatalogResult<()> {
    let mut incoming = BTreeMap::new();
    let mut dependants: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for descriptor in descriptors.values() {
        let Some(metadata) = descriptor.payload().declaration_metadata() else {
            continue;
        };
        incoming.insert(descriptor.id(), metadata.dependencies.len());
        for dependency in &metadata.dependencies {
            let fail = |reason| CatalogError::InvalidDependency {
                object: descriptor.id(),
                target: dependency.id,
                reason,
            };
            let target = descriptors
                .get(&dependency.id)
                .ok_or_else(|| fail("missing_dependency"))?;
            if target.generation() != dependency.generation {
                return Err(fail("stale_dependency"));
            }
            if let Some(required) = target.payload().declaration_metadata() {
                if metadata.profile != required.profile {
                    return Err(fail("incompatible_profile"));
                }
                if metadata.state == DeclarationState::Ready
                    && required.state != DeclarationState::Ready
                {
                    return Err(fail("dependency_not_ready"));
                }
            }
            dependants
                .entry(dependency.id)
                .or_default()
                .push(descriptor.id());
        }
        if let CatalogPayload::Constraint(constraint) = descriptor.payload()
            && let Some(index_id) = constraint.backing_index
        {
            let id = CatalogObjectId::Index(index_id);
            let fail = |reason| CatalogError::InvalidDependency {
                object: descriptor.id(),
                target: id,
                reason,
            };
            if !metadata
                .dependencies
                .iter()
                .any(|dependency| dependency.id == id)
            {
                return Err(fail("undeclared_backing_dependency"));
            }
            let target = descriptors
                .get(&id)
                .ok_or_else(|| fail("missing_backing_index"))?;
            let CatalogPayload::Index(index) = target.payload() else {
                return Err(fail("wrong_backing_kind"));
            };
            if target.parent() != descriptor.parent() || index.target != constraint.target {
                return Err(fail("wrong_backing_owner_or_target"));
            }
            if !matches!(&index.configuration, IndexConfiguration::Constraint { declaring_type }
                if declaring_type == &constraint.declaring_type)
            {
                return Err(fail("incomplete_backing_kind"));
            }
        }
    }
    let mut roots: BTreeSet<_> = descriptors
        .keys()
        .filter(|id| incoming.get(id).copied().unwrap_or(0) == 0)
        .copied()
        .collect();
    while let Some(id) = roots.pop_first() {
        incoming.remove(&id);
        for dependant in dependants.get(&id).into_iter().flatten() {
            if let Some(count) = incoming.get_mut(dependant) {
                *count -= 1;
                if *count == 0 {
                    roots.insert(*dependant);
                }
            }
        }
    }
    if let Some((&id, _)) = incoming.first_key_value() {
        return Err(CatalogError::InvalidDependency {
            object: id,
            target: id,
            reason: "dependency_cycle",
        });
    }
    Ok(())
}
