//! Ephemeral, domain-delimited UNIQUE equality keys. Not a persistence codec.

use selene_core::{Value, ValueComparisonError as E};

pub(super) fn write(value: &Value, out: &mut Vec<u8>, depth: usize) -> Result<(), E> {
    if depth > selene_core::MAX_STRUCTURAL_TYPE_DEPTH {
        return Err(E::TooDeep);
    }
    if let Some(number) = selene_core::NumericKey::of(value) {
        out.push(2);
        number.append_grouping_key(out);
        return Ok(());
    }
    match value {
        Value::Null => out.push(0),
        Value::Bool(value) => {
            out.push(1);
            out.push(u8::from(*value));
        }
        Value::String(value) => variable(9, value.as_str().as_bytes(), out),
        Value::Bytes(value) => variable(10, value, out),
        Value::List(values) => {
            out.push(11);
            length(values.len(), out);
            for value in values {
                write(value, out, depth + 1)?;
            }
        }
        Value::Record(record) => {
            let selene_core::Record::Open(fields) = record.as_ref() else {
                return Err(E::NotComparable);
            };
            out.push(12);
            length(fields.len(), out);
            let mut fields: Vec<_> = fields.iter().collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            for pair in fields.windows(2) {
                if pair[0].0 == pair[1].0 {
                    return Err(E::NotComparable);
                }
            }
            for (name, value) in fields {
                variable(0, name.as_str().as_bytes(), out);
                write(value, out, depth + 1)?;
            }
        }
        Value::Duration(value) => {
            if selene_core::duration_value_family(value).is_none() {
                return Err(E::NotComparable);
            }
            out.push(14);
            let (months, nanos) = selene_core::duration_order_key(value);
            out.extend_from_slice(&months.to_le_bytes());
            out.extend_from_slice(&nanos.to_le_bytes());
        }
        Value::ZonedDateTime(value) => {
            out.push(15);
            out.extend_from_slice(&value.timestamp().as_nanosecond().to_le_bytes());
        }
        Value::ZonedTime(value) => {
            out.push(16);
            out.extend_from_slice(&value.timestamp().as_nanosecond().to_le_bytes());
        }
        Value::Date(value) => {
            out.push(17);
            date(*value, out);
        }
        Value::LocalDateTime(value) => {
            out.push(18);
            date(value.date(), out);
            time(value.time(), out);
        }
        Value::LocalTime(value) => {
            out.push(19);
            time(*value, out);
        }
        Value::Uuid(value) => {
            out.push(20);
            out.extend_from_slice(value.as_bytes());
        }
        Value::Json(value) => variable(30, value.to_canonical_string().as_bytes(), out),
        Value::Vector(value) => {
            out.push(31);
            length(value.dimension(), out);
            for component in value.as_slice() {
                let bits = if component.is_nan() {
                    f32::NAN.to_bits()
                } else if *component == 0.0 {
                    0
                } else {
                    component.to_bits()
                };
                out.extend_from_slice(&bits.to_le_bytes());
            }
        }
        // Query-only and unsupported carriers are errors, even before domain
        // observation. Never serialize bare runtime IDs or panic on input here.
        _ => return Err(E::NotComparable),
    }
    Ok(())
}
fn variable(tag: u8, bytes: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    length(bytes.len(), out);
    out.extend_from_slice(bytes);
}
fn length(len: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(&(len as u64).to_le_bytes());
}
fn date(value: jiff::civil::Date, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.year().to_le_bytes());
    out.extend_from_slice(&value.month().to_le_bytes());
    out.extend_from_slice(&value.day().to_le_bytes());
}
fn time(value: jiff::civil::Time, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.hour().to_le_bytes());
    out.extend_from_slice(&value.minute().to_le_bytes());
    out.extend_from_slice(&value.second().to_le_bytes());
    out.extend_from_slice(&value.subsec_nanosecond().to_le_bytes());
}

#[cfg(test)]
mod tests;
