//! Commit-path maintenance for built-in node property indexes.
//!
//! Open-graph data can violate a registered index kind. Mutation-time
//! maintenance logs and skips the single offending `(label, property, value)`
//! update so commits still publish. Registration-time index construction is
//! stricter and returns a typed error because silently creating a partial index
//! would be harder to reason about.

use rustc_hash::FxHashMap;
use selene_core::{DbString, LabelSet, PropertyMap, Value};
use std::collections::BTreeSet;

use crate::error::{GraphError, GraphResult};
use crate::graph::PropertyIndexEntry;
use crate::typed_index::{TypedIndex, TypedIndexKind, TypedIndexValueError};

type PropertyIndexMap = FxHashMap<(DbString, DbString), PropertyIndexEntry>;

pub(crate) fn apply_node_create(
    indexes: &mut PropertyIndexMap,
    labels: &LabelSet,
    props: &PropertyMap,
    row: u32,
) -> GraphResult<()> {
    for label in labels.iter() {
        for (property, value) in props.iter() {
            if is_null(value) {
                continue;
            }
            insert_commit(indexes, label.clone(), property.clone(), value, row)?;
        }
    }
    Ok(())
}

pub(crate) fn apply_node_delete(
    indexes: &mut PropertyIndexMap,
    labels: &LabelSet,
    props: &PropertyMap,
    row: u32,
) -> GraphResult<()> {
    for label in labels.iter() {
        for (property, value) in props.iter() {
            if is_null(value) {
                continue;
            }
            remove_commit(indexes, label.clone(), property.clone(), value, row)?;
        }
    }
    Ok(())
}

pub(crate) fn apply_edge_create(
    indexes: &mut PropertyIndexMap,
    label: &DbString,
    props: &PropertyMap,
    row: u32,
) -> GraphResult<()> {
    for (property, value) in props.iter() {
        if is_null(value) {
            continue;
        }
        insert_commit(indexes, label.clone(), property.clone(), value, row)?;
    }
    Ok(())
}

pub(crate) fn apply_edge_delete(
    indexes: &mut PropertyIndexMap,
    label: &DbString,
    props: &PropertyMap,
    row: u32,
) -> GraphResult<()> {
    for (property, value) in props.iter() {
        if is_null(value) {
            continue;
        }
        remove_commit(indexes, label.clone(), property.clone(), value, row)?;
    }
    Ok(())
}

pub(crate) fn apply_node_update(
    indexes: &mut PropertyIndexMap,
    old_labels: &LabelSet,
    old_props: &PropertyMap,
    new_labels: &LabelSet,
    new_props: &PropertyMap,
    row: u32,
) -> GraphResult<()> {
    // Iterate only registered indexes whose `(label, property)` is reachable
    // from this update — i.e., the label appears on either side and the
    // property exists on either side. Index-count grows independently of
    // a single node's column footprint, so this keeps update cost
    // proportional to the changed footprint, not the registry size.
    let candidates = candidate_keys(indexes, old_labels, old_props, new_labels, new_props);
    for (label, property) in candidates {
        let old_value = indexable_value(old_labels, old_props, &label, &property);
        let new_value = indexable_value(new_labels, new_props, &label, &property);
        if values_share_key(
            indexes,
            label.clone(),
            property.clone(),
            old_value,
            new_value,
        ) {
            continue;
        }
        if let Some(value) = old_value {
            remove_commit(indexes, label.clone(), property.clone(), value, row)?;
        }
        if let Some(value) = new_value {
            insert_commit(indexes, label.clone(), property.clone(), value, row)?;
        }
    }
    Ok(())
}

pub(crate) fn apply_edge_update(
    indexes: &mut PropertyIndexMap,
    label: &DbString,
    old_props: &PropertyMap,
    new_props: &PropertyMap,
    row: u32,
) -> GraphResult<()> {
    if indexes.is_empty() {
        return Ok(());
    }
    let mut properties: BTreeSet<DbString> = BTreeSet::new();
    properties.extend(old_props.keys().cloned());
    properties.extend(new_props.keys().cloned());
    for property in properties {
        if !indexes.contains_key(&(label.clone(), property.clone())) {
            continue;
        }
        let old_value = old_props.get(&property).filter(|value| !is_null(value));
        let new_value = new_props.get(&property).filter(|value| !is_null(value));
        if values_share_key(
            indexes,
            label.clone(),
            property.clone(),
            old_value,
            new_value,
        ) {
            continue;
        }
        if let Some(value) = old_value {
            remove_commit(indexes, label.clone(), property.clone(), value, row)?;
        }
        if let Some(value) = new_value {
            insert_commit(indexes, label.clone(), property.clone(), value, row)?;
        }
    }
    Ok(())
}

/// Return the set of registered `(label, property)` keys that this update
/// could possibly touch: the label must appear on at least one side of
/// the diff AND the property must appear on at least one side.
fn candidate_keys(
    indexes: &PropertyIndexMap,
    old_labels: &LabelSet,
    old_props: &PropertyMap,
    new_labels: &LabelSet,
    new_props: &PropertyMap,
) -> BTreeSet<(DbString, DbString)> {
    // Zero registered indexes is the common case for graphs that never declared
    // a property index — skip the three BTreeSet allocations entirely (mirrors
    // the composite-index sibling, which is already alloc-free on an empty map).
    if indexes.is_empty() {
        return BTreeSet::new();
    }
    let mut labels: BTreeSet<DbString> = BTreeSet::new();
    labels.extend(old_labels.iter().cloned());
    labels.extend(new_labels.iter().cloned());

    let mut properties: BTreeSet<DbString> = BTreeSet::new();
    properties.extend(old_props.keys().cloned());
    properties.extend(new_props.keys().cloned());

    let mut candidates = BTreeSet::new();
    for label in &labels {
        for property in &properties {
            let key = (label.clone(), property.clone());
            if indexes.contains_key(&key) {
                candidates.insert(key);
            }
        }
    }
    candidates
}

/// Build a property index strictly: any kind mismatch / NaN aborts with a
/// typed [`GraphError::IndexValueRejected`]. Used at registration time
/// (`SharedGraph::create_property_index`) so users learn immediately when
/// existing data violates the declared kind.
pub(crate) fn build_property_index(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: TypedIndexKind,
) -> GraphResult<TypedIndex> {
    build_property_index_inner(graph, label, property, kind, BuildPolicy::Strict)
        .map(|built| built.index)
}

/// Build a property index leniently: kind-mismatched / NaN values are
/// logged and skipped, the same policy used by the commit-path
/// (`insert_commit`). Used by [`rebuild_property_indexes`] so a snapshot
/// that was accepted at runtime (with documented kind drift in open
/// graphs) does not fail recovery — drift is recoverable, fictional
/// indexes are not.
pub(crate) fn build_property_index_lenient(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: TypedIndexKind,
) -> GraphResult<LenientIndexBuild> {
    build_property_index_inner(graph, label, property, kind, BuildPolicy::Lenient)
}

/// Build an edge property index strictly over existing edge columns.
pub(crate) fn build_edge_property_index(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: TypedIndexKind,
) -> GraphResult<TypedIndex> {
    build_edge_property_index_inner(graph, label, property, kind, BuildPolicy::Strict)
        .map(|built| built.index)
}

/// Build an edge property index leniently for recovery/rebuild paths.
pub(crate) fn build_edge_property_index_lenient(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: TypedIndexKind,
) -> GraphResult<LenientIndexBuild> {
    build_edge_property_index_inner(graph, label, property, kind, BuildPolicy::Lenient)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildPolicy {
    Strict,
    Lenient,
}

/// A built index plus the number of live rows it could not key.
///
/// Under [`BuildPolicy::Strict`] the first rejection returns `Err`, so an `Ok`
/// strict build always reports zero and the strict wrappers discard this.
pub(crate) struct LenientIndexBuild {
    /// The index over every row whose variant matched the registered kind.
    pub(crate) index: TypedIndex,
    /// Rows skipped for a kind mismatch. NaN skips are excluded; see
    /// [`crate::graph::index_entries::PropertyIndexEntry::drifted_rows`].
    pub(crate) drifted_rows: u64,
}

fn build_property_index_inner(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: TypedIndexKind,
    policy: BuildPolicy,
) -> GraphResult<LenientIndexBuild> {
    let mut index = TypedIndex::new(kind);
    let mut drifted_rows = 0_u64;
    for row_index in 0..graph.node_store.labels.len() {
        let row = u32::try_from(row_index).map_err(|_| GraphError::Inconsistent {
            reason: format!(
                "node store row index {row_index} exceeds u32::MAX; selene-graph \
                 caps rows at u32::MAX",
            ),
        })?;
        if !graph.node_store.is_alive(row) {
            continue;
        }
        let Some(labels) = graph.node_store.labels.get(row_index) else {
            continue;
        };
        if !labels.contains(&label) {
            continue;
        }
        let Some(props) = graph.node_store.properties.get(row_index) else {
            continue;
        };
        let Some(value) = props.get(&property) else {
            continue;
        };
        if is_null(value) {
            continue;
        }
        match index.insert(value, row) {
            Ok(()) => {}
            Err(err) => match policy {
                BuildPolicy::Strict => {
                    return Err(index_rejection(label.clone(), property.clone(), err));
                }
                BuildPolicy::Lenient => {
                    if counts_as_drift(&err) {
                        drifted_rows = drifted_rows.saturating_add(1);
                    }
                    warn_rejected("rebuild", label.clone(), property.clone(), row, &err);
                }
            },
        }
    }
    Ok(LenientIndexBuild {
        index,
        drifted_rows,
    })
}

fn build_edge_property_index_inner(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: TypedIndexKind,
    policy: BuildPolicy,
) -> GraphResult<LenientIndexBuild> {
    let mut index = TypedIndex::new(kind);
    let mut drifted_rows = 0_u64;
    for row_index in 0..graph.edge_store.label.len() {
        let row = u32::try_from(row_index).map_err(|_| GraphError::Inconsistent {
            reason: format!(
                "edge store row index {row_index} exceeds u32::MAX; selene-graph \
                 caps rows at u32::MAX",
            ),
        })?;
        if !graph.edge_store.is_alive(row) {
            continue;
        }
        let Some(edge_label) = graph.edge_store.label.get(row_index) else {
            continue;
        };
        if edge_label != &label {
            continue;
        }
        let Some(props) = graph.edge_store.properties.get(row_index) else {
            continue;
        };
        let Some(value) = props.get(&property) else {
            continue;
        };
        if is_null(value) {
            continue;
        }
        match index.insert(value, row) {
            Ok(()) => {}
            Err(err) => match policy {
                BuildPolicy::Strict => {
                    return Err(index_rejection(label.clone(), property.clone(), err));
                }
                BuildPolicy::Lenient => {
                    if counts_as_drift(&err) {
                        drifted_rows = drifted_rows.saturating_add(1);
                    }
                    warn_rejected("edge rebuild", label.clone(), property.clone(), row, &err);
                }
            },
        }
    }
    Ok(LenientIndexBuild {
        index,
        drifted_rows,
    })
}

/// Rebuild every registered property index from the graph's columns,
/// using lenient (log-and-skip) semantics on kind mismatches so recovery
/// of a runtime-accepted snapshot never fails. The strict policy lives
/// at registration only.
pub(crate) fn rebuild_property_indexes(graph: &mut crate::SeleneGraph) -> GraphResult<()> {
    let registrations: Vec<((DbString, DbString), TypedIndexKind, Option<DbString>)> = graph
        .property_index
        .iter()
        .map(|(key, entry)| (key.clone(), entry.kind(), entry.name.clone()))
        .collect();
    graph.property_index.clear();
    for ((label, property), kind, name) in registrations {
        let built = build_property_index_lenient(graph, label.clone(), property.clone(), kind)?;
        graph.property_index.insert(
            (label, property),
            PropertyIndexEntry::new_with_drift(built.index, name, built.drifted_rows),
        );
    }
    Ok(())
}

/// Rebuild every registered edge property index from the graph's edge columns.
pub(crate) fn rebuild_edge_property_indexes(graph: &mut crate::SeleneGraph) -> GraphResult<()> {
    let registrations: Vec<((DbString, DbString), TypedIndexKind, Option<DbString>)> = graph
        .edge_property_index
        .iter()
        .map(|(key, entry)| (key.clone(), entry.kind(), entry.name.clone()))
        .collect();
    graph.edge_property_index.clear();
    for ((label, property), kind, name) in registrations {
        let built =
            build_edge_property_index_lenient(graph, label.clone(), property.clone(), kind)?;
        graph.edge_property_index.insert(
            (label, property),
            PropertyIndexEntry::new_with_drift(built.index, name, built.drifted_rows),
        );
    }
    Ok(())
}

fn values_share_key(
    indexes: &PropertyIndexMap,
    label: DbString,
    property: DbString,
    old_value: Option<&Value>,
    new_value: Option<&Value>,
) -> bool {
    match (old_value, new_value) {
        (None, None) => true,
        (Some(old_value), Some(new_value)) => indexes
            .get(&(label.clone(), property.clone()))
            .is_some_and(|entry| entry.index.values_share_key(old_value, new_value)),
        _ => false,
    }
}

fn indexable_value<'a>(
    labels: &LabelSet,
    props: &'a PropertyMap,
    label: &DbString,
    property: &DbString,
) -> Option<&'a Value> {
    if !labels.contains(label) {
        return None;
    }
    props.get(property).filter(|value| !is_null(value))
}

fn insert_commit(
    indexes: &mut PropertyIndexMap,
    label: DbString,
    property: DbString,
    value: &Value,
    row: u32,
) -> GraphResult<()> {
    let Some(entry) = indexes.get_mut(&(label.clone(), property.clone())) else {
        return Ok(());
    };
    let Err(err) = std::sync::Arc::make_mut(&mut entry.index).insert(value, row) else {
        return Ok(());
    };
    if counts_as_drift(&err) {
        entry.drifted_rows = entry.drifted_rows.saturating_add(1);
    }
    demote_or_promote(label, property, row, "insert", err)
}

fn remove_commit(
    indexes: &mut PropertyIndexMap,
    label: DbString,
    property: DbString,
    value: &Value,
    row: u32,
) -> GraphResult<()> {
    let Some(entry) = indexes.get_mut(&(label.clone(), property.clone())) else {
        return Ok(());
    };
    let Err(err) = std::sync::Arc::make_mut(&mut entry.index).remove(value, row) else {
        return Ok(());
    };
    if counts_as_drift(&err) {
        entry.drifted_rows = entry.drifted_rows.saturating_sub(1);
    }
    demote_or_promote(label, property, row, "remove", err)
}

/// Whether a skipped value leaves the index an incomplete view of its column.
///
/// A kind mismatch does: the row is live and a scan comparing across variants
/// may match it, so the index must stop answering until the row leaves.
///
/// NaN does not. NaN satisfies no equality or range predicate, so a scan omits
/// the row too and index and scan already agree. Counting it would disable an
/// index that needs no disabling.
///
/// `insert_commit` and `remove_commit` must apply this identically. An
/// asymmetric filter drives the tally toward zero and re-promotes an index that
/// is still missing rows.
///
/// Without universal comparison, a cross-family value may raise `22G04` even
/// when it cannot match a probe. Omitting that row changes both equality and
/// range behavior. The planner therefore declines any kind-drifted index and
/// lets ordinary predicate evaluation preserve the failure. Duration unit
/// groups share one physical index kind; GQL additionally validates their
/// actual key domain before consuming an indexed predicate.
const fn counts_as_drift(err: &TypedIndexValueError) -> bool {
    matches!(err, TypedIndexValueError::KindMismatch { .. })
}

/// Commit-path branching for [`TypedIndexValueError`]. Open-graph kind
/// drift (`KindMismatch`, `NaN`) keeps the `warn_rejected` lenient skip
/// because it remains recoverable per the module rustdoc.
fn demote_or_promote(
    label: DbString,
    property: DbString,
    row: u32,
    op: &'static str,
    err: TypedIndexValueError,
) -> GraphResult<()> {
    match err {
        TypedIndexValueError::KindMismatch { .. } | TypedIndexValueError::NaN { .. } => {
            warn_rejected(op, label, property, row, &err);
            Ok(())
        }
    }
}

fn index_rejection(label: DbString, property: DbString, err: TypedIndexValueError) -> GraphError {
    match err {
        TypedIndexValueError::KindMismatch { .. } | TypedIndexValueError::NaN { .. } => {
            GraphError::IndexValueRejected {
                label,
                property,
                expected_kind: err.expected_kind(),
                observed: err.observed(),
            }
        }
    }
}

fn warn_rejected(
    op: &'static str,
    label: DbString,
    property: DbString,
    row: u32,
    err: &TypedIndexValueError,
) {
    tracing::warn!(
        op,
        %label,
        %property,
        row,
        expected_kind = ?err.expected_kind(),
        observed = err.observed(),
        "skipped property-index update for value that does not match the registered index kind",
    );
}

const fn is_null(value: &Value) -> bool {
    matches!(value, Value::Null)
}

#[cfg(test)]
#[path = "property_index_tests.rs"]
mod tests;
