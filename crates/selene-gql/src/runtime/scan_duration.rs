//! Duration indexes share a physical kind across two comparison unit groups.
//! Validate the actual indexed domain before a consumed predicate can omit an
//! incomparable row. Complete indexes inspect distinct keys, not primary rows.

use selene_core::{
    ComparisonMode, DbString, DurationOrderKey, Value, duration_keys_comparable, duration_order_key,
};
use selene_graph::{CompositeKeyComponent, TypedIndex};

use super::{
    DataExceptionSubclass, EvalCtx, ExecutorError, comparison_domain, scan,
    scan_resolve::ResolvedBounds,
};
use crate::{IndexKind, NodeOrEdgeScan, ScanKind, SourceSpan};

pub(super) fn validate_bounds(
    scan: &NodeOrEdgeScan,
    property: &DbString,
    bounds: &ResolvedBounds,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<(), ExecutorError> {
    match bounds {
        ResolvedBounds::Range { lo, hi, .. } => validate(scan, property, &[lo, hi], ctx),
        ResolvedBounds::Equality(value)
        | ResolvedBounds::GreaterThan(value)
        | ResolvedBounds::GreaterEqual(value)
        | ResolvedBounds::LessThan(value)
        | ResolvedBounds::LessEqual(value) => validate(scan, property, &[value], ctx),
    }
}

pub(super) fn validate(
    scan: &NodeOrEdgeScan,
    property: &DbString,
    probes: &[&Value],
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<(), ExecutorError> {
    if !probes
        .iter()
        .any(|value| matches!(value, Value::Duration(_)))
    {
        return Ok(());
    }
    if let Some(label) = scan::single_label(&scan.label_predicate) {
        let snapshot = ctx.tx.snapshot();
        let index = match scan.kind {
            ScanKind::Node => snapshot.property_index_for(label, property),
            ScanKind::Edge => snapshot.edge_property_index_for(label, property),
        };
        if let Some(index) = index
            && let TypedIndex::Duration(entries) = index.as_ref()
        {
            for key in entries.keys() {
                validate_key(*key, probes)?;
            }
            return Ok(());
        }
    }
    // A stale plan or incomplete index can require primary-value fallback.
    for entity in scan::label_index_entities(scan, ctx)? {
        if !scan::label_matches_scan(scan, entity, ctx) {
            continue;
        }
        if let Some(value) = scan::entity_property_value(entity, property, ctx) {
            for probe in probes {
                comparison_domain::ensure_pair(
                    value,
                    probe,
                    ComparisonMode::Ordering,
                    SourceSpan::default(),
                )?;
            }
        }
    }
    Ok(())
}

pub(super) fn validate_composite(
    scan: &NodeOrEdgeScan,
    properties: &[(DbString, IndexKind)],
    values: &[Value],
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<(), ExecutorError> {
    if !values
        .iter()
        .any(|value| matches!(value, Value::Duration(_)))
    {
        return Ok(());
    }
    let names: Vec<_> = properties.iter().map(|(name, _)| name.clone()).collect();
    let snapshot = ctx.tx.snapshot();
    if scan.kind == ScanKind::Node
        && let Some(label) = scan::single_label(&scan.label_predicate)
        && let Some(entry) = snapshot.composite_property_index_entry_for(label, &names)
        && let Some(index) = entry.probe_arc()
    {
        for (name, value) in names.iter().zip(values) {
            if !matches!(value, Value::Duration(_)) {
                continue;
            }
            let position = entry
                .declared_properties
                .iter()
                .position(|field| field == name)
                .expect("registered property");
            for (key, _) in index.entries() {
                if let CompositeKeyComponent::Duration(duration) = key[position] {
                    validate_key(duration, &[value])?;
                }
            }
        }
        return Ok(());
    }
    for ((property, _), value) in properties.iter().zip(values) {
        validate(scan, property, &[value], ctx)?;
    }
    Ok(())
}

fn validate_key(key: DurationOrderKey, probes: &[&Value]) -> Result<(), ExecutorError> {
    for probe in probes {
        if let Value::Duration(span) = probe
            && !duration_keys_comparable(key, duration_order_key(span))
        {
            return Err(ExecutorError::DataException {
                subclass: DataExceptionSubclass::ValuesNotComparable,
                message: "duration operands belong to different unit groups".into(),
                span: SourceSpan::default(),
            });
        }
    }
    Ok(())
}
