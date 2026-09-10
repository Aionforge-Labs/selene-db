//! Persistable default-value descriptors for closed graph property declarations.

use std::collections::BTreeSet;

use selene_core::{DbString, JsonValue, Record, Value, VectorValue, db_string};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::error::{GraphError, GraphResult};

/// One field in a persistable record default descriptor.
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
    bytecheck(bounds(
        __C: rkyv::validation::ArchiveContext,
        <__C as rkyv::rancor::Fallible>::Error: rkyv::rancor::Source
    )),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source)
)]
pub struct PropertyDefaultRecordField {
    /// Field name.
    pub name: DbString,
    /// Field default descriptor.
    #[rkyv(omit_bounds)]
    pub value: Box<PropertyDefaultValue>,
}

/// Persistable default-value descriptor for closed graph property declarations.
#[derive(Clone, Debug, Eq, Hash, PartialEq, rkyv::Archive, rkyv::Deserialize, rkyv::Serialize)]
#[rkyv(
    bytecheck(bounds(__C: rkyv::validation::ArchiveContext)),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source)
)]
#[non_exhaustive]
pub enum PropertyDefaultValue {
    /// Null default.
    Null,
    /// Boolean default.
    Boolean(bool),
    /// Signed integer default.
    Integer(i64),
    /// Database-string default.
    String(DbString),
    /// Byte-string default.
    Bytes(Vec<u8>),
    /// List default with recursively materializable default descriptors.
    List(#[rkyv(omit_bounds)] Vec<Box<PropertyDefaultValue>>),
    /// Open record default with recursively materializable field descriptors.
    Record(#[rkyv(omit_bounds)] Vec<PropertyDefaultRecordField>),
    /// Canonical UUID text default.
    Uuid(DbString),
    /// Canonical JSON text default.
    Json(DbString),
    /// Default floating-point value, stored as canonical IEEE 754 binary64 bits.
    Float(u64),
    /// Distinct 32-bit floating-point value, stored as canonical IEEE 754 binary32 bits.
    Float32(u32),
    /// Zoned datetime default, stored as canonical temporal text.
    ZonedDateTime(DbString),
    /// Local datetime default, stored as canonical temporal text.
    LocalDateTime(DbString),
    /// Date default, stored as canonical temporal text.
    Date(DbString),
    /// Zoned time default, stored as canonical temporal text.
    ZonedTime(DbString),
    /// Local time default, stored as canonical temporal text.
    LocalTime(DbString),
    /// Duration default, stored as canonical temporal text.
    Duration(DbString),
    /// Unsigned integer default.
    Uint(u64),
    /// Signed 128-bit integer default.
    Int128(i128),
    /// Unsigned 128-bit integer default.
    Uint128(u128),
    /// Fixed-precision decimal default, stored as canonical decimal text.
    Decimal(DbString),
    /// Dense vector default, stored as canonical IEEE 754 binary32 bits.
    Vector(Vec<u32>),
}

impl PropertyDefaultValue {
    /// Materialize this descriptor as a runtime value.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] if a persisted float default is not
    /// finite, or if a persisted decimal/UUID/JSON/temporal default no longer
    /// parses as a valid value.
    pub fn to_value(&self) -> GraphResult<Value> {
        let mut pending = vec![(self, 1)];
        while let Some((value, depth)) = pending.pop() {
            if depth > selene_core::MAX_STORED_VALUE_DEPTH {
                return Err(selene_core::CoreError::from(
                    selene_core::StoredValueError::DepthLimit,
                )
                .into());
            }
            match value {
                Self::List(values) => {
                    pending.extend(values.iter().map(|value| (value.as_ref(), depth + 1)))
                }
                Self::Record(fields) => {
                    pending.extend(fields.iter().map(|field| (field.value.as_ref(), depth + 1)))
                }
                _ => {}
            }
        }
        self.materialize()
    }

    fn materialize(&self) -> GraphResult<Value> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Boolean(value) => Value::Bool(*value),
            Self::Integer(value) => Value::Int(*value),
            Self::Uint(value) => Value::Uint(*value),
            Self::Int128(value) => Value::Int128(*value),
            Self::Uint128(value) => Value::Uint128(*value),
            Self::Decimal(value) => {
                Value::Decimal(
                    value
                        .as_str()
                        .parse()
                        .map_err(|err| GraphError::Inconsistent {
                            reason: format!("persisted DECIMAL property default is invalid: {err}"),
                        })?,
                )
            }
            Self::Float(bits) => {
                let value = f64::from_bits(*bits);
                if !value.is_finite() {
                    return Err(GraphError::Inconsistent {
                        reason: "persisted FLOAT property default is not finite".to_owned(),
                    });
                }
                Value::Float(value)
            }
            Self::Float32(bits) => {
                let value = f32::from_bits(*bits);
                if !value.is_finite() {
                    return Err(GraphError::Inconsistent {
                        reason: "persisted FLOAT32 property default is not finite".to_owned(),
                    });
                }
                Value::Float32(value)
            }
            Self::ZonedDateTime(value) => Value::ZonedDateTime(Box::new(
                parse_zoned_datetime_default(value.as_str(), "ZONED DATETIME")?,
            )),
            Self::LocalDateTime(value) => Value::LocalDateTime(
                value
                    .as_str()
                    .parse()
                    .map_err(|err| invalid_temporal_default("LOCAL DATETIME", err))?,
            ),
            Self::Date(value) => Value::Date(
                value
                    .as_str()
                    .parse()
                    .map_err(|err| invalid_temporal_default("DATE", err))?,
            ),
            Self::ZonedTime(value) => {
                Value::ZonedTime(Box::new(parse_zoned_time_default(value.as_str())?))
            }
            Self::LocalTime(value) => Value::LocalTime(
                value
                    .as_str()
                    .parse()
                    .map_err(|err| invalid_temporal_default("LOCAL TIME", err))?,
            ),
            Self::Duration(value) => Value::Duration(Box::new(
                value
                    .as_str()
                    .parse()
                    .map_err(|err| invalid_temporal_default("DURATION", err))?,
            )),
            Self::String(value) => Value::String(value.clone()),
            Self::Bytes(value) => Value::Bytes(value.clone().into()),
            Self::List(values) => Value::List(
                values
                    .iter()
                    .map(|value| value.materialize())
                    .collect::<GraphResult<Vec<_>>>()?,
            ),
            Self::Record(fields) => {
                let mut seen = BTreeSet::new();
                let mut values = SmallVec::<[(DbString, Value); 4]>::new();
                for field in fields {
                    if !seen.insert(field.name.clone()) {
                        return Err(GraphError::Inconsistent {
                            reason: format!(
                                "persisted RECORD property default contains duplicate field {}",
                                field.name
                            ),
                        });
                    }
                    values.push((field.name.clone(), field.value.materialize()?));
                }
                Value::Record(Box::new(Record::Open(values)))
            }
            Self::Uuid(value) => {
                Value::Uuid(
                    value
                        .as_str()
                        .parse()
                        .map_err(|err| GraphError::Inconsistent {
                            reason: format!("persisted UUID property default is invalid: {err}"),
                        })?,
                )
            }
            Self::Json(value) => {
                Value::Json(JsonValue::parse_str(value.as_str()).map_err(|err| {
                    GraphError::Inconsistent {
                        reason: format!("persisted JSON property default is invalid: {err}"),
                    }
                })?)
            }
            Self::Vector(bits) => {
                let components = bits.iter().copied().map(f32::from_bits).collect::<Vec<_>>();
                Value::Vector(VectorValue::new(components).map_err(|err| {
                    GraphError::Inconsistent {
                        reason: format!("persisted VECTOR property default is invalid: {err}"),
                    }
                })?)
            }
        })
    }

    /// Convert a runtime value into a persistable default descriptor.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        selene_core::StoredValue::validate(value).ok()?;
        Self::from_validated_value(value)
    }

    fn from_validated_value(value: &Value) -> Option<Self> {
        match value {
            Value::Null => Some(Self::Null),
            Value::Bool(value) => Some(Self::Boolean(*value)),
            Value::Int(value) => Some(Self::Integer(*value)),
            Value::Uint(value) => Some(Self::Uint(*value)),
            Value::Int128(value) => Some(Self::Int128(*value)),
            Value::Uint128(value) => Some(Self::Uint128(*value)),
            Value::Decimal(value) => db_string(&value.to_string()).ok().map(Self::Decimal),
            Value::Float(value) if value.is_finite() => {
                Some(Self::Float(canonical_f64_bits(*value)))
            }
            Value::Float32(value) if value.is_finite() => {
                Some(Self::Float32(canonical_f32_bits(*value)))
            }
            Value::ZonedDateTime(value) => db_string(&zoned_datetime_image(value))
                .ok()
                .map(Self::ZonedDateTime),
            Value::LocalDateTime(value) => {
                db_string(&value.to_string()).ok().map(Self::LocalDateTime)
            }
            Value::Date(value) => db_string(&value.to_string()).ok().map(Self::Date),
            Value::ZonedTime(value) => db_string(&zoned_time_image(value))
                .ok()
                .map(Self::ZonedTime),
            Value::LocalTime(value) => db_string(&value.to_string()).ok().map(Self::LocalTime),
            Value::Duration(value) => db_string(&value.to_string()).ok().map(Self::Duration),
            Value::String(value) => Some(Self::String(value.clone())),
            Value::Bytes(value) => Some(Self::Bytes(value.to_vec())),
            Value::List(values) => values
                .iter()
                .map(Self::from_validated_value)
                .map(|value| value.map(Box::new))
                .collect::<Option<Vec<_>>>()
                .map(Self::List),
            Value::Record(record) => match record.as_ref() {
                Record::Open(fields) => {
                    let mut seen = BTreeSet::new();
                    fields
                        .iter()
                        .map(|(name, value)| {
                            if !seen.insert(name.clone()) {
                                return None;
                            }
                            Some(PropertyDefaultRecordField {
                                name: name.clone(),
                                value: Box::new(Self::from_validated_value(value)?),
                            })
                        })
                        .collect::<Option<Vec<_>>>()
                        .map(Self::Record)
                }
                _ => None,
            },
            Value::RecordTyped(_) => None,
            Value::Uuid(value) => db_string(&value.to_string()).ok().map(Self::Uuid),
            Value::Json(value) => db_string(&value.to_canonical_string()).ok().map(Self::Json),
            Value::Vector(value) => Some(Self::Vector(
                value
                    .as_slice()
                    .iter()
                    .copied()
                    .map(canonical_f32_bits)
                    .collect(),
            )),
            _ => None,
        }
    }
}

fn canonical_f64_bits(value: f64) -> u64 {
    if value == 0.0 {
        0.0_f64.to_bits()
    } else {
        value.to_bits()
    }
}

fn canonical_f32_bits(value: f32) -> u32 {
    if value == 0.0 {
        0.0_f32.to_bits()
    } else {
        value.to_bits()
    }
}

fn zoned_datetime_image(value: &jiff::Zoned) -> String {
    format!("{}{}", value.datetime(), value.offset())
}

fn zoned_time_image(value: &jiff::Zoned) -> String {
    format!("{}{}", value.time(), value.offset())
}

fn parse_zoned_datetime_default(text: &str, kind: &'static str) -> GraphResult<jiff::Zoned> {
    let pieces = jiff::fmt::temporal::DateTimeParser::new()
        .parse_pieces(text)
        .map_err(|err| invalid_temporal_default(kind, err))?;
    let time = pieces.time().ok_or_else(|| GraphError::Inconsistent {
        reason: format!("persisted {kind} property default requires a time"),
    })?;
    let zone = pieces
        .to_time_zone()
        .map_err(|err| invalid_temporal_default(kind, err))?
        .or_else(|| pieces.to_numeric_offset().map(jiff::tz::TimeZone::fixed))
        .ok_or_else(|| GraphError::Inconsistent {
            reason: format!("persisted {kind} property default requires a time zone displacement"),
        })?;
    pieces
        .date()
        .to_datetime(time)
        .to_zoned(zone)
        .map_err(|err| invalid_temporal_default(kind, err))
}

fn parse_zoned_time_default(text: &str) -> GraphResult<jiff::Zoned> {
    let anchored = format!("1970-01-01T{text}");
    parse_zoned_datetime_default(&anchored, "ZONED TIME")
}

fn invalid_temporal_default(kind: &'static str, err: impl std::fmt::Display) -> GraphError {
    GraphError::Inconsistent {
        reason: format!("persisted {kind} property default is invalid: {err}"),
    }
}
