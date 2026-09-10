//! Owned result-order metadata, independent of AST and execution arenas.

/// Direction of one result sort key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    /// Smaller non-null values precede larger ones.
    Ascending,
    /// Larger non-null values precede smaller ones.
    Descending,
}

/// Absolute placement of null values, after applying sort direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NullPlacement {
    /// Nulls precede non-null values.
    First,
    /// Nulls follow non-null values.
    Last,
}

/// One declared sort key. An unprojected key has no output-column coordinate;
/// its normalized expression text is retained rather than fabricating a field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultOrderKey {
    column: Option<usize>,
    expression: String,
    direction: SortDirection,
    nulls: NullPlacement,
}

impl ResultOrderKey {
    /// Construct metadata from the validated sort operation.
    pub fn new(
        column: Option<usize>,
        expression: String,
        direction: SortDirection,
        nulls: NullPlacement,
    ) -> Self {
        Self {
            column,
            expression,
            direction,
            nulls,
        }
    }

    /// Zero-based output column, when the key names one directly.
    #[must_use]
    pub const fn column(&self) -> Option<usize> {
        self.column
    }
    /// Canonical expression text; this is diagnostic text, not an executable AST.
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }
    /// Declared ascending/descending direction.
    #[must_use]
    pub const fn direction(&self) -> SortDirection {
        self.direction
    }
    /// Absolute null placement, including resolved implementation defaults.
    #[must_use]
    pub const fn nulls(&self) -> NullPlacement {
        self.nulls
    }
}
