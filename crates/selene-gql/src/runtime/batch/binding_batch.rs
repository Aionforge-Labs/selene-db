//! Typed column-major binding batches with explicit nulls and selections.
//!
//! A [`BindingBatch`] carries one [`BindingTableSchema`] (declared column
//! types and order, shared with the row executor so result descriptors match
//! by construction), column-major values, an explicit null bitmap per column,
//! an optional internal selection vector, and an explicit logical row count.
//!
//! Positions ([`BatchPosition`]) are crate-private `u32` offsets into the
//! physical columns. They are never graph storage rows, never stable node or
//! edge identities, and never cross the crate boundary: materialization emits
//! only [`Value`]s. The unit table is preserved explicitly — [`BindingBatch::unit`]
//! (zero columns, one logical row) is distinct from [`BindingBatch::empty`]
//! (zero logical rows) per ISO/IEC 39075:2024 §4.3.6 as recorded in the GQL
//! and durability notes.
//!
//! All constructors validate their invariants and return [`BatchError`] on
//! violation; hot loops may therefore rely on length/index alignment via
//! `debug_assert!` without re-checking.

use std::mem::size_of;

use selene_core::Value;

use crate::plan::BindingTableSchema;

/// Internal position of one logical row inside a batch's physical columns.
///
/// This is deliberately a crate-private offset: there is intentionally no
/// conversion into `NodeId`, `EdgeId`, or any storage-row type, so batch
/// coordinates cannot leak into graph identities. See the API-surface
/// regression test.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct BatchPosition(u32);

impl BatchPosition {
    /// Return the physical column index. Only batch internals call this.
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// One column: values plus an explicit null bitmap.
///
/// `nulls[i]` is true exactly when `values[i]` is [`Value::Null`]. The bitmap
/// is redundant with the values by design: sparse filtering and buffer reuse
/// must keep both aligned, and the regression tests assert that alignment.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BatchColumn {
    values: Vec<Value>,
    nulls: Vec<bool>,
}

impl BatchColumn {
    /// Build a column, deriving the null bitmap from [`Value::Null`] entries.
    #[must_use]
    pub(crate) fn from_values(values: Vec<Value>) -> Self {
        let nulls = values.iter().map(|value| *value == Value::Null).collect();
        Self { values, nulls }
    }

    /// Build a column from caller-owned storage, validating alignment once.
    ///
    /// Operators use this with buffer-recycled vectors so steady state
    /// allocates nothing per batch. Hot loops rely on the validated lengths.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::ColumnLengthMismatch`] when the bitmap does not
    /// cover exactly the values.
    pub(crate) fn from_parts(values: Vec<Value>, nulls: Vec<bool>) -> Result<Self, BatchError> {
        if values.len() != nulls.len() {
            return Err(BatchError::ColumnLengthMismatch);
        }
        Ok(Self { values, nulls })
    }

    /// Borrow values in physical order.
    #[must_use]
    pub(crate) fn values(&self) -> &[Value] {
        &self.values
    }

    /// Borrow the null bitmap in physical order.
    #[must_use]
    pub(crate) fn nulls(&self) -> &[bool] {
        &self.nulls
    }

    /// Return the physical length.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    /// Return true when the column holds no physical rows.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Estimated resident bytes for budget accounting.
    #[must_use]
    pub(crate) fn estimated_bytes(&self) -> usize {
        self.values.capacity().saturating_mul(size_of::<Value>()) + self.nulls.capacity()
    }

    /// Retained capacity in elements, for reuse reporting.
    #[must_use]
    pub(crate) fn retained_capacity(&self) -> usize {
        self.values.capacity()
    }

    fn take_storage(self) -> (Vec<Value>, Vec<bool>) {
        (self.values, self.nulls)
    }
}

/// One typed binding batch: schema plus column-major physical storage.
///
/// Logical rows are either the dense physical rows (no selection) or the
/// rows addressed by the selection vector. [`Self::logical_rows`] is always
/// explicit; consumers never infer it from storage.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BindingBatch {
    schema: BindingTableSchema,
    columns: Vec<BatchColumn>,
    selection: Option<Vec<BatchPosition>>,
    logical_rows: usize,
}

impl BindingBatch {
    /// Build a batch from a schema and per-column values.
    ///
    /// Every column must match the schema width in count and share one
    /// physical length. The batch starts dense (no selection).
    ///
    /// # Errors
    ///
    /// Returns [`BatchError`] on width or length mismatch.
    pub(crate) fn from_columns(
        schema: BindingTableSchema,
        values: Vec<Vec<Value>>,
    ) -> Result<Self, BatchError> {
        if values.len() != schema.columns.len() {
            return Err(BatchError::SchemaWidthMismatch {
                schema_columns: schema.columns.len(),
                value_columns: values.len(),
            });
        }
        let physical = values.first().map_or(0, Vec::len);
        if values.iter().any(|column| column.len() != physical) {
            return Err(BatchError::ColumnLengthMismatch);
        }
        Ok(Self {
            schema,
            columns: values.into_iter().map(BatchColumn::from_values).collect(),
            selection: None,
            logical_rows: physical,
        })
    }

    /// Build a batch from pre-built columns, validating width and lengths.
    ///
    /// Operators use this with buffer-recycled column storage so steady state
    /// allocates nothing per batch. The batch starts dense (no selection).
    ///
    /// # Errors
    ///
    /// Returns [`BatchError`] on width or length mismatch.
    pub(crate) fn from_batch_columns(
        schema: BindingTableSchema,
        columns: Vec<BatchColumn>,
    ) -> Result<Self, BatchError> {
        if columns.len() != schema.columns.len() {
            return Err(BatchError::SchemaWidthMismatch {
                schema_columns: schema.columns.len(),
                value_columns: columns.len(),
            });
        }
        let physical = columns.first().map_or(0, BatchColumn::len);
        if columns.iter().any(|column| column.len() != physical) {
            return Err(BatchError::ColumnLengthMismatch);
        }
        Ok(Self {
            schema,
            columns,
            selection: None,
            logical_rows: physical,
        })
    }

    /// Build an empty batch (zero logical rows) with the given schema.
    ///
    /// Declared types and column order still come from `schema`, so an empty
    /// result keeps its descriptor.
    #[must_use]
    pub(crate) fn empty(schema: BindingTableSchema) -> Self {
        let columns = schema
            .columns
            .iter()
            .map(|_| BatchColumn::default())
            .collect();
        Self {
            schema,
            columns,
            selection: None,
            logical_rows: 0,
        }
    }

    /// Build the relational unit table: zero columns, exactly one logical row.
    ///
    /// One row of zero fields is not an empty input: downstream projection or
    /// mutation over this batch runs exactly once.
    #[must_use]
    pub(crate) fn unit() -> Self {
        Self {
            schema: BindingTableSchema {
                columns: Vec::new(),
            },
            columns: Vec::new(),
            selection: None,
            logical_rows: 1,
        }
    }

    /// Return true for the zero-column single-row unit table.
    #[must_use]
    pub(crate) fn is_unit(&self) -> bool {
        self.schema.columns.is_empty() && self.logical_rows == 1
    }

    /// Borrow the declared schema (column types and order).
    #[must_use]
    pub(crate) const fn schema(&self) -> &BindingTableSchema {
        &self.schema
    }

    /// Return the number of columns.
    #[must_use]
    pub(crate) fn width(&self) -> usize {
        self.columns.len()
    }

    /// Return the explicit logical row count.
    #[must_use]
    pub(crate) const fn logical_rows(&self) -> usize {
        self.logical_rows
    }

    /// Return the physical row count backing this batch.
    #[must_use]
    pub(crate) fn physical_len(&self) -> usize {
        self.columns.first().map_or(0, BatchColumn::len)
    }

    /// Borrow the active selection, when the batch is sparsely filtered.
    #[must_use]
    pub(crate) const fn selection(&self) -> Option<&Vec<BatchPosition>> {
        self.selection.as_ref()
    }

    /// Borrow one column by index.
    #[must_use]
    pub(crate) fn column(&self, index: usize) -> Option<&BatchColumn> {
        self.columns.get(index)
    }

    /// Materialize one logical row in schema-column order.
    ///
    /// The unit table yields one empty row. Positions resolve through the
    /// selection vector; every position was validated at construction or
    /// filtering time.
    #[must_use]
    pub(crate) fn logical_row(&self, logical: usize) -> Vec<Value> {
        let physical = match self.selection.as_ref() {
            Some(selection) => selection[logical].index(),
            None => logical,
        };
        debug_assert!(self.columns.iter().all(|c| physical < c.len()));
        self.columns
            .iter()
            .map(|column| column.values.get(physical).cloned().unwrap_or(Value::Null))
            .collect()
    }

    /// Materialize every logical row in order.
    #[must_use]
    pub(crate) fn logical_rows_vec(&self) -> Vec<Vec<Value>> {
        (0..self.logical_rows)
            .map(|i| self.logical_row(i))
            .collect()
    }

    /// Restrict the batch to the logical rows flagged in `keep`.
    ///
    /// `keep` has one entry per current logical row. The filter composes with
    /// any existing selection and reuses `buffer` scratch for the new
    /// selection vector, returning the previous selection storage to the
    /// buffer. Null bitmaps stay aligned because filtering only reorders
    /// positions; physical columns are never rewritten.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::KeepLengthMismatch`] when `keep` does not cover
    /// exactly the current logical rows.
    pub(crate) fn select(
        &mut self,
        keep: &[bool],
        buffer: &mut BatchBuffer,
    ) -> Result<(), BatchError> {
        if keep.len() != self.logical_rows {
            return Err(BatchError::KeepLengthMismatch {
                logical_rows: self.logical_rows,
                keep_len: keep.len(),
            });
        }
        let mut next = buffer.take_selection();
        next.clear();
        for (logical, keep_row) in keep.iter().enumerate() {
            if *keep_row {
                let physical = match self.selection.as_ref() {
                    Some(selection) => selection[logical],
                    None => BatchPosition(logical as u32),
                };
                next.push(physical);
            }
        }
        if let Some(previous) = self.selection.replace(next) {
            buffer.recycle_selection(previous);
        }
        self.logical_rows = self.selection.as_ref().map_or(0, Vec::len);
        Ok(())
    }

    /// Project one literal value across every logical row into a new batch.
    ///
    /// This is the minimal projection primitive that proves the unit-table
    /// contract: a unit input yields exactly one row, an empty input yields
    /// none. `schema` must describe exactly one column.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::SchemaWidthMismatch`] unless `schema` has one
    /// column.
    pub(crate) fn project_literal(
        &self,
        schema: BindingTableSchema,
        value: Value,
    ) -> Result<Self, BatchError> {
        if schema.columns.len() != 1 {
            return Err(BatchError::SchemaWidthMismatch {
                schema_columns: schema.columns.len(),
                value_columns: 1,
            });
        }
        let is_null = value == Value::Null;
        Ok(Self {
            schema,
            columns: vec![BatchColumn {
                values: vec![value; self.logical_rows],
                nulls: vec![is_null; self.logical_rows],
            }],
            selection: None,
            logical_rows: self.logical_rows,
        })
    }

    /// Estimated resident bytes for budget accounting.
    #[must_use]
    pub(crate) fn estimated_bytes(&self) -> usize {
        self.columns
            .iter()
            .map(BatchColumn::estimated_bytes)
            .sum::<usize>()
            + self.selection.as_ref().map_or(0, |s| {
                s.capacity().saturating_mul(size_of::<BatchPosition>())
            })
    }

    /// Return column/selection storage to `buffer` for reuse.
    pub(crate) fn recycle(self, buffer: &mut BatchBuffer) {
        for column in self.columns {
            let (values, nulls) = column.take_storage();
            buffer.recycle_column(values, nulls);
        }
        if let Some(selection) = self.selection {
            buffer.recycle_selection(selection);
        }
        buffer.recycled_batches += 1;
    }
}

/// Reusable scratch storage shared across pulls.
///
/// Operators take column vectors from the buffer instead of allocating, and
/// the tracer recycles each consumed batch back into it. This keeps steady
/// state at roughly one live batch plus retained capacity, which the
/// performance probe reports via [`Self::retained_capacity_bytes`].
#[derive(Clone, Debug, Default)]
pub(crate) struct BatchBuffer {
    value_columns: Vec<Vec<Value>>,
    null_columns: Vec<Vec<bool>>,
    selections: Vec<Vec<BatchPosition>>,
    recycled_batches: u64,
}

impl BatchBuffer {
    /// Construct an empty buffer.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            value_columns: Vec::new(),
            null_columns: Vec::new(),
            selections: Vec::new(),
            recycled_batches: 0,
        }
    }

    /// Take a value-column vector, reusing retained capacity when available.
    #[must_use]
    pub(crate) fn take_values(&mut self) -> Vec<Value> {
        self.value_columns.pop().unwrap_or_default()
    }

    /// Take a null-bitmap vector, reusing retained capacity when available.
    #[must_use]
    pub(crate) fn take_nulls(&mut self) -> Vec<bool> {
        self.null_columns.pop().unwrap_or_default()
    }

    /// Take a selection vector, reusing retained capacity when available.
    #[must_use]
    pub(crate) fn take_selection(&mut self) -> Vec<BatchPosition> {
        self.selections.pop().unwrap_or_default()
    }

    /// Return one column's storage for reuse.
    pub(crate) fn recycle_column(&mut self, mut values: Vec<Value>, mut nulls: Vec<bool>) {
        values.clear();
        nulls.clear();
        self.value_columns.push(values);
        self.null_columns.push(nulls);
    }

    /// Return one selection's storage for reuse.
    pub(crate) fn recycle_selection(&mut self, mut selection: Vec<BatchPosition>) {
        selection.clear();
        self.selections.push(selection);
    }

    /// Return the number of batches recycled through this buffer.
    #[must_use]
    pub(crate) const fn recycled_batches(&self) -> u64 {
        self.recycled_batches
    }

    /// Estimate retained (allocated but idle or live-pooled) bytes.
    #[must_use]
    pub(crate) fn retained_capacity_bytes(&self) -> usize {
        self.value_columns
            .iter()
            .map(|v| v.capacity().saturating_mul(size_of::<Value>()))
            .sum::<usize>()
            + self.null_columns.iter().map(Vec::capacity).sum::<usize>()
            + self
                .selections
                .iter()
                .map(|s| s.capacity().saturating_mul(size_of::<BatchPosition>()))
                .sum::<usize>()
    }
}

/// Rejected batch construction or filtering input.
///
/// Every variant is a caller bug (the safe constructors), never a data
/// error: operators map these to `ImplementationDefined` at the boundary and
/// hot loops rely on the validated invariants.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum BatchError {
    /// Column count disagreed with the schema width.
    #[error("batch schema width {schema_columns} disagrees with {value_columns} value columns")]
    SchemaWidthMismatch {
        /// Columns declared by the schema.
        schema_columns: usize,
        /// Columns supplied by the caller.
        value_columns: usize,
    },
    /// Physical columns disagreed in length.
    #[error("batch columns disagree in physical length")]
    ColumnLengthMismatch,
    /// A selection position addressed no physical row.
    #[error("batch selection position {position} exceeds physical length {physical_len}")]
    SelectionOutOfBounds {
        /// Offending position.
        position: usize,
        /// Physical rows available.
        physical_len: usize,
    },
    /// A selection vector covered the wrong logical row count.
    #[error("batch selection length {selection_len} disagrees with {logical_rows} logical rows")]
    SelectionLengthMismatch {
        /// Positions supplied.
        selection_len: usize,
        /// Logical rows expected.
        logical_rows: usize,
    },
    /// A sparse-filter mask covered the wrong logical row count.
    #[error("batch keep mask length {keep_len} disagrees with {logical_rows} logical rows")]
    KeepLengthMismatch {
        /// Current logical rows.
        logical_rows: usize,
        /// Mask entries supplied.
        keep_len: usize,
    },
}

/// Validate an externally supplied selection before trusting it in hot loops.
///
/// Safe constructors build selections internally, but this entry point lets
/// future operators validate foreign selections once at the boundary. Tests
/// exercise it for malformed internal selections.
pub(crate) fn validate_selection(
    selection: &[BatchPosition],
    physical_len: usize,
    logical_rows: usize,
) -> Result<(), BatchError> {
    if selection.len() != logical_rows {
        return Err(BatchError::SelectionLengthMismatch {
            selection_len: selection.len(),
            logical_rows,
        });
    }
    for position in selection {
        if position.index() >= physical_len {
            return Err(BatchError::SelectionOutOfBounds {
                position: position.index(),
                physical_len,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::plan::BindingTableColumn;

    use super::*;

    fn named_schema(names: &[&str]) -> BindingTableSchema {
        BindingTableSchema {
            columns: names
                .iter()
                .map(|name| BindingTableColumn {
                    name: Some(selene_core::db_string(name).unwrap()),
                    hidden: None,
                    ty: crate::AnalyzedType::Dynamic,
                })
                .collect(),
        }
    }

    #[test]
    fn unit_and_empty_stay_distinct() {
        let unit = BindingBatch::unit();
        assert!(unit.is_unit());
        assert_eq!(unit.width(), 0);
        assert_eq!(unit.logical_rows(), 1);
        assert_eq!(unit.logical_row(0), Vec::<Value>::new());

        let empty = BindingBatch::empty(named_schema(&["n"]));
        assert!(!empty.is_unit());
        assert_eq!(empty.logical_rows(), 0);
        assert_eq!(empty.width(), 1);
    }

    #[test]
    fn constructors_reject_mismatched_shapes() {
        let schema = named_schema(&["a", "b"]);
        assert!(matches!(
            BindingBatch::from_columns(schema.clone(), vec![vec![Value::Null]]),
            Err(BatchError::SchemaWidthMismatch { .. })
        ));
        assert!(matches!(
            BindingBatch::from_columns(
                schema,
                vec![vec![Value::Int(1)], vec![Value::Int(1), Value::Int(2)]],
            ),
            Err(BatchError::ColumnLengthMismatch)
        ));
        assert!(matches!(
            validate_selection(&[BatchPosition(7)], 4, 1),
            Err(BatchError::SelectionOutOfBounds { .. })
        ));
        assert!(matches!(
            validate_selection(&[BatchPosition(0), BatchPosition(1)], 4, 1),
            Err(BatchError::SelectionLengthMismatch { .. })
        ));
        validate_selection(&[BatchPosition(3)], 4, 1).unwrap();
    }
}
