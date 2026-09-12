//! Seed-bound scan short-circuit for correlated pattern execution.

use selene_core::Value;

use crate::{
    NodeOrEdgeScan, PatternPlan, ScanAccess, ScanKind,
    runtime::{Binding, BindingTableSchema, EvalCtx, ExecutorError},
};

use super::{
    scan::{self, ScanEntityId},
    scan_bind,
    scan_resolve::{range_satisfiable_runtime, resolve_bitmap_union_key_values, resolve_bounds},
};

/// Seed-bound scan short-circuit for correlated pattern execution.
///
/// Shared by the row scan and the batch scan: when the seed already binds the
/// scanned variable to a live entity, the result is at most that single entity
/// (checked against label, access, and property predicates). `None` falls
/// through to the general candidate walk with seed unification.
pub(crate) fn try_seeded_scan(
    scan: &NodeOrEdgeScan,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    seed: &Binding,
    slots: scan_bind::ScanSlots,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Option<Vec<Binding>>, ExecutorError> {
    let Some(index) = slots.binding_index() else {
        return Ok(None);
    };
    let Some(seed_value) = seed.get(index) else {
        return Ok(None);
    };
    let Some(entity) = seeded_entity(scan.kind, seed_value, ctx) else {
        return Ok(None);
    };
    if !scan::label_matches_scan(scan, entity, ctx) || !value_constraint_passes(scan, entity, ctx)?
    {
        return Ok(Some(Vec::new()));
    }
    let val = entity.into_value();
    let Some(binding) = scan_bind::binding_for_scan(schema, Some(seed), val.clone(), slots) else {
        return Ok(Some(Vec::new()));
    };
    if !scan::predicates_pass(scan, pattern, &binding, schema, &val, ctx)? {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(vec![binding]))
}

fn seeded_entity(
    kind: ScanKind,
    seed_value: &Value,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Option<ScanEntityId> {
    let snapshot = ctx.tx.snapshot();
    match (kind, seed_value) {
        (ScanKind::Node, Value::NodeRef(id)) => snapshot
            .is_node_alive(*id)
            .then_some(ScanEntityId::Node(*id)),
        (ScanKind::Edge, Value::EdgeRef(id)) => snapshot
            .is_edge_alive(*id)
            .then_some(ScanEntityId::Edge(*id)),
        _ => None,
    }
}

fn value_constraint_passes(
    scan: &NodeOrEdgeScan,
    entity: ScanEntityId,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    match &scan.access {
        ScanAccess::Linear | ScanAccess::LabelIndex { .. } => Ok(true),
        ScanAccess::TypedIndexRange {
            property,
            kind,
            bounds,
            ..
        } => {
            let Some(resolved) = resolve_bounds(bounds, *kind, ctx)? else {
                return Ok(false);
            };
            Ok(range_satisfiable_runtime(&resolved)
                && scan::entity_matches_resolved_bounds(entity, property, &resolved, ctx))
        }
        ScanAccess::BitmapUnion {
            property,
            kind,
            keys,
            ..
        } => {
            let mut resolved = Vec::with_capacity(keys.len());
            for key in keys {
                resolved.extend(resolve_bitmap_union_key_values(key, *kind, ctx)?);
            }
            Ok((!resolved.is_empty() || keys.is_empty())
                && scan::entity_matches_any_resolved(entity, property, &resolved, ctx))
        }
        ScanAccess::CompositeLookup {
            properties, keys, ..
        } => {
            let Some(values) = scan::resolve_composite_values(properties, keys, ctx)? else {
                return Ok(false);
            };
            Ok(scan::entity_matches_resolved_composite(
                entity, properties, &values, ctx,
            ))
        }
    }
}
