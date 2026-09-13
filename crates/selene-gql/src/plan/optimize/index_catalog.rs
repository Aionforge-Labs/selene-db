//! Query-time index discovery surface.

use std::ops::RangeBounds;

use selene_core::{DbString, Value};

/// Graph element kind targeted by an index lookup.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum IndexTarget {
    /// Node index lookup.
    Node,
    /// Edge index lookup.
    Edge,
}

/// Embedder-injected catalog for optimizer index discovery.
///
/// Handles are valid only for the catalog snapshot used to optimize a plan.
/// Embedders that cache plans across snapshot rotations must either re-plan or
/// validate handles against the new snapshot before execution.
pub trait IndexCatalog: Send + Sync {
    /// Discover an equivalent complete node expression index and exact probe
    /// cardinality. Unknown semantic/profile or data completeness must decline.
    fn expression_index(
        &self,
        label: &DbString,
        expression: &selene_core::scalar_index_expression::ScalarIndexExpression,
        value: &Value,
    ) -> Option<(TypedIndexLookup, u64)> {
        let _ = (label, expression, value);
        None
    }

    /// Return a typed-property index for `(target, label, property)`, if any.
    fn typed_index(
        &self,
        target: IndexTarget,
        label: DbString,
        property: DbString,
    ) -> Option<TypedIndexLookup>;

    /// Return a label-only index for `(target, label)`, if any.
    fn label_index(&self, target: IndexTarget, label: DbString) -> Option<IndexHandle>;

    /// Return a composite-property index for `(target, label, properties)`, if any.
    fn composite_index(
        &self,
        target: IndexTarget,
        label: DbString,
        properties: &[DbString],
    ) -> Option<CompositeIndexHandle>;

    // ---------------------------------------------------------------------
    // Cost statistics (OPT-5)
    //
    // These methods expose live row-count statistics read from the *same*
    // pinned snapshot the optimizer plans against (pinned-snapshot invariant).
    // They return *estimated output row counts* so the cost model can gate
    // index-vs-Linear and rank competing index candidates. The default `None`
    // bodies preserve the pre-OPT-5 structural behavior: when no statistic is
    // available (e.g. [`EmptyIndexCatalog`], or a catalog with no stats backing)
    // every estimator declines and the rules keep their exact structural
    // decision + verbatim heuristic fallback. Because every index access path is
    // row-equivalent to a Linear scan (the runtime always re-applies the residual
    // label / property filters), cost can only flip *which correct path runs* —
    // never which rows are produced.
    // ---------------------------------------------------------------------

    /// Total live row count for `target` — the Linear-scan baseline.
    ///
    /// Defaults to `None` (no statistics).
    fn total_rows(&self, target: IndexTarget) -> Option<u64> {
        let _ = target;
        None
    }

    /// Exact number of rows carrying `label` for `target` (RoaringBitmap
    /// popcount). Defaults to `None`.
    fn label_cardinality(&self, target: IndexTarget, label: DbString) -> Option<u64> {
        let _ = (target, label);
        None
    }

    /// Exact number of rows matching `value` under the `(target, label,
    /// property)` typed index (literal equality). Defaults to `None`.
    fn equality_cardinality(
        &self,
        target: IndexTarget,
        label: DbString,
        property: DbString,
        value: &Value,
    ) -> Option<u64> {
        let _ = (target, label, property, value);
        None
    }

    /// Exact number of rows matching `range` under the `(target, label,
    /// property)` typed index (literal bounds). Defaults to `None`.
    fn range_cardinality(
        &self,
        target: IndexTarget,
        label: DbString,
        property: DbString,
        range: (std::ops::Bound<Value>, std::ops::Bound<Value>),
    ) -> Option<u64> {
        let _ = (target, label, property, range);
        None
    }

    /// Expected rows per distinct key for the `(target, label, property)` typed
    /// index = `cardinality / distinct_keys`. Used for parameter equality where
    /// the value is unknown at plan time. Defaults to `None`.
    fn typed_avg_bucket(
        &self,
        target: IndexTarget,
        label: DbString,
        property: DbString,
    ) -> Option<u64> {
        let _ = (target, label, property);
        None
    }

    /// Exact number of rows matching the literal composite `keys` (in declared
    /// order) under the `(target, label, properties)` composite index. Defaults
    /// to `None`. A `None` element in `keys` marks a parameter slot that makes
    /// the literal probe impossible; the caller then falls back to
    /// [`IndexCatalog::composite_avg_bucket`].
    fn composite_cardinality(
        &self,
        target: IndexTarget,
        label: DbString,
        properties: &[DbString],
        keys: &[Value],
    ) -> Option<u64> {
        let _ = (target, label, properties, keys);
        None
    }

    /// Expected rows per distinct composite key = `cardinality / distinct_keys`
    /// for the `(target, label, properties)` composite index. Used for
    /// parameter-keyed composite probes. Defaults to `None`.
    fn composite_avg_bucket(
        &self,
        target: IndexTarget,
        label: DbString,
        properties: &[DbString],
    ) -> Option<u64> {
        let _ = (target, label, properties);
        None
    }
}

/// A half-open or closed `Value` range built from optimizer-side bounds.
///
/// `(Bound<Value>, Bound<Value>)` already implements [`RangeBounds<Value>`], so
/// this type alias documents intent without a newtype wrapper.
pub(crate) type ValueRange = (std::ops::Bound<Value>, std::ops::Bound<Value>);

/// Assert at compile time that the range tuple satisfies `RangeBounds<Value>`
/// (the shape `SeleneGraph::node_property_range_cardinality` consumes).
const _: fn() = || {
    fn _assert<R: RangeBounds<Value>>() {}
    _assert::<ValueRange>();
};

/// Opaque index handle.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct IndexHandle(u64);

impl IndexHandle {
    /// Construct an opaque handle from a caller-owned raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the caller-owned raw value.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Typed-index lookup metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TypedIndexLookup {
    /// Opaque catalog handle.
    pub handle: IndexHandle,
    /// Indexed value kind.
    pub kind: IndexKind,
}

impl TypedIndexLookup {
    /// Construct typed-index lookup metadata.
    #[must_use]
    pub const fn new(handle: IndexHandle, kind: IndexKind) -> Self {
        Self { handle, kind }
    }
}

/// Composite-index lookup metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CompositeIndexHandle {
    /// Opaque catalog handle.
    pub handle: IndexHandle,
    /// Indexed properties in declaration order, paired with their typed-index
    /// kinds. The kinds enable per-component plan-time compatibility checks
    /// when admitting parameter slots into composite-index probes
    /// (BRIEF-154 §B.2 F7/F17 folds).
    pub properties: Vec<(DbString, IndexKind)>,
}

impl CompositeIndexHandle {
    /// Construct composite-index lookup metadata.
    #[must_use]
    pub fn new(handle: IndexHandle, properties: Vec<(DbString, IndexKind)>) -> Self {
        Self { handle, properties }
    }

    /// Return the property keys in declaration order, dropping the kind column.
    #[must_use]
    pub fn property_keys(&self) -> Vec<DbString> {
        self.properties.iter().map(|(key, _)| key.clone()).collect()
    }
}

/// Indexable value kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum IndexKind {
    /// Boolean typed index.
    Boolean,
    /// Signed integer typed index.
    Integer,
    /// Unsigned integer typed index.
    UnsignedInteger,
    /// Signed 128-bit integer typed index.
    Integer128,
    /// Unsigned 128-bit integer typed index.
    UnsignedInteger128,
    /// Decimal typed index.
    Decimal,
    /// 32-bit floating-point typed index.
    Float32,
    /// 64-bit floating-point typed index.
    Float,
    /// String typed index.
    String,
    /// Date typed index.
    Date,
    /// Local datetime typed index.
    LocalDateTime,
    /// Zoned datetime typed index.
    ZonedDateTime,
    /// Local time typed index.
    LocalTime,
    /// Zoned time typed index.
    ZonedTime,
    /// Duration typed index.
    Duration,
    /// UUID typed index.
    Uuid,
}

/// Empty catalog with no registered indexes.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyIndexCatalog;

impl IndexCatalog for EmptyIndexCatalog {
    fn typed_index(
        &self,
        _target: IndexTarget,
        _label: DbString,
        _property: DbString,
    ) -> Option<TypedIndexLookup> {
        None
    }

    fn label_index(&self, _target: IndexTarget, _label: DbString) -> Option<IndexHandle> {
        None
    }

    fn composite_index(
        &self,
        _target: IndexTarget,
        _label: DbString,
        _properties: &[DbString],
    ) -> Option<CompositeIndexHandle> {
        None
    }
}
