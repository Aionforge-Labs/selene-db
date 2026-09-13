//! `bounds=…` rendering for indexed-scan access summaries.
//!
//! Produces the new EXPLAIN detail surface introduced by BRIEF-154 §C.1 #7:
//! literals render with their kind tag (`STRING 'foo'`), parameter slots
//! render as `$name` (matches the GQL source form). Lives in a sibling
//! module so `plan/optimize/summary.rs` stays under the 700 LOC file cap.
//!
//! Output is intended for human EXPLAIN consumption and the snapshot-harness
//! goldens; it is NOT a stable wire format.
//!
//! Examples:
//! - `TypedIndexRange[bounds=Equality(STRING 'foo')]`
//! - `TypedIndexRange[bounds=Range([INTEGER 10 .. $upper))]`
//! - `BitmapUnion[bounds=Keys [STRING 'alice', $b]]`
//! - `BitmapUnion[bounds=Keys [$names...]]`
//! - `CompositeLookup[bounds=Composite [tenant=STRING 't1', kind=$k]]`

use std::fmt::Write as _;

use selene_core::DbString;

use crate::{IndexKey, Literal, ScanAccess, TypedIndexBounds};

/// Render the bounds-detail string for a [`ScanAccess`], or `None` for access
/// variants that do not carry parameter-shaped probe keys (Linear / LabelIndex).
pub(super) fn bounds_detail_for_access(access: &ScanAccess) -> Option<String> {
    match access {
        ScanAccess::ExpressionLookup {
            handle,
            expression,
            value,
        } => Some(format!(
            "Expression({}, {expression:?}, {value:?})",
            handle.raw()
        )),
        ScanAccess::TypedIndexRange { bounds, .. } => Some(render_bounds(bounds)),
        ScanAccess::BitmapUnion { keys, .. } => Some(render_bitmap_union_keys(keys)),
        ScanAccess::CompositeLookup { keys, .. } => Some(render_composite_keys(keys)),
        ScanAccess::Linear | ScanAccess::LabelIndex { .. } => None,
    }
}

fn render_bounds(bounds: &TypedIndexBounds) -> String {
    match bounds {
        TypedIndexBounds::Equality(key) => format!("Equality({})", render_index_key(key)),
        TypedIndexBounds::GreaterThan(key) => format!("GreaterThan({})", render_index_key(key)),
        TypedIndexBounds::GreaterEqual(key) => format!("GreaterEqual({})", render_index_key(key)),
        TypedIndexBounds::LessThan(key) => format!("LessThan({})", render_index_key(key)),
        TypedIndexBounds::LessEqual(key) => format!("LessEqual({})", render_index_key(key)),
        TypedIndexBounds::Range {
            lo,
            lo_inclusive,
            hi,
            hi_inclusive,
        } => format!(
            "Range({}{} .. {}{})",
            if *lo_inclusive { "[" } else { "(" },
            render_index_key(lo),
            render_index_key(hi),
            if *hi_inclusive { "]" } else { ")" },
        ),
    }
}

fn render_bitmap_union_keys(keys: &[IndexKey]) -> String {
    format!(
        "Keys [{}]",
        keys.iter()
            .map(render_index_key)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn render_composite_keys(keys: &[(DbString, IndexKey)]) -> String {
    format!(
        "Composite [{}]",
        keys.iter()
            .map(|(property, key)| format!("{}={}", property.as_str(), render_index_key(key)))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn render_index_key(key: &IndexKey) -> String {
    match key {
        IndexKey::Literal(literal) => render_literal(literal),
        IndexKey::Parameter { name, .. } => format!("${}", name.as_str()),
        IndexKey::ParameterList { name, .. } => format!("${}...", name.as_str()),
    }
}

fn render_literal(literal: &Literal) -> String {
    match literal {
        Literal::Bool(value, _) => format!("BOOLEAN {value}"),
        Literal::Integer(value, _) | Literal::RadixInteger(value, _, _) => {
            format!("INTEGER {value}")
        }
        Literal::Decimal(value, _, _) => format!("DECIMAL {value}"),
        Literal::Float(value, _, _) => format!("FLOAT {value}"),
        Literal::String(value, _, _) => format!("STRING '{}'", value.as_str()),
        Literal::Bytes(value, _) => format!("BYTES X'{}'", hex_bytes(value)),
        Literal::Uuid(value, _, _) => format!("UUID '{value}'"),
        Literal::ZonedDateTime(value, _, _) => {
            format!("ZONED DATETIME '{}'", format_zoned_datetime(value))
        }
        Literal::LocalDateTime(value, _, _) => format!("LOCAL DATETIME '{value}'"),
        Literal::Date(value, _, _) => format!("DATE '{value}'"),
        Literal::ZonedTime(value, _, _) => format!("ZONED TIME '{}'", format_zoned_time(value)),
        Literal::LocalTime(value, _, _) => format!("LOCAL TIME '{value}'"),
        Literal::Duration(value, _, _) => format!("DURATION '{value}'"),
        Literal::Null(_) => "NULL".to_owned(),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02X}");
    }
    out
}

fn format_zoned_datetime(value: &jiff::Zoned) -> String {
    format!("{}{}", value.datetime(), value.offset())
}

fn format_zoned_time(value: &jiff::Zoned) -> String {
    format!("{}{}", value.time(), value.offset())
}
