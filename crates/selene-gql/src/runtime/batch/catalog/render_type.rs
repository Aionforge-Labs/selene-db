//! Catalog-facing GQL type rendering.

use crate::{BindingTableType, GqlType, RecordType, ast::format_ident::fmt_ident};

pub(super) fn render_gql_type(ty: &GqlType) -> String {
    match ty {
        GqlType::Any => "ANY".to_owned(),
        GqlType::AnyProperty => "ANY PROPERTY VALUE".to_owned(),
        GqlType::ClosedDynamicUnion(components) => components
            .iter()
            .map(render_gql_type)
            .collect::<Vec<_>>()
            .join(" | "),
        GqlType::String => "STRING".to_owned(),
        GqlType::CharacterString(character) if character.min_len == 0 => {
            format!("STRING({})", character.max_len)
        }
        GqlType::CharacterString(character) => {
            format!("STRING({}, {})", character.min_len, character.max_len)
        }
        GqlType::Boolean => "BOOLEAN".to_owned(),
        GqlType::Integer => "INTEGER".to_owned(),
        GqlType::Float => "FLOAT".to_owned(),
        GqlType::Int8 => "INT8".to_owned(),
        GqlType::Int16 => "INT16".to_owned(),
        GqlType::Int32 => "INT32".to_owned(),
        GqlType::Int64 => "INT64".to_owned(),
        GqlType::Int128 => "INT128".to_owned(),
        GqlType::Uint8 => "UINT8".to_owned(),
        GqlType::Uint16 => "UINT16".to_owned(),
        GqlType::Uint32 => "UINT32".to_owned(),
        GqlType::Uint64 => "UINT64".to_owned(),
        GqlType::Uint128 => "UINT128".to_owned(),
        GqlType::USmallInt => "USMALLINT".to_owned(),
        GqlType::Uint => "UINT".to_owned(),
        GqlType::UBigInt => "UBIGINT".to_owned(),
        GqlType::SmallInt => "SMALLINT".to_owned(),
        GqlType::BigInt => "BIGINT".to_owned(),
        GqlType::Decimal => "DECIMAL".to_owned(),
        GqlType::DecimalExact(decimal) if decimal.scale == 0 => {
            format!("DECIMAL({})", decimal.precision)
        }
        GqlType::DecimalExact(decimal) => {
            format!("DECIMAL({}, {})", decimal.precision, decimal.scale)
        }
        GqlType::Float32 => "FLOAT32".to_owned(),
        GqlType::Float64 => "FLOAT64".to_owned(),
        GqlType::Real => "REAL".to_owned(),
        GqlType::Double => "DOUBLE".to_owned(),
        GqlType::Bytes => "BYTES".to_owned(),
        GqlType::ByteString(bytes) if bytes.min_len == 0 => {
            format!("BYTES({})", bytes.max_len)
        }
        GqlType::ByteString(bytes) => {
            format!("BYTES({}, {})", bytes.min_len, bytes.max_len)
        }
        GqlType::Uuid => "UUID".to_owned(),
        GqlType::Json => "JSON".to_owned(),
        GqlType::ZonedDateTime => "ZONED DATETIME".to_owned(),
        GqlType::LocalDateTime => "LOCAL DATETIME".to_owned(),
        GqlType::Date => "DATE".to_owned(),
        GqlType::ZonedTime => "ZONED TIME".to_owned(),
        GqlType::LocalTime => "LOCAL TIME".to_owned(),
        GqlType::Duration => "DURATION".to_owned(),
        GqlType::DurationYearToMonth => "DURATION (YEAR TO MONTH)".to_owned(),
        GqlType::DurationDayToSecond => "DURATION (DAY TO SECOND)".to_owned(),
        GqlType::Vector => "VECTOR".to_owned(),
        GqlType::Record(RecordType::Open) => "RECORD".to_owned(),
        GqlType::Record(RecordType::Closed(fields)) => {
            if fields.is_empty() {
                return "RECORD {}".to_owned();
            }
            let rendered = fields
                .iter()
                .map(|(name, ty)| format!("{} :: {}", fmt_ident(name.clone()), render_gql_type(ty)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("RECORD {{ {rendered} }}")
        }
        GqlType::List(inner) => format!("LIST<{}>", render_gql_type(inner)),
        GqlType::BoundedList {
            element_type,
            max_len,
        } => {
            format!("LIST<{}>[{}]", render_gql_type(element_type), max_len)
        }
        GqlType::NotNull(inner) => format!("{} NOT NULL", render_gql_type(inner)),
        GqlType::Path => "PATH".to_owned(),
        GqlType::GraphRef => "GRAPH".to_owned(),
        GqlType::NodeRef => "NODE".to_owned(),
        GqlType::EdgeRef => "EDGE".to_owned(),
        GqlType::TableRef(BindingTableType::Any) => "TABLE".to_owned(),
        GqlType::TableRef(BindingTableType::Closed(fields)) => {
            if fields.is_empty() {
                return "TABLE {}".to_owned();
            }
            let rendered = fields
                .iter()
                .map(|(name, ty)| format!("{} :: {}", fmt_ident(name.clone()), render_gql_type(ty)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("TABLE {{ {rendered} }}")
        }
        GqlType::Null => "NULL".to_owned(),
        GqlType::Nothing => "NOTHING".to_owned(),
    }
}
