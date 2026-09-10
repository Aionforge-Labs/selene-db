//! Explicit semantic property/codec boundary. This module defines no bytes,
//! serde enum layout, storage migration, or durable reference representation.

use crate::{CoreError, CoreResult, DbString, Record, Value};

/// Maximum stored-value nesting, including the scalar/container root. Checked
/// before recursive legacy decoding and before admitting values for storage.
pub const MAX_STORED_VALUE_DEPTH: usize = 256;

/// An owned, recursively validated storable value.
///
/// Scalars, native JSON/vectors, lists, and named records are admitted. Query
/// references/paths and legacy positional records are not. In particular, a
/// process-local record type ID cannot substitute for semantic field names.
/// F02-PR03 owns encoding this contract; this type deliberately has no serde
/// or rkyv implementation.
///
/// ```compile_fail
/// let value = selene_core::StoredValue::try_from(selene_core::Value::Int(1)).unwrap();
/// let bytes = postcard::to_allocvec(&value).unwrap(); // F02-PR03 owns bytes.
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct StoredValue(Value);

/// Why a runtime value cannot cross the stored-value boundary.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StoredValueError {
    /// A value exceeds the bounded semantic/legacy-codec nesting envelope.
    #[error("stored value exceeds {MAX_STORED_VALUE_DEPTH} nesting levels")]
    DepthLimit,
    /// A query-only reference, path, or opaque runtime payload.
    #[error("{family} is query-only and cannot be stored")]
    QueryOnly {
        /// Rejected runtime family, not its potentially sensitive value.
        family: &'static str,
    },
    /// Legacy positional record payload does not supply semantic field names.
    #[error("stored records require semantic field names, not a RecordTypeId")]
    MissingRecordDescriptor,
    /// A native caller supplied more than one value for an exact record field.
    #[error("duplicate stored record field {0}")]
    DuplicateField(DbString),
}

impl StoredValue {
    /// Validate a borrowed value without copying its payload. Scalar validation
    /// allocates nothing; recursive containers use an explicit traversal stack.
    pub fn validate(value: &Value) -> CoreResult<()> {
        fn check<'a>(
            value: &'a Value,
            depth: usize,
            pending: &mut Vec<(&'a Value, usize)>,
        ) -> Result<(), StoredValueError> {
            if depth > MAX_STORED_VALUE_DEPTH {
                return Err(StoredValueError::DepthLimit);
            }
            match value {
                Value::NodeRef(_)
                | Value::EdgeRef(_)
                | Value::GraphRef(_)
                | Value::TableRef(_)
                | Value::Path(_)
                | Value::Extended { .. } => {
                    return Err(StoredValueError::QueryOnly {
                        family: value.variant_name(),
                    });
                }
                Value::RecordTyped(_) => return Err(StoredValueError::MissingRecordDescriptor),
                Value::List(values) => {
                    pending.extend(values.iter().map(|value| (value, depth + 1)))
                }
                Value::Record(record) => match record.as_ref() {
                    Record::Open(fields) => {
                        let mut names = std::collections::BTreeSet::new();
                        for (name, value) in fields {
                            if !names.insert(name) {
                                return Err(StoredValueError::DuplicateField(name.clone()));
                            }
                            pending.push((value, depth + 1));
                        }
                    }
                },
                Value::Bool(_)
                | Value::Int(_)
                | Value::Uint(_)
                | Value::Int128(_)
                | Value::Uint128(_)
                | Value::Float(_)
                | Value::Float32(_)
                | Value::Decimal(_)
                | Value::String(_)
                | Value::Bytes(_)
                | Value::ZonedDateTime(_)
                | Value::LocalDateTime(_)
                | Value::Date(_)
                | Value::ZonedTime(_)
                | Value::LocalTime(_)
                | Value::Duration(_)
                | Value::Null
                | Value::Uuid(_)
                | Value::Vector(_)
                | Value::Json(_) => {}
            }
            Ok(())
        }
        let mut pending = Vec::new();
        check(value, 1, &mut pending).map_err(CoreError::StoredValue)?;
        while let Some((value, depth)) = pending.pop() {
            check(value, depth, &mut pending).map_err(CoreError::StoredValue)?;
        }
        Ok(())
    }

    /// Borrow the validated semantic value; no mutable payload access is exposed.
    #[must_use]
    pub const fn as_value(&self) -> &Value {
        &self.0
    }

    /// Consume the checked boundary into an ordinary runtime value.
    #[must_use]
    pub fn into_value(self) -> Value {
        self.0
    }
}

impl TryFrom<Value> for StoredValue {
    type Error = CoreError;

    fn try_from(value: Value) -> CoreResult<Self> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

#[cfg(test)]
#[path = "stored_value_tests.rs"]
mod tests;
