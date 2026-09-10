//! Single-scan pattern executor.

use std::ops::Bound::{Excluded, Included, Unbounded};

use selene_core::{DbString, EdgeId, LabelSet, NodeId, Value};
use selene_graph::{CandidateSet, Edge};

use crate::{
    FilterPredicate, FilterPredicateKind, IndexKey, IndexKind, LabelExpr, NodeOrEdgeScan,
    PatternPlan, ScanAccess, ScanKind, TypedIndexBounds,
    runtime::{Binding, BindingTableSchema, ExecutorError},
};

use super::scan_resolve::{
    IndexKeyOutcome, ResolvedBounds, range_satisfiable_runtime, resolve_bitmap_union_key_values,
    resolve_bounds, resolve_index_key,
};
use super::{EvalCtx, evaluator, scan_bind, scan_seed, value_compare};

/// Stable identifier of a node or edge matched during scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScanEntityId {
    Node(NodeId),
    Edge(EdgeId),
}

impl ScanEntityId {
    #[inline]
    pub(super) fn into_value(self) -> Value {
        match self {
            Self::Node(id) => Value::NodeRef(id),
            Self::Edge(id) => Value::EdgeRef(id),
        }
    }
}

fn scan_error(_err: selene_graph::GraphError) -> ExecutorError {
    ExecutorError::ImplementationDefined {
        detail: "graph scan candidate error",
    }
}

/// Execute one `JoinTree::Scan` against the transaction snapshot.
pub(crate) fn scan_pattern(
    scan: &NodeOrEdgeScan,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    seed: Option<&Binding>,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    scan_bindings(scan, pattern, schema, seed, ctx)
}

fn scan_bindings(
    scan: &NodeOrEdgeScan,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    seed: Option<&Binding>,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let slots = scan_bind::ScanSlots::resolve(scan, pattern, schema)?;
    match seed {
        Some(seed) => scan_entities_with_seed(scan, pattern, schema, seed, slots, ctx),
        None => collect_scan_entities(scan, pattern, schema, None, slots, ctx),
    }
}

fn scan_entities_with_seed(
    scan: &NodeOrEdgeScan,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    seed: &Binding,
    slots: scan_bind::ScanSlots,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    if let Some(rows) = scan_seed::try_seeded_scan(scan, pattern, schema, seed, slots, ctx)? {
        return Ok(rows);
    }
    collect_scan_entities(scan, pattern, schema, Some(seed), slots, ctx)
}

fn collect_scan_entities(
    scan: &NodeOrEdgeScan,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    seed: Option<&Binding>,
    slots: scan_bind::ScanSlots,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let candidates = candidate_entities(scan, ctx)?;
    let label_prechecked = label_matched_by_access(scan);
    let mut rows = Vec::with_capacity(candidates.len());
    for entity in candidates {
        if !label_prechecked && !label_matches_scan(scan, entity, ctx) {
            continue;
        }
        let value = entity.into_value();
        let Some(binding) = scan_bind::binding_for_scan(schema, seed, value.clone(), slots) else {
            continue;
        };
        if predicates_pass(scan, pattern, &binding, schema, &value, ctx)? {
            rows.push(binding);
        }
    }
    Ok(rows)
}

fn label_matched_by_access(scan: &NodeOrEdgeScan) -> bool {
    matches!(scan.access, ScanAccess::LabelIndex { .. })
        && single_label(&scan.label_predicate).is_some()
}

pub(super) fn candidate_entities(
    scan: &NodeOrEdgeScan,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<ScanEntityId>, ExecutorError> {
    match &scan.access {
        ScanAccess::Linear => linear_entities(scan.kind, ctx),
        ScanAccess::LabelIndex { .. } => label_index_entities(scan, ctx),
        ScanAccess::TypedIndexRange {
            property,
            kind,
            bounds,
            ..
        } => typed_index_entities(scan, property, *kind, bounds, ctx),
        ScanAccess::BitmapUnion {
            property,
            kind,
            keys,
            ..
        } => bitmap_union_entities(scan, property, *kind, keys, ctx),
        ScanAccess::CompositeLookup {
            properties, keys, ..
        } => composite_lookup_entities(scan, properties, keys, ctx),
    }
}

pub(super) fn candidate_edge_set(
    scan: &NodeOrEdgeScan,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<CandidateSet<Edge>, ExecutorError> {
    let entities = candidate_entities(scan, ctx)?;
    let edge_ids = entities.into_iter().filter_map(|e| match e {
        ScanEntityId::Edge(id) => Some(id),
        ScanEntityId::Node(_) => None,
    });
    ctx.tx
        .snapshot()
        .bind_edge_candidates(edge_ids)
        .map_err(scan_error)
}

fn linear_entities(
    kind: ScanKind,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<ScanEntityId>, ExecutorError> {
    let snapshot = ctx.tx.snapshot();
    match kind {
        ScanKind::Node => Ok(snapshot
            .live_node_candidates()
            .map_err(scan_error)?
            .iter()
            .map(ScanEntityId::Node)
            .collect()),
        ScanKind::Edge => Ok(snapshot
            .live_edge_candidates()
            .map_err(scan_error)?
            .iter()
            .map(ScanEntityId::Edge)
            .collect()),
    }
}

pub(super) fn label_index_entities(
    scan: &NodeOrEdgeScan,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<ScanEntityId>, ExecutorError> {
    let Some(label) = single_label(&scan.label_predicate) else {
        return linear_entities(scan.kind, ctx);
    };
    let snapshot = ctx.tx.snapshot();
    match scan.kind {
        ScanKind::Node => Ok(snapshot
            .node_candidates_with_label(label)
            .map_err(scan_error)?
            .iter()
            .map(ScanEntityId::Node)
            .collect()),
        ScanKind::Edge => Ok(snapshot
            .edge_candidates_with_label(label)
            .map_err(scan_error)?
            .iter()
            .map(ScanEntityId::Edge)
            .collect()),
    }
}

fn typed_index_entities(
    scan: &NodeOrEdgeScan,
    property: &DbString,
    kind: IndexKind,
    bounds: &TypedIndexBounds,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<ScanEntityId>, ExecutorError> {
    let Some(resolved) = resolve_bounds(bounds, kind, ctx)? else {
        return Ok(Vec::new());
    };
    super::scan_duration::validate_bounds(scan, property, &resolved, ctx)?;
    if !range_satisfiable_runtime(&resolved) {
        return Ok(Vec::new());
    }
    let Some(label) = single_label(&scan.label_predicate) else {
        return Ok(linear_entities(scan.kind, ctx)?
            .into_iter()
            .filter(|e| entity_matches_resolved_bounds(*e, property, &resolved, ctx))
            .collect());
    };
    let snapshot = ctx.tx.snapshot();
    let indexed = match &resolved {
        ResolvedBounds::Equality(value) => match scan.kind {
            ScanKind::Node => snapshot
                .node_candidates_with_property_eq(label, property, value)
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Node).collect()),
            ScanKind::Edge => snapshot
                .edge_candidates_with_property_eq(label, property, value)
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Edge).collect()),
        },
        ResolvedBounds::GreaterThan(value) => match scan.kind {
            ScanKind::Node => snapshot
                .node_candidates_with_property_range(
                    label,
                    property,
                    (Excluded(value.clone()), Unbounded),
                )
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Node).collect()),
            ScanKind::Edge => snapshot
                .edge_candidates_with_property_range(
                    label,
                    property,
                    (Excluded(value.clone()), Unbounded),
                )
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Edge).collect()),
        },
        ResolvedBounds::GreaterEqual(value) => match scan.kind {
            ScanKind::Node => snapshot
                .node_candidates_with_property_range(
                    label,
                    property,
                    (Included(value.clone()), Unbounded),
                )
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Node).collect()),
            ScanKind::Edge => snapshot
                .edge_candidates_with_property_range(
                    label,
                    property,
                    (Included(value.clone()), Unbounded),
                )
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Edge).collect()),
        },
        ResolvedBounds::LessThan(value) => match scan.kind {
            ScanKind::Node => snapshot
                .node_candidates_with_property_range(
                    label,
                    property,
                    (Unbounded, Excluded(value.clone())),
                )
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Node).collect()),
            ScanKind::Edge => snapshot
                .edge_candidates_with_property_range(
                    label,
                    property,
                    (Unbounded, Excluded(value.clone())),
                )
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Edge).collect()),
        },
        ResolvedBounds::LessEqual(value) => match scan.kind {
            ScanKind::Node => snapshot
                .node_candidates_with_property_range(
                    label,
                    property,
                    (Unbounded, Included(value.clone())),
                )
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Node).collect()),
            ScanKind::Edge => snapshot
                .edge_candidates_with_property_range(
                    label,
                    property,
                    (Unbounded, Included(value.clone())),
                )
                .map_err(scan_error)?
                .map(|c| c.iter().map(ScanEntityId::Edge).collect()),
        },
        ResolvedBounds::Range {
            lo,
            lo_inclusive,
            hi,
            hi_inclusive,
        } => {
            let lo_bound = if *lo_inclusive {
                Included(lo.clone())
            } else {
                Excluded(lo.clone())
            };
            let hi_bound = if *hi_inclusive {
                Included(hi.clone())
            } else {
                Excluded(hi.clone())
            };
            match scan.kind {
                ScanKind::Node => snapshot
                    .node_candidates_with_property_range(label, property, (lo_bound, hi_bound))
                    .map_err(scan_error)?
                    .map(|c| c.iter().map(ScanEntityId::Node).collect()),
                ScanKind::Edge => snapshot
                    .edge_candidates_with_property_range(label, property, (lo_bound, hi_bound))
                    .map_err(scan_error)?
                    .map(|c| c.iter().map(ScanEntityId::Edge).collect()),
            }
        }
    };
    if let Some(entities) = indexed {
        Ok(entities)
    } else {
        Ok(linear_entities(scan.kind, ctx)?
            .into_iter()
            .filter(|e| entity_matches_resolved_bounds(*e, property, &resolved, ctx))
            .collect())
    }
}

fn bitmap_union_entities(
    scan: &NodeOrEdgeScan,
    property: &DbString,
    kind: IndexKind,
    keys: &[IndexKey],
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<ScanEntityId>, ExecutorError> {
    let mut resolved_keys: Vec<Value> = Vec::with_capacity(keys.len());
    for key in keys {
        resolved_keys.extend(resolve_bitmap_union_key_values(key, kind, ctx)?);
    }
    if resolved_keys.is_empty() && !keys.is_empty() {
        return Ok(Vec::new());
    }
    super::scan_duration::validate(
        scan,
        property,
        &resolved_keys.iter().collect::<Vec<_>>(),
        ctx,
    )?;
    let Some(label) = single_label(&scan.label_predicate) else {
        return Ok(linear_entities(scan.kind, ctx)?
            .into_iter()
            .filter(|e| entity_matches_any_resolved(*e, property, &resolved_keys, ctx))
            .collect());
    };
    let snapshot = ctx.tx.snapshot();
    let indexed = match scan.kind {
        ScanKind::Node => snapshot
            .node_candidates_with_property_any(label, property, &resolved_keys)
            .map_err(scan_error)?
            .map(|c| c.iter().map(ScanEntityId::Node).collect()),
        ScanKind::Edge => snapshot
            .edge_candidates_with_property_any(label, property, &resolved_keys)
            .map_err(scan_error)?
            .map(|c| c.iter().map(ScanEntityId::Edge).collect()),
    };
    if let Some(entities) = indexed {
        Ok(entities)
    } else {
        Ok(linear_entities(scan.kind, ctx)?
            .into_iter()
            .filter(|e| entity_matches_any_resolved(*e, property, &resolved_keys, ctx))
            .collect())
    }
}

fn composite_lookup_entities(
    scan: &NodeOrEdgeScan,
    properties: &[(DbString, IndexKind)],
    keys: &[(DbString, IndexKey)],
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<ScanEntityId>, ExecutorError> {
    let Some(resolved_values) = resolve_composite_values(properties, keys, ctx)? else {
        return Ok(Vec::new());
    };
    super::scan_duration::validate_composite(scan, properties, &resolved_values, ctx)?;
    if scan.kind != ScanKind::Node {
        return Ok(linear_entities(scan.kind, ctx)?
            .into_iter()
            .filter(|e| entity_matches_resolved_composite(*e, properties, &resolved_values, ctx))
            .collect());
    }
    let Some(label) = single_label(&scan.label_predicate) else {
        return Ok(linear_entities(scan.kind, ctx)?
            .into_iter()
            .filter(|e| entity_matches_resolved_composite(*e, properties, &resolved_values, ctx))
            .collect());
    };
    let property_keys: Vec<DbString> = properties
        .iter()
        .map(|(property, _)| property.clone())
        .collect();
    let snapshot = ctx.tx.snapshot();
    if let Some(candidates) = snapshot
        .node_candidates_with_composite_key(label, &property_keys, &resolved_values)
        .map_err(scan_error)?
    {
        return Ok(candidates.iter().map(ScanEntityId::Node).collect());
    }
    Ok(linear_entities(scan.kind, ctx)?
        .into_iter()
        .filter(|e| entity_matches_resolved_composite(*e, properties, &resolved_values, ctx))
        .collect())
}

/// Resolve a composite probe's per-component keys against bound parameters.
pub(super) fn resolve_composite_values(
    properties: &[(DbString, IndexKind)],
    keys: &[(DbString, IndexKey)],
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Option<Vec<Value>>, ExecutorError> {
    let mut out = Vec::with_capacity(properties.len());
    for (property, kind) in properties {
        let Some((_, key)) = keys.iter().find(|(name, _)| name == property) else {
            return Ok(None);
        };
        match resolve_index_key(key, *kind, ctx)? {
            IndexKeyOutcome::Value(value) => out.push(value),
            IndexKeyOutcome::EmptyResult => return Ok(None),
        }
    }
    Ok(Some(out))
}

pub(super) fn entity_matches_resolved_composite(
    entity: ScanEntityId,
    properties: &[(DbString, IndexKind)],
    values: &[Value],
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> bool {
    properties
        .iter()
        .zip(values.iter())
        .all(|((property, _), expected)| {
            entity_property_value(entity, property, ctx)
                .is_some_and(|value| value_eq_non_null(value, expected))
        })
}

pub(super) fn entity_matches_resolved_bounds(
    entity: ScanEntityId,
    property: &DbString,
    resolved: &ResolvedBounds,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> bool {
    entity_property_value(entity, property, ctx)
        .is_some_and(|value| value_matches_resolved_bounds(value, resolved))
}

pub(super) fn entity_matches_any_resolved(
    entity: ScanEntityId,
    property: &DbString,
    values: &[Value],
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> bool {
    entity_property_value(entity, property, ctx).is_some_and(|value| {
        values
            .iter()
            .any(|expected| value_eq_non_null(value, expected))
    })
}

pub(super) fn entity_property_value<'a>(
    entity: ScanEntityId,
    property: &DbString,
    ctx: &'a EvalCtx<'_, '_, '_, '_>,
) -> Option<&'a Value> {
    let snapshot = ctx.tx.snapshot();
    match entity {
        ScanEntityId::Node(id) => snapshot
            .node_properties(id)
            .and_then(|properties| properties.get(property)),
        ScanEntityId::Edge(id) => snapshot
            .edge_properties(id)
            .and_then(|properties| properties.get(property)),
    }
}

fn value_matches_resolved_bounds(value: &Value, resolved: &ResolvedBounds) -> bool {
    match resolved {
        ResolvedBounds::Equality(expected) => value_eq_non_null(value, expected),
        ResolvedBounds::GreaterThan(expected) => {
            value_compare::compare_non_null(value, expected) == Some(std::cmp::Ordering::Greater)
        }
        ResolvedBounds::GreaterEqual(expected) => matches!(
            value_compare::compare_non_null(value, expected),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
        ResolvedBounds::LessThan(expected) => {
            value_compare::compare_non_null(value, expected) == Some(std::cmp::Ordering::Less)
        }
        ResolvedBounds::LessEqual(expected) => matches!(
            value_compare::compare_non_null(value, expected),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
        ResolvedBounds::Range {
            lo,
            lo_inclusive,
            hi,
            hi_inclusive,
        } => {
            let Some(lo_order) = value_compare::compare_non_null(value, lo) else {
                return false;
            };
            let Some(hi_order) = value_compare::compare_non_null(value, hi) else {
                return false;
            };
            let lo_ok = if *lo_inclusive {
                matches!(
                    lo_order,
                    std::cmp::Ordering::Greater | std::cmp::Ordering::Equal
                )
            } else {
                lo_order == std::cmp::Ordering::Greater
            };
            let hi_ok = if *hi_inclusive {
                matches!(
                    hi_order,
                    std::cmp::Ordering::Less | std::cmp::Ordering::Equal
                )
            } else {
                hi_order == std::cmp::Ordering::Less
            };
            lo_ok && hi_ok
        }
    }
}

fn value_eq_non_null(lhs: &Value, rhs: &Value) -> bool {
    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
        return false;
    }
    value_compare::equal_non_null(lhs, rhs)
}

#[inline]
pub(crate) fn predicates_pass(
    scan: &NodeOrEdgeScan,
    pattern: &PatternPlan,
    binding: &Binding,
    schema: &BindingTableSchema,
    entity: &Value,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    for predicate in &scan.property_predicates {
        if !predicate_passes(predicate, pattern, binding, schema, entity, ctx)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn predicate_passes(
    predicate: &FilterPredicate,
    pattern: &PatternPlan,
    binding: &Binding,
    schema: &BindingTableSchema,
    entity: &Value,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    if predicate.index_consumed {
        return Ok(true);
    }
    match &predicate.kind {
        FilterPredicateKind::Expression => {
            let value = evaluator::evaluate(&predicate.expr, binding, schema, ctx)?;
            Ok(matches!(value, Value::Bool(true)))
        }
        FilterPredicateKind::PropertyEquals {
            binding: property_binding,
            key,
        } => {
            let target = property_binding
                .and_then(|binding_id| value_for_binding(pattern, binding_id, binding, schema))
                .unwrap_or_else(|| entity.clone());
            let property = match &target {
                Value::NodeRef(id) => ctx
                    .tx
                    .snapshot()
                    .node_properties(*id)
                    .and_then(|properties| properties.get(key))
                    .cloned(),
                Value::EdgeRef(id) => ctx
                    .tx
                    .snapshot()
                    .edge_properties(*id)
                    .and_then(|properties| properties.get(key))
                    .cloned(),
                Value::Null => None,
                _ => None,
            }
            .unwrap_or(Value::Null);
            let expected = evaluator::evaluate(&predicate.expr, binding, schema, ctx)?;
            Ok(value_eq_non_null(&property, &expected))
        }
    }
}

pub(crate) fn value_for_binding(
    pattern: &PatternPlan,
    binding_id: crate::BindingId,
    binding: &Binding,
    schema: &BindingTableSchema,
) -> Option<Value> {
    let binding_def = pattern
        .bindings
        .iter()
        .find(|candidate| candidate.binding == binding_id)?;
    let index = schema
        .columns
        .iter()
        .position(|column| column.name == Some(binding_def.name.clone()))?;
    binding.get(index).cloned()
}

#[inline]
pub(super) fn label_matches_scan(
    scan: &NodeOrEdgeScan,
    entity: ScanEntityId,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> bool {
    let Some(label_expr) = &scan.label_predicate else {
        return true;
    };
    let snapshot = ctx.tx.snapshot();
    match entity {
        ScanEntityId::Node(id) => snapshot
            .node_labels(id)
            .is_some_and(|labels| label_matches_node(label_expr, labels)),
        ScanEntityId::Edge(id) => snapshot
            .edge_label(id)
            .is_some_and(|label| label_matches_edge(label_expr, label)),
    }
}

pub(crate) fn label_matches_node(expr: &LabelExpr, labels: &LabelSet) -> bool {
    match expr {
        LabelExpr::Single(label) => labels.contains(label),
        LabelExpr::Conjunction(parts) => parts.iter().all(|part| label_matches_node(part, labels)),
        LabelExpr::Disjunction(parts) => parts.iter().any(|part| label_matches_node(part, labels)),
        LabelExpr::Negation(part) => !label_matches_node(part, labels),
        LabelExpr::Wildcard => true,
    }
}

pub(crate) fn label_matches_edge(expr: &LabelExpr, label: &DbString) -> bool {
    match expr {
        LabelExpr::Single(expected) => expected == label,
        LabelExpr::Conjunction(parts) => parts.iter().all(|part| label_matches_edge(part, label)),
        LabelExpr::Disjunction(parts) => parts.iter().any(|part| label_matches_edge(part, label)),
        LabelExpr::Negation(part) => !label_matches_edge(part, label),
        LabelExpr::Wildcard => true,
    }
}

pub(super) fn single_label(label: &Option<LabelExpr>) -> Option<&DbString> {
    match label {
        Some(LabelExpr::Single(label)) => Some(label),
        _ => None,
    }
}
