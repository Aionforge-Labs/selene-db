//! Index-or-scan row filters for procedure property filters.
//!
//! Procedure filters (`selene.text_search_nodes`, `selene.vector_search_nodes_ann`,
//! `algo.pagerank`) admit candidates through an indexed property. They require
//! the property to be indexed and report a bad argument when it is not, or when
//! the supplied values are of a kind the registered index cannot key.
//!
//! A registered index still declines to answer while it omits live rows whose
//! variant it cannot key, because an incomplete index would silently drop
//! candidates. These helpers scan in that case, using the same cross-variant
//! equality the executor's own fallback uses, so a filtered procedure returns
//! what a complete index would have returned instead of failing or
//! under-reporting.
//!
//! Two properties are deliberate. Argument validity is decided against the
//! registered kind *before* completeness is consulted, so the same call is an
//! error or a success regardless of whether some unrelated row has drifted.
//! And the scan walks the label's own row bitmap rather than every live row, so
//! its cost tracks the label rather than the graph.

use selene_core::{DbString, Value};
use selene_graph::{CandidateSet, Edge, IndexedEntity, Node, PropertyIndexReadInfo, SeleneGraph};

use super::value_compare;

/// Node candidates of `label` whose `property` equals any of `values`.
///
/// `None` means no index is registered for `(label, property)`, or a supplied
/// value is of a kind that index could never key, or its catalog binding is
/// ineligible — the unavailable/invalid-index argument errors the
/// procedures report. It never means "the index is incomplete"; that case
/// scans.
pub(crate) fn node_candidates_with_property_any(
    snapshot: &SeleneGraph,
    label: &DbString,
    property: &DbString,
    values: &[Value],
) -> Result<Option<CandidateSet<Node>>, selene_graph::GraphError> {
    let entry = usable_entry(
        snapshot.property_index_read_info(IndexedEntity::Node, label, property),
        values,
    );
    let Some(entry) = entry else {
        return Ok(None);
    };
    if entry.is_complete() {
        return snapshot.node_candidates_with_property_any(label, property, values);
    }
    let labelled = snapshot.node_candidates_with_label(label)?;
    let mut matching = Vec::new();
    for id in labelled.iter() {
        let Some(properties) = snapshot.node_properties(id) else {
            continue;
        };
        if matches_any(properties.get(property), values) {
            matching.push(id);
        }
    }
    snapshot.bind_node_candidates(matching).map(Some)
}

/// Edge candidates of `label` whose `property` equals any of `values`.
///
/// Mirrors [`node_candidates_with_property_any`] over the edge store.
pub(crate) fn edge_candidates_with_property_any(
    snapshot: &SeleneGraph,
    label: &DbString,
    property: &DbString,
    values: &[Value],
) -> Result<Option<CandidateSet<Edge>>, selene_graph::GraphError> {
    let entry = usable_entry(
        snapshot.property_index_read_info(IndexedEntity::Edge, label, property),
        values,
    );
    let Some(entry) = entry else {
        return Ok(None);
    };
    if entry.is_complete() {
        return snapshot.edge_candidates_with_property_any(label, property, values);
    }
    let labelled = snapshot.edge_candidates_with_label(label)?;
    let mut matching = Vec::new();
    for id in labelled.iter() {
        let Some(properties) = snapshot.edge_properties(id) else {
            continue;
        };
        if matches_any(properties.get(property), values) {
            matching.push(id);
        }
    }
    snapshot.bind_edge_candidates(matching).map(Some)
}

/// The registered entry, if one exists and can key every supplied value.
fn usable_entry<'a>(
    entry: Option<PropertyIndexReadInfo<'a>>,
    values: &[Value],
) -> Option<PropertyIndexReadInfo<'a>> {
    let entry = entry?;
    values
        .iter()
        .all(|value| entry.admits(value))
        .then_some(entry)
}

/// Whether a stored property value equals any supplied filter value.
///
/// NULL never matches on either side, matching the index path, where NULLs are
/// excluded before a key is ever built.
fn matches_any(stored: Option<&Value>, values: &[Value]) -> bool {
    let Some(stored) = stored else {
        return false;
    };
    if matches!(stored, Value::Null) {
        return false;
    }
    values
        .iter()
        .filter(|candidate| !matches!(candidate, Value::Null))
        .any(|candidate| value_compare::equal_non_null(stored, candidate))
}
