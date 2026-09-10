//! SeleneGraph-backed [`IndexCatalog`] for production optimizer index discovery.
//!
//! [`LiveIndexCatalog`] adapts a *pinned* immutable graph snapshot
//! (`Arc<SeleneGraph>`) to the optimizer's [`IndexCatalog`] trait so the default
//! structural rules can select label / typed / composite index access paths
//! against the live graph. It is the production counterpart to the test-only
//! `MockIndexCatalog`.
//!
//! # Pinned-snapshot invariant (load-bearing)
//!
//! The catalog captures exactly one `Arc<SeleneGraph>` at construction and
//! probes *that* snapshot for every method call — it never re-reads the
//! `ArcSwap`. The optimizer MUST plan against the same snapshot the executor
//! runs against; re-reading per probe could let `optimize()` observe a
//! different index set per call under concurrent index DDL, producing a plan
//! whose access paths don't match the executed snapshot. Embedders build the
//! catalog from the snapshot they pin for execution (see
//! `Session::execute_source`).
//!
//! Because the snapshot is an immutable `ArcSwap` load (D10 lock-free read),
//! constructing and probing the catalog takes no write lock and preserves the
//! single-writer / lock-free-reader contract.
//!
//! # Always-available label index
//!
//! [`label_index`](LiveIndexCatalog::label_index) returns `Some` for any
//! `(Node | Edge, label)` because SeleneGraph maintains the intrinsic
//! RoaringBitmap label index on every write — a label scan is always a valid
//! access path. The runtime arm (`runtime::scan::label_index_rows`) falls back
//! to a linear scan when the requested label's bitmap is absent (zero matching
//! rows), so reporting `Some` here is always safe.
//!
//! # Cost statistics (OPT-5)
//!
//! The catalog also serves the optimizer's cost model: `total_rows`,
//! `label_cardinality`, `equality_cardinality`, `range_cardinality`,
//! `typed_avg_bucket`, `composite_cardinality`, and `composite_avg_bucket` all
//! read live row counts from the *same* pinned snapshot. They return estimated
//! output row counts (lower = cheaper) so the index rules can gate
//! index-vs-Linear and rank competing index candidates without re-reading the
//! `ArcSwap` or taking a write lock. Because every index access path is
//! row-equivalent to a Linear scan (the runtime always re-applies the residual
//! filters), these statistics can only change *which correct path runs*.

use std::sync::Arc;

use selene_core::{DbString, Value};
use selene_graph::{SeleneGraph, TypedIndexKind};

use crate::plan::optimize::{
    CompositeIndexHandle, IndexCatalog, IndexHandle, IndexKind, IndexTarget, TypedIndexLookup,
};

/// Index catalog backed by a pinned [`SeleneGraph`] snapshot.
///
/// See the module docs for the pinned-snapshot invariant and the
/// always-available label-index contract.
#[derive(Clone, Debug)]
pub struct LiveIndexCatalog {
    snapshot: Arc<SeleneGraph>,
}

impl LiveIndexCatalog {
    /// Construct a catalog over a pinned graph snapshot.
    ///
    /// The supplied `Arc<SeleneGraph>` must be the same snapshot the plan will
    /// execute against (see the module-level pinned-snapshot invariant).
    #[must_use]
    pub const fn new(snapshot: Arc<SeleneGraph>) -> Self {
        Self { snapshot }
    }
}

impl IndexCatalog for LiveIndexCatalog {
    fn typed_index(
        &self,
        target: IndexTarget,
        label: DbString,
        property: DbString,
    ) -> Option<TypedIndexLookup> {
        let kind = match target {
            IndexTarget::Node => self.snapshot.property_index_for(&label, &property)?.kind(),
            IndexTarget::Edge => self
                .snapshot
                .edge_property_index_for(&label, &property)?
                .kind(),
        };
        // The opaque handle is never dereferenced by the runtime; the scan
        // re-derives the actual index by (label, property) at execute time.
        Some(TypedIndexLookup::new(
            IndexHandle::new(0),
            index_kind_from(kind),
        ))
    }

    fn label_index(&self, target: IndexTarget, _label: DbString) -> Option<IndexHandle> {
        // The intrinsic RoaringBitmap label index is always available for both
        // node and edge targets; the runtime falls back to linear when a label
        // bitmap is absent. The handle is opaque (runtime re-derives by label).
        match target {
            IndexTarget::Node | IndexTarget::Edge => Some(IndexHandle::new(0)),
        }
    }

    fn composite_index(
        &self,
        target: IndexTarget,
        label: DbString,
        properties: &[DbString],
    ) -> Option<CompositeIndexHandle> {
        // Composite indexes are node-only at HEAD.
        if target != IndexTarget::Node {
            return None;
        }
        let mut canonical = properties.to_vec();
        canonical.sort_unstable();
        let entry = self
            .snapshot
            .composite_property_index_entry_for(&label, &canonical)?;
        if !entry.is_complete() {
            return None;
        }
        let kinds = entry.kinds();
        // Per-component IndexKind in declaration order enables parameter-aware
        // composite probes (BRIEF-154 §B.2). The runtime re-derives the actual
        // index by (label, sorted-properties) at execute time.
        let component_kinds: Vec<(DbString, IndexKind)> = entry
            .declared_properties
            .iter()
            .zip(kinds.iter())
            .map(|(property, kind)| (property.clone(), index_kind_from(*kind)))
            .collect();
        Some(CompositeIndexHandle::new(
            IndexHandle::new(0),
            component_kinds,
        ))
    }

    // ---- Cost statistics (OPT-5) ----
    //
    // All counts are read from `self.snapshot` (the pinned snapshot), never a
    // re-loaded `ArcSwap` — preserving D10 and the pinned-snapshot invariant.
    // Each probe is O(1)/O(log n): RoaringBitmap::len is a popcount, the typed/
    // composite cardinality + distinct_keys are pre-summed/length reads, and the
    // equality/range probes borrow (not materialize) the index bitmap.

    fn total_rows(&self, target: IndexTarget) -> Option<u64> {
        Some(match target {
            IndexTarget::Node => self.snapshot.node_count() as u64,
            IndexTarget::Edge => self.snapshot.edge_count() as u64,
        })
    }

    fn label_cardinality(&self, target: IndexTarget, label: DbString) -> Option<u64> {
        match target {
            // Absent bitmap = zero rows carry the label; report 0 (an exact,
            // maximally-selective count) rather than None so the cost gate can
            // still prefer the index.
            IndexTarget::Node => Some(self.snapshot.node_label_cardinality(&label)),
            IndexTarget::Edge => Some(self.snapshot.edge_label_cardinality(&label)),
        }
    }

    fn equality_cardinality(
        &self,
        target: IndexTarget,
        label: DbString,
        property: DbString,
        value: &Value,
    ) -> Option<u64> {
        match target {
            IndexTarget::Node => self
                .snapshot
                .node_property_eq_cardinality(&label, &property, value),
            IndexTarget::Edge => self
                .snapshot
                .edge_property_eq_cardinality(&label, &property, value),
        }
    }

    fn range_cardinality(
        &self,
        target: IndexTarget,
        label: DbString,
        property: DbString,
        range: (std::ops::Bound<Value>, std::ops::Bound<Value>),
    ) -> Option<u64> {
        match target {
            IndexTarget::Node => self
                .snapshot
                .node_property_range_cardinality(&label, &property, range),
            IndexTarget::Edge => self
                .snapshot
                .edge_property_range_cardinality(&label, &property, range),
        }
    }

    fn typed_avg_bucket(
        &self,
        target: IndexTarget,
        label: DbString,
        property: DbString,
    ) -> Option<u64> {
        let index = match target {
            IndexTarget::Node => self.snapshot.property_index_for(&label, &property)?,
            IndexTarget::Edge => self.snapshot.edge_property_index_for(&label, &property)?,
        };
        Some(avg_bucket(index.cardinality(), index.distinct_keys()))
    }

    fn composite_cardinality(
        &self,
        target: IndexTarget,
        label: DbString,
        properties: &[DbString],
        keys: &[Value],
    ) -> Option<u64> {
        if target != IndexTarget::Node {
            return None;
        }
        let mut canonical = properties.to_vec();
        canonical.sort_unstable();
        let index = self
            .snapshot
            .composite_property_index_for(&label, &canonical)?;
        // The runtime composite index orders components by canonical (sorted)
        // property order; reorder `keys` (given in `properties` order) to match.
        let mut order: Vec<usize> = (0..properties.len()).collect();
        order.sort_by_key(|&i| properties[i].clone());
        if keys.len() != properties.len() {
            return None;
        }
        let ordered: Vec<&Value> = order.iter().map(|&i| &keys[i]).collect();
        // Single-coercion key build. A successful coercion yields the exact
        // bucket count; an arity / kind mismatch (`Err`) declines so the caller
        // keeps the structural decision.
        match index.key_from_values(&ordered) {
            Ok(key) => Some(index.lookup_key(&key).map_or(0, |bm| bm.len())),
            Err(_) => None,
        }
    }

    fn composite_avg_bucket(
        &self,
        target: IndexTarget,
        label: DbString,
        properties: &[DbString],
    ) -> Option<u64> {
        if target != IndexTarget::Node {
            return None;
        }
        let mut canonical = properties.to_vec();
        canonical.sort_unstable();
        let index = self
            .snapshot
            .composite_property_index_for(&label, &canonical)?;
        Some(avg_bucket(index.cardinality(), index.distinct_keys()))
    }
}

/// Expected rows per distinct key = `ceil(cardinality / distinct_keys)`, used
/// as the parameter-equality cost (the value is unknown at plan time). Rounds
/// up so a near-unique index never estimates 0 rows. Empty index → 0.
fn avg_bucket(cardinality: u64, distinct_keys: u64) -> u64 {
    if distinct_keys == 0 {
        return 0;
    }
    cardinality.div_ceil(distinct_keys)
}

/// Map a storage-level [`TypedIndexKind`] to the optimizer's [`IndexKind`].
fn index_kind_from(kind: TypedIndexKind) -> IndexKind {
    match kind {
        TypedIndexKind::Bool => IndexKind::Boolean,
        TypedIndexKind::I64 => IndexKind::Integer,
        TypedIndexKind::U64 => IndexKind::UnsignedInteger,
        TypedIndexKind::I128 => IndexKind::Integer128,
        TypedIndexKind::U128 => IndexKind::UnsignedInteger128,
        TypedIndexKind::Decimal => IndexKind::Decimal,
        TypedIndexKind::F32 => IndexKind::Float32,
        TypedIndexKind::F64 => IndexKind::Float,
        TypedIndexKind::String => IndexKind::String,
        TypedIndexKind::Date => IndexKind::Date,
        TypedIndexKind::LocalDateTime => IndexKind::LocalDateTime,
        TypedIndexKind::ZonedDateTime => IndexKind::ZonedDateTime,
        TypedIndexKind::LocalTime => IndexKind::LocalTime,
        TypedIndexKind::ZonedTime => IndexKind::ZonedTime,
        TypedIndexKind::Duration => IndexKind::Duration,
        TypedIndexKind::Uuid => IndexKind::Uuid,
    }
}
