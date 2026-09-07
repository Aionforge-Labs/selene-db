//! Cardinality helpers for query planning and cost estimation.

use std::ops::RangeBounds;

use selene_core::{DbString, Value};

use super::SeleneGraph;

impl SeleneGraph {
    /// Return the number of live nodes carrying `label`.
    #[must_use]
    pub fn node_label_cardinality(&self, label: &DbString) -> u64 {
        self.idx_label.get(label).map_or(0, |bm| bm.len())
    }

    /// Return the number of live edges carrying `label`.
    #[must_use]
    pub fn edge_label_cardinality(&self, label: &DbString) -> u64 {
        self.idx_edge_label.get(label).map_or(0, |bm| bm.len())
    }

    /// Return the cardinality of live nodes matching `value` under a registered index.
    #[must_use]
    pub fn node_property_eq_cardinality(
        &self,
        label: &DbString,
        property: &DbString,
        value: &Value,
    ) -> Option<u64> {
        self.nodes_with_property_eq(label, property, value)
            .map(|cow| cow.len())
    }

    /// Return the cardinality of live edges matching `value` under a registered index.
    #[must_use]
    pub fn edge_property_eq_cardinality(
        &self,
        label: &DbString,
        property: &DbString,
        value: &Value,
    ) -> Option<u64> {
        self.edges_with_property_eq(label, property, value)
            .map(|cow| cow.len())
    }

    /// Return the cardinality of live nodes matching `range` under a registered index.
    #[must_use]
    pub fn node_property_range_cardinality<R>(
        &self,
        label: &DbString,
        property: &DbString,
        range: R,
    ) -> Option<u64>
    where
        R: RangeBounds<Value>,
    {
        self.nodes_with_property_range(label, property, range)
            .map(|bm| bm.len())
    }

    /// Return the cardinality of live edges matching `range` under a registered index.
    #[must_use]
    pub fn edge_property_range_cardinality<R>(
        &self,
        label: &DbString,
        property: &DbString,
        range: R,
    ) -> Option<u64>
    where
        R: RangeBounds<Value>,
    {
        self.edges_with_property_range(label, property, range)
            .map(|bm| bm.len())
    }
}
