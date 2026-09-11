//! Explicit value tags; no call to the legacy Value serde adapter.

use super::{Budget, CodecError as E, CodecResult, Decoder, Encoder, Limits};
use crate::{Record, StoredValue, Value, VectorValue, db_string};

/// Encode one format-2 stored value, rejecting query-only identities recursively.
pub fn encode_value(value: &StoredValue, limits: Limits) -> CodecResult<Vec<u8>> {
    let mut encoder = Encoder::new(limits)?;
    encoder.value(value.as_value(), 1)?;
    Ok(encoder.finish())
}

/// Decode exactly one format-2 stored value; trailing bytes are invalid.
pub fn decode_value(bytes: &[u8], limits: Limits) -> CodecResult<StoredValue> {
    let mut budget = Budget::new(limits)?;
    let mut decoder = Decoder::new(bytes, &mut budget)?;
    let value = decoder.value(1)?;
    decoder.finish()?;
    StoredValue::try_from(value).map_err(|_| E::Semantic)
}

impl Budget {
    /// Charge owned recursive containers before cloning an existing logical value
    /// into isolated replay. Shared strings/JSON/vector/byte storage stays shared.
    pub fn stored_clone(&mut self, value: &Value) -> CodecResult<()> {
        let mut pending = vec![(value, 1)];
        while let Some((value, depth)) = pending.pop() {
            self.depth(depth)?;
            self.charge(1, 128)?;
            match value {
                Value::List(values) => {
                    self.charge(0, values.len().checked_mul(256).ok_or(E::Limit)?)?;
                    pending.extend(values.iter().map(|v| (v, depth + 1)));
                }
                Value::Record(record) => {
                    let Record::Open(fields) = record.as_ref();
                    self.charge(0, fields.len().checked_mul(256).ok_or(E::Limit)?)?;
                    pending.extend(fields.iter().map(|(_, v)| (v, depth + 1)));
                }
                Value::NodeRef(_)
                | Value::EdgeRef(_)
                | Value::GraphRef(_)
                | Value::TableRef(_)
                | Value::Path(_)
                | Value::RecordTyped(_)
                | Value::Extended { .. } => return Err(E::Semantic),
                _ => {}
            }
        }
        Ok(())
    }
}

impl Encoder {
    /// Append a semantic value with a depth inherited from its enclosing payload.
    pub fn value(&mut self, value: &Value, depth: usize) -> CodecResult<()> {
        self.budget.depth(depth)?;
        self.budget.charge(1, 128)?;
        match value {
            Value::Null => self.u8(0),
            Value::Bool(v) => {
                self.u8(1)?;
                self.boolean(*v)
            }
            Value::Int(v) => {
                self.u8(2)?;
                self.fixed(&v.to_le_bytes())
            }
            Value::Uint(v) => {
                self.u8(3)?;
                self.u64(*v)
            }
            Value::Int128(v) => {
                self.u8(4)?;
                self.fixed(&v.to_le_bytes())
            }
            Value::Uint128(v) => {
                self.u8(5)?;
                self.fixed(&v.to_le_bytes())
            }
            Value::Float(v) => {
                self.u8(6)?;
                self.u64(v.to_bits())
            }
            Value::Float32(v) => {
                self.u8(7)?;
                self.u32(v.to_bits())
            }
            Value::Decimal(v) => {
                self.u8(8)?;
                self.fixed(&v.serialize())
            }
            Value::String(v) => {
                self.u8(9)?;
                self.text(v.as_str())
            }
            Value::Bytes(v) => {
                self.u8(10)?;
                self.blob(v)
            }
            Value::List(values) => {
                self.u8(11)?;
                self.count(values.len())?;
                for value in values {
                    self.value(value, depth + 1)?;
                }
                Ok(())
            }
            Value::Record(record) => {
                let Record::Open(fields) = record.as_ref();
                self.u8(12)?;
                self.count(fields.len())?;
                // Preserve declared field order. Duplicates, unlike order, are invalid.
                let mut names = std::collections::BTreeSet::new();
                for (name, value) in fields {
                    if !names.insert(name) {
                        return Err(E::Invalid("duplicate record field"));
                    }
                    self.text(name.as_str())?;
                    self.value(value, depth + 1)?;
                }
                Ok(())
            }
            Value::Uuid(v) => {
                self.u8(13)?;
                self.fixed(v.as_bytes())
            }
            Value::Vector(v) => {
                self.u8(14)?;
                self.budget
                    .charge(v.as_slice().len(), v.as_slice().len() * 16)?;
                self.u32(v.as_slice().len() as u32)?;
                for component in v.as_slice() {
                    self.u32(component.to_bits())?;
                }
                Ok(())
            }
            Value::Json(v) => {
                let mut buffer = BoundedTextBuffer {
                    bytes: Vec::new(),
                    limit: self.budget.json_limit(),
                };
                serde_json::to_writer(&mut buffer, v.as_serde()).map_err(|_| E::Limit)?;
                self.budget.charge(
                    buffer.bytes.len(),
                    buffer.bytes.len().checked_mul(128).ok_or(E::Limit)?,
                )?;
                self.u8(15)?;
                self.blob(&buffer.bytes)
            }
            Value::Date(v) => self.temporal(16, v),
            Value::LocalTime(v) => self.temporal(17, v),
            Value::LocalDateTime(v) => self.temporal(18, v),
            Value::ZonedTime(v) => self.temporal(19, v),
            Value::ZonedDateTime(v) => self.temporal(20, v),
            Value::Duration(v) => {
                self.u8(21)?;
                for component in [
                    i64::from(v.get_years()),
                    i64::from(v.get_months()),
                    i64::from(v.get_weeks()),
                    i64::from(v.get_days()),
                    i64::from(v.get_hours()),
                    v.get_minutes(),
                    v.get_seconds(),
                    v.get_milliseconds(),
                    v.get_microseconds(),
                    v.get_nanoseconds(),
                ] {
                    self.fixed(&component.to_le_bytes())?;
                }
                Ok(())
            }
            Value::NodeRef(_)
            | Value::EdgeRef(_)
            | Value::GraphRef(_)
            | Value::TableRef(_)
            | Value::Path(_)
            | Value::Extended { .. }
            | Value::RecordTyped(_) => Err(E::Semantic),
        }
    }
    fn temporal(&mut self, tag: u8, value: &impl std::fmt::Display) -> CodecResult<()> {
        use std::io::Write as _;
        let mut buffer = BoundedTextBuffer {
            bytes: Vec::new(),
            limit: 512,
        };
        write!(&mut buffer, "{value}").map_err(|_| E::Limit)?;
        self.u8(tag)?;
        self.blob(&buffer.bytes)
    }
}

impl Decoder<'_, '_> {
    /// Read one bounded semantic value. Query-only tags are deliberately unassigned.
    pub fn value(&mut self, depth: usize) -> CodecResult<Value> {
        let mut pending: Vec<Container> = Vec::new();
        loop {
            self.budget.depth(depth + pending.len())?;
            if let Some(Container::Record { key, names, .. }) = pending.last_mut() {
                let name = db_string(self.text()?).map_err(|_| E::Semantic)?;
                if !names.insert(name.clone()) {
                    return Err(E::Invalid("duplicate record field"));
                }
                *key = Some(name);
            }
            self.budget.charge(1, 128)?;
            let tag = self.u8()?;
            let mut value = match tag {
                11 => {
                    let count = self.count()?;
                    if count != 0 {
                        pending.push(Container::List {
                            remaining: count,
                            values: Vec::with_capacity(count),
                        });
                        continue;
                    }
                    Value::List(Vec::new())
                }
                12 => {
                    let count = self.count()?;
                    if count != 0 {
                        pending.push(Container::Record {
                            remaining: count,
                            fields: Vec::with_capacity(count),
                            key: None,
                            names: std::collections::BTreeSet::new(),
                        });
                        continue;
                    }
                    Value::Record(Box::new(Record::Open(smallvec::SmallVec::new())))
                }
                _ => self.scalar_value(tag)?,
            };
            loop {
                let Some(container) = pending.last_mut() else {
                    return Ok(value);
                };
                let remaining = match container {
                    Container::List { remaining, values } => {
                        values.push(value);
                        remaining
                    }
                    Container::Record {
                        remaining,
                        fields,
                        key,
                        ..
                    } => {
                        fields.push((key.take().expect("record key read before value"), value));
                        remaining
                    }
                };
                *remaining -= 1;
                if *remaining != 0 {
                    break;
                }
                value = match pending.pop().expect("completed container") {
                    Container::List { values, .. } => Value::List(values),
                    Container::Record { fields, .. } => {
                        Value::Record(Box::new(Record::Open(fields.into_iter().collect())))
                    }
                };
            }
        }
    }

    fn scalar_value(&mut self, tag: u8) -> CodecResult<Value> {
        Ok(match tag {
            0 => Value::Null,
            1 => Value::Bool(self.boolean()?),
            2 => Value::Int(self.u64()? as i64),
            3 => Value::Uint(self.u64()?),
            4 => Value::Int128(i128::from_le_bytes(
                self.take(16)?.try_into().expect("checked"),
            )),
            5 => Value::Uint128(u128::from_le_bytes(
                self.take(16)?.try_into().expect("checked"),
            )),
            6 => Value::Float(f64::from_bits(self.u64()?)),
            7 => Value::Float32(f32::from_bits(self.u32()?)),
            8 => {
                let bytes: [u8; 16] = self.take(16)?.try_into().expect("checked");
                let flags = u32::from_le_bytes(bytes[..4].try_into().expect("checked"));
                if flags & !0x80ff_0000 != 0 || (flags >> 16) & 0xff > 28 {
                    return Err(E::Invalid("decimal flags"));
                }
                Value::Decimal(crate::Decimal::deserialize(bytes))
            }
            9 => Value::String(db_string(self.text()?).map_err(|_| E::Semantic)?),
            10 => Value::Bytes(self.blob()?.into()),
            13 => Value::Uuid(crate::Uuid::from_bytes(
                self.take(16)?.try_into().expect("checked"),
            )),
            14 => {
                let count = self.u32()? as usize;
                if !(1..=crate::MAX_VECTOR_DIMENSION).contains(&count) {
                    return Err(E::Limit);
                }
                self.budget.charge(count, count * 16)?;
                let bytes = self.take(count * 4)?;
                let components: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_bits(u32::from_le_bytes(b.try_into().expect("four bytes"))))
                    .collect();
                Value::Vector(VectorValue::new(components).map_err(|_| E::Semantic)?)
            }
            15 => {
                let text = self.text()?;
                self.budget
                    .charge(text.len(), text.len().checked_mul(128).ok_or(E::Limit)?)?;
                let json = crate::JsonValue::parse_str(text).map_err(|_| E::Semantic)?;
                if json.to_canonical_string() != text {
                    return Err(E::Invalid("JSON canonical text"));
                }
                Value::Json(json)
            }
            16 => Value::Date(parse_temporal(self.text()?)?),
            17 => Value::LocalTime(parse_temporal(self.text()?)?),
            18 => Value::LocalDateTime(parse_temporal(self.text()?)?),
            19 => Value::ZonedTime(Box::new(parse_temporal(self.text()?)?)),
            20 => Value::ZonedDateTime(Box::new(parse_temporal(self.text()?)?)),
            21 => {
                let mut span = jiff::Span::new();
                for unit in 0..10 {
                    let amount = self.u64()? as i64;
                    // Jiff's sign is shared by all fields: mixed signs are noncanonical.
                    if amount != 0
                        && span.signum() != 0
                        && amount.signum() != i64::from(span.signum())
                    {
                        return Err(E::Invalid("duration sign"));
                    }
                    span = match unit {
                        0 => span.try_years(amount),
                        1 => span.try_months(amount),
                        2 => span.try_weeks(amount),
                        3 => span.try_days(amount),
                        4 => span.try_hours(amount),
                        5 => span.try_minutes(amount),
                        6 => span.try_seconds(amount),
                        7 => span.try_milliseconds(amount),
                        8 => span.try_microseconds(amount),
                        _ => span.try_nanoseconds(amount),
                    }
                    .map_err(|_| E::Semantic)?;
                }
                Value::Duration(Box::new(span))
            }
            _ => return Err(E::Unsupported("value tag")),
        })
    }
}

enum Container {
    List {
        remaining: usize,
        values: Vec<Value>,
    },
    Record {
        remaining: usize,
        fields: Vec<(crate::DbString, Value)>,
        key: Option<crate::DbString>,
        names: std::collections::BTreeSet<crate::DbString>,
    },
}

fn parse_temporal<T: std::str::FromStr + std::fmt::Display>(text: &str) -> CodecResult<T> {
    // Temporal/zone parsers never receive a payload-sized string.
    if text.len() > 512 {
        return Err(E::Limit);
    }
    let value: T = text.parse().map_err(|_| E::Semantic)?;
    if value.to_string() != text {
        return Err(E::Invalid("temporal canonical text"));
    }
    Ok(value)
}

struct BoundedTextBuffer {
    bytes: Vec<u8>,
    limit: usize,
}
impl std::io::Write for BoundedTextBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("logical JSON limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
