//! Typed `RECORD` field-type descriptors for the closed-graph catalog.
//!
//! Split out of `graph_types.rs` (700-LOC cap). Holds the rkyv-persistable
//! RECORD field-type family — [`RecordFieldTypes`] / [`RecordFieldTypeDef`] /
//! [`RecordFieldType`] (the `CORE/GTYP` snapshot side; the serde/WAL counterpart
//! is `selene_core::schema::RecordFieldStructure`) — plus the catalog-time
//! shape validators these types share.
//!
//! Why: Per ISO 39075:2024 §18.9 `<record type>` / `<field types specification>`
//! (GV46 closed record types) and §18.10 (GV48 nested record types).

use std::collections::BTreeSet;

use selene_core::{
    ByteStringType, CharacterStringType, DbString, DecimalType, PropertyValueType, Value,
};
use serde::{Deserialize, Serialize};

use super::MAX_RECORD_TYPE_NESTING;
use crate::error::{GraphError, GraphResult};

/// Ordered field-type descriptor list for a closed/typed `RECORD` property declaration
/// (rkyv snapshot side; persisted in `CORE/GTYP`). The serde/WAL counterpart is
/// `selene_core::schema::RecordFieldStructure`; the two carry the same structure and must
/// round-trip into each other.
// Why: Per ISO 39075:2024 §18.9 <record type> / <field types specification>; GV46 (closed
// record types, §18.9 CR42).
//
// The bytecheck/serialize/deserialize bounds block is required on every type in the
// mutual-recursion cycle (RecordFieldTypes -> RecordFieldTypeDef -> RecordFieldType ->
// RecordFieldTypes) so the `omit_bounds` boundary's manual bounds propagate uniformly up
// to the `Arc<GraphTypeDef>` archive check in `GraphMeta`.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
#[rkyv(
    bytecheck(bounds(__C: rkyv::validation::ArchiveContext)),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source)
)]
pub struct RecordFieldTypes(pub Vec<RecordFieldTypeDef>);

/// One declared field within a typed `RECORD`: name, its (possibly nested) type, and
/// whether the field is required (non-nullable).
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
#[rkyv(
    bytecheck(bounds(__C: rkyv::validation::ArchiveContext)),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source)
)]
pub struct RecordFieldTypeDef {
    /// Field name.
    pub name: DbString,
    /// Declared field type (recursively nestable).
    pub field_type: RecordFieldType,
    /// `true` when the field is required (NOT NULL).
    pub required: bool,
}

/// Persistable, recursively-nestable field-type descriptor for typed `RECORD` declarations.
///
/// Kept `#[non_exhaustive]` like [`super::PropertyElementType`]: its conversions and
/// `matches` live in this crate, so same-crate exhaustiveness checks still apply while
/// downstream crates remain forward-compatible.
// Why: Per ISO 39075:2024 §18.10 CR1 (GV48 nested record types) — a field type may itself
// contain a list or record type.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
#[rkyv(
    bytecheck(bounds(__C: rkyv::validation::ArchiveContext)),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source)
)]
#[non_exhaustive]
pub enum RecordFieldType {
    /// Scalar field type.
    Scalar(PropertyValueType),
    /// STRING field type with a user-specified length envelope.
    CharacterString(CharacterStringType),
    /// DECIMAL field type with a user-specified precision/scale envelope.
    Decimal(DecimalType),
    /// BYTES field type with a user-specified length envelope.
    ByteString(ByteStringType),
    /// `LIST` field type.
    List(#[rkyv(omit_bounds)] Box<RecordFieldType>),
    /// Open/bare nested `RECORD` field type.
    OpenRecord,
    /// Closed/typed nested `RECORD` field type.
    Record(#[rkyv(omit_bounds)] Box<RecordFieldTypes>),
    /// Explicitly non-null field or nested element type.
    NotNull(#[rkyv(omit_bounds)] Box<RecordFieldType>),
}

impl RecordFieldType {
    /// Return true when `value` structurally conforms to this declared field type.
    #[must_use]
    pub fn matches(&self, value: &Value) -> bool {
        self.structural_type().is_ok_and(|ty| ty.matches(value))
    }
}

impl RecordFieldTypes {
    /// Return true when `value` structurally conforms to this closed-record descriptor.
    ///
    /// A `Value::Record(Record::Open)` is checked by field name with **set equality** —
    /// every declared field must be present (or optional) and match, and no undeclared
    /// extra field may appear. A legacy `Value::RecordTyped` cannot establish field-name
    /// identity and is rejected. Per ISO 39075:2024 §4.15.4 a
    /// closed record value must have the same field-name set as the descriptor.
    ///
    /// An explicit `Value::Null` for a field conforms unless that field is declared
    /// `NOT NULL`. Missing open-record fields never conform because a closed record
    /// value must have the same field-name set as the descriptor.
    #[must_use]
    pub fn matches(&self, value: &Value) -> bool {
        self.structural_type().is_ok_and(|ty| ty.matches(value))
    }
}

/// Validate the shape of a typed-`RECORD` field-type list at catalog time:
/// nesting budget, unique field names, and recursively-valid field types.
pub(super) fn validate_record_field_types(
    type_name: DbString,
    property_name: DbString,
    fields: &RecordFieldTypes,
    depth: u32,
) -> GraphResult<()> {
    if depth > MAX_RECORD_TYPE_NESTING {
        return Err(GraphError::Inconsistent {
            reason: format!(
                "property {property_name} on type {type_name} exceeds RECORD nesting limit"
            ),
        });
    }
    // ISO 39075:2024 §18.10 SR2: a field name shall not equal another field name.
    let mut seen = BTreeSet::new();
    for field in &fields.0 {
        if !seen.insert(field.name.clone()) {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "property {property_name} on type {type_name} declares duplicate record field name {}",
                    field.name
                ),
            });
        }
        validate_record_field_type(
            type_name.clone(),
            property_name.clone(),
            &field.field_type,
            depth,
        )?;
    }
    Ok(())
}

fn validate_record_field_type(
    type_name: DbString,
    property_name: DbString,
    field_type: &RecordFieldType,
    depth: u32,
) -> GraphResult<()> {
    match field_type {
        RecordFieldType::Scalar(
            value_type @ (PropertyValueType::List
            | PropertyValueType::Record
            | PropertyValueType::RecordTyped),
        ) => Err(GraphError::Inconsistent {
            reason: format!(
                "property {property_name} on type {type_name} uses an unsupported scalar RECORD field type {value_type}"
            ),
        }),
        RecordFieldType::Scalar(_)
        | RecordFieldType::CharacterString(_)
        | RecordFieldType::Decimal(_)
        | RecordFieldType::ByteString(_) => Ok(()),
        RecordFieldType::List(inner) => {
            validate_record_field_type(type_name, property_name, inner, depth + 1)
        }
        RecordFieldType::OpenRecord => Ok(()),
        RecordFieldType::Record(inner) => {
            validate_record_field_types(type_name, property_name, inner, depth + 1)
        }
        RecordFieldType::NotNull(inner) => {
            validate_record_field_type(type_name, property_name, inner, depth)
        }
    }
}
