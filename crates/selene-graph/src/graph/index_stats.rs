//! Diagnosable per-index statistics for the property-index families (#1102).
//!
//! # Why this exists
//!
//! A typed property index cannot key a value whose variant does not match its
//! registered kind. Open graphs accept a value of any variant, so one ordinary
//! write can leave a live row the index cannot represent. Since #1099 such an
//! index *declines every probe* and callers fall back to a scan — correct, in
//! that queries keep returning the same rows either way, but silent: the index
//! is still registered, still listed by `SHOW INDEXES`, and answering nothing
//! from its own bitmaps.
//!
//! `drifted_rows` recorded that on the entry, but nothing projected it out of
//! `selene-graph`. `iter_property_index_entries` yields label/property/kind/name
//! only, and `selene.verify`'s coverage check deliberately probes through
//! `lookup_eq_ignoring_drift` — right for a corruption audit, but it means
//! verify reports a clean bill of health for a demoted index. The one remaining
//! trace was a `tracing::warn!` at write time, which an embedder with no
//! subscriber never sees.
//!
//! So an operator whose query got dramatically slower after a routine write had
//! no engine-visible way to find the cause. This module is the projection that
//! makes it findable; `selene.property_index_stats` is its query surface.
//!
//! # Why all three families
//!
//! Node single-property, edge single-property, and composite indexes each carry
//! their own `drifted_rows`. Surfacing one and not the others would recreate
//! exactly the blind spot being closed, so this walks all three and tags each
//! row with the entity it indexes.

use selene_core::DbString;

use crate::typed_index::TypedIndexKind;

use super::SeleneGraph;

/// Which entity family a property index covers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IndexedEntity {
    /// A node property index.
    Node,
    /// An edge property index.
    Edge,
}

impl IndexedEntity {
    /// Stable rendering used by the procedure surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "NODE",
            Self::Edge => "EDGE",
        }
    }
}

/// One property index's diagnosable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyIndexStatsRow {
    /// Whether this index covers nodes or edges.
    pub entity: IndexedEntity,
    /// Indexed label.
    pub label: DbString,
    /// Indexed properties, in declaration order. Length 1 unless composite.
    pub properties: Vec<DbString>,
    /// Registered typed-index kind per property, positionally aligned with
    /// [`Self::properties`].
    pub kinds: Vec<TypedIndexKind>,
    /// Whether this is a composite (multi-property) registration.
    ///
    /// Carried rather than inferred from `properties.len()`: the catalog names
    /// the two families differently, so a renderer that guessed would spell a
    /// one-property composite under the single-property scheme and stop
    /// agreeing with `SHOW INDEXES`.
    pub composite: bool,
    /// Explicit catalog name, or `None` when the name is derived at render time.
    pub name: Option<DbString>,
    /// Rows the index can answer from.
    pub indexed_rows: u64,
    /// Live rows the index cannot key, and therefore omits.
    ///
    /// NaN skips are excluded: NaN satisfies no equality or range predicate, so
    /// a scan omits a NaN row too and the index stays complete without it.
    pub drifted_rows: u64,
}

impl PropertyIndexStatsRow {
    /// Whether this index still answers probes from its own bitmaps.
    ///
    /// `false` is the actionable signal: the registration is intact and the
    /// index is listed as usual, but every probe declines and queries against
    /// it are running as full scans until the offending rows are corrected or
    /// the index is rebuilt at a kind that admits them.
    ///
    /// This is derived rather than stored so a caller does not have to know
    /// that the rule is "any drift at all", which is a stricter threshold than
    /// operators tend to assume.
    #[must_use]
    pub const fn answers_probes(&self) -> bool {
        self.drifted_rows == 0
    }
}

impl SeleneGraph {
    /// Iterate every property index with the state needed to diagnose it.
    ///
    /// Covers all three families that carry drift — node single-property, edge
    /// single-property, and composite — in that order. Vector and text indexes
    /// have their own stats surfaces and are not included.
    /// These are physical diagnostics: zero drift does not imply that a catalog
    /// binding permits query use. Use checked query accessors for that decision.
    pub fn iter_property_index_stats(&self) -> impl Iterator<Item = PropertyIndexStatsRow> + '_ {
        let single = |entity: IndexedEntity,
                      map: &'_ rustc_hash::FxHashMap<
            (DbString, DbString),
            super::index_entries::PropertyIndexEntry,
        >| {
            map.iter()
                .map(|((label, property), entry)| PropertyIndexStatsRow {
                    entity,
                    label: label.clone(),
                    properties: vec![property.clone()],
                    kinds: vec![entry.kind()],
                    composite: false,
                    name: entry.name.clone(),
                    indexed_rows: entry.index.cardinality(),
                    drifted_rows: entry.drifted_rows,
                })
                .collect::<Vec<_>>()
        };

        let nodes = single(IndexedEntity::Node, &self.property_index);
        let edges = single(IndexedEntity::Edge, &self.edge_property_index);
        let composites = self
            .composite_property_index
            .iter()
            .map(|((label, _), entry)| PropertyIndexStatsRow {
                // Composite registrations are node-only today; tagging the row
                // rather than assuming it keeps the surface stable if edge
                // composites land later.
                entity: IndexedEntity::Node,
                label: label.clone(),
                properties: entry.declared_properties.to_vec(),
                kinds: entry.kinds().to_vec(),
                composite: true,
                name: entry.name.clone(),
                indexed_rows: entry.index.cardinality(),
                drifted_rows: entry.drifted_rows,
            })
            .collect::<Vec<_>>();

        nodes.into_iter().chain(edges).chain(composites)
    }
}

#[cfg(test)]
#[path = "index_stats/tests.rs"]
mod tests;
