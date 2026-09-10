//! Core error types and ISO GQLSTATUS mappings.

use crate::db_string::DbString;

/// Result alias for `selene-core` operations.
pub type CoreResult<T> = Result<T, CoreError>;

/// Error type for foundation data-model operations.
///
/// Codes in the `0Gxxx` range are selene-db implementation-defined conditions
/// reserved for engine-internal validation and registry failures.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum CoreError {
    /// A runtime value is not admitted by the semantic stored-value contract.
    #[error(transparent)]
    #[diagnostic(code(SLENE_C_017))]
    StoredValue(#[from] crate::StoredValueError),
    /// A string or byte-string exceeded the implementation-defined length.
    #[error("string too long: {got} bytes (max {max})")]
    #[diagnostic(code(SLENE_C_002))]
    StringTooLong {
        /// Observed byte length.
        got: usize,
        /// Maximum byte length.
        max: u32,
    },

    /// A list or record exceeded the implementation-defined cardinality.
    #[error("constructed value too large: {got} elements (max {max})")]
    #[diagnostic(code(SLENE_C_003))]
    ConstructedValueTooLarge {
        /// Observed element count.
        got: usize,
        /// Maximum element count.
        max: u32,
    },

    /// A decimal exceeded the implementation-defined significant-digit precision.
    #[error("decimal precision exceeded: {got} significant digits (max {max})")]
    #[diagnostic(code(SLENE_C_004))]
    DecimalPrecisionExceeded {
        /// Observed significant-digit count.
        got: u32,
        /// Maximum significant-digit count.
        max: u32,
    },

    /// A native dense vector was constructed without components.
    #[error("vector must contain at least one component")]
    #[diagnostic(code(SLENE_C_010))]
    VectorEmpty,

    /// A native dense vector exceeded the implementation-defined dimension cap.
    #[error("vector dimension too large: {got} components (max {max})")]
    #[diagnostic(code(SLENE_C_011))]
    VectorTooLarge {
        /// Observed component count.
        got: usize,
        /// Maximum component count.
        max: usize,
    },

    /// A native dense vector component was NaN or infinite.
    #[error("vector component {index} is not finite: {value}")]
    #[diagnostic(code(SLENE_C_012))]
    VectorComponentNotFinite {
        /// Zero-based component index.
        index: usize,
        /// Rejected component value.
        value: f32,
    },

    /// Two native dense vectors had incompatible dimensions for metric work.
    #[error("vector dimensions do not match: lhs has {lhs} components, rhs has {rhs}")]
    #[diagnostic(code(SLENE_C_013))]
    VectorDimensionMismatch {
        /// Left-hand vector dimension.
        lhs: usize,
        /// Right-hand vector dimension.
        rhs: usize,
    },

    /// A cosine-distance vector had zero magnitude.
    #[error("cosine distance is undefined for zero-norm vector on {side}")]
    #[diagnostic(code(SLENE_C_014))]
    VectorZeroNorm {
        /// Which side of the comparison was zero magnitude.
        side: &'static str,
    },

    /// Identifier value zero is reserved as the tombstone sentinel.
    #[error("invalid identifier: zero is reserved as tombstone sentinel")]
    #[diagnostic(code(SLENE_C_007))]
    ZeroIdentifier,

    /// Compact `PropertyMap` was constructed with mismatched key and value counts.
    #[error("compact property map key/value length mismatch: {keys} keys, {values} values")]
    #[diagnostic(code(SLENE_C_008))]
    CompactKeyValueLengthMismatch {
        /// Number of keys supplied.
        keys: usize,
        /// Number of value slots supplied.
        values: usize,
    },

    /// A label diff or property diff named the same key in both add/set and remove.
    #[error("overlapping {kind} diff: key {key} appears in both add/set and remove")]
    #[diagnostic(code(SLENE_C_009))]
    OverlappingDiff {
        /// `"label"` or `"property"`.
        kind: &'static str,
        /// The contradicting key.
        key: DbString,
    },

    /// JSON text could not be parsed.
    #[error("invalid JSON text: {message}")]
    #[diagnostic(code(SLENE_C_015))]
    JsonParse {
        /// Parser diagnostic.
        message: String,
    },

    /// A JSON Patch document or operation is invalid.
    #[error("invalid JSON Patch: {message}")]
    #[diagnostic(code(SLENE_C_016))]
    JsonPatch {
        /// Patch diagnostic.
        message: String,
    },
}

impl CoreError {
    /// Map this error to its 5-character ISO GQLSTATUS code.
    ///
    /// ISO/IEC 39075:2024 clause 23 defines the status-code shape. Spec 02
    /// section 3.1 binds the value-limit and numeric-limit choices used here.
    #[must_use]
    pub const fn gqlstatus(&self) -> &'static str {
        match self {
            Self::StoredValue(_) => "22G03",
            Self::StringTooLong { .. } | Self::ConstructedValueTooLarge { .. } => "22G03",
            Self::DecimalPrecisionExceeded { .. } | Self::VectorComponentNotFinite { .. } => {
                "22003"
            }
            Self::VectorEmpty | Self::VectorTooLarge { .. } => "22G03",
            Self::VectorDimensionMismatch { .. } => "22G04",
            Self::VectorZeroNorm { .. } => "22012",
            Self::ZeroIdentifier => "0G003",
            Self::CompactKeyValueLengthMismatch { .. } => "0G008",
            Self::OverlappingDiff { .. } => "0G009",
            Self::JsonParse { .. } => "22018",
            Self::JsonPatch { .. } => "22G03",
        }
    }
}

#[cfg(test)]
mod tests {
    use miette::Diagnostic;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(CoreError::StringTooLong { got: 2, max: 1 }, "22G03", "SLENE_C_002")]
    #[case(
        CoreError::ConstructedValueTooLarge { got: 2, max: 1 },
        "22G03",
        "SLENE_C_003"
    )]
    #[case(
        CoreError::DecimalPrecisionExceeded { got: 29, max: 28 },
        "22003",
        "SLENE_C_004"
    )]
    #[case(CoreError::VectorEmpty, "22G03", "SLENE_C_010")]
    #[case(
        CoreError::VectorTooLarge { got: 65_536, max: 65_535 },
        "22G03",
        "SLENE_C_011"
    )]
    #[case(
        CoreError::VectorComponentNotFinite { index: 1, value: f32::INFINITY },
        "22003",
        "SLENE_C_012"
    )]
    #[case(
        CoreError::VectorDimensionMismatch { lhs: 2, rhs: 3 },
        "22G04",
        "SLENE_C_013"
    )]
    #[case(
        CoreError::VectorZeroNorm { side: "lhs" },
        "22012",
        "SLENE_C_014"
    )]
    #[case(CoreError::ZeroIdentifier, "0G003", "SLENE_C_007")]
    #[case(
        CoreError::CompactKeyValueLengthMismatch { keys: 2, values: 1 },
        "0G008",
        "SLENE_C_008"
    )]
    #[case(
        CoreError::OverlappingDiff { kind: "label", key: crate::db_string("err.test.overlap").unwrap() },
        "0G009",
        "SLENE_C_009"
    )]
    #[case(
        CoreError::JsonParse { message: "expected value".to_owned() },
        "22018",
        "SLENE_C_015"
    )]
    #[case(
        CoreError::JsonPatch { message: "missing op".to_owned() },
        "22G03",
        "SLENE_C_016"
    )]
    fn gqlstatus_and_diagnostic_code_match(
        #[case] error: CoreError,
        #[case] gqlstatus: &str,
        #[case] diagnostic_code: &str,
    ) {
        assert_eq!(error.gqlstatus(), gqlstatus);
        assert!(
            crate::gqlstatus_name(gqlstatus).is_some(),
            "GQLSTATUS code {gqlstatus} emitted by CoreError but not in ALL_GQLSTATUS_NAMES"
        );
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some(diagnostic_code)
        );
    }

    #[test]
    fn display_includes_structured_field_values() {
        let error = CoreError::StringTooLong { got: 7, max: 3 };
        let rendered = error.to_string();
        assert!(rendered.contains('7'));
        assert!(rendered.contains('3'));
    }
}
