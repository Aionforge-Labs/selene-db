use std::collections::HashMap;

use selene_core::{DbString, LabelSet, PropertyValueType};
use selene_graph::{EdgeTypeDef, GraphTypeDef, NodeTypeDef, PropertyTypeDef};

use crate::{
    GqlType, MutationStatement, RecordType, SetItem, SourceSpan, ValueExpr,
    analyze::{
        ast::AnalyzedStatement, binding::BindingId, error::AnalysisError, types::AnalyzedType,
        write_set::WriteKind,
    },
};

pub(super) fn validate_property_values(
    graph_type: &GraphTypeDef,
    declared_in: DbString,
    declarations: &[PropertyTypeDef],
    properties: &[(DbString, ValueExpr)],
    analyzed: &AnalyzedStatement,
) -> Result<(), AnalysisError> {
    for (key, value) in properties {
        let declaration = declarations.iter().find(|decl| decl.name == *key).ok_or(
            AnalysisError::SchemaUndeclaredProperty {
                property: key.clone(),
                declared_in: declared_in.clone(),
                graph_type: graph_type.name.clone(),
                span: value.span(),
            },
        )?;
        validate_one_property_value(
            declared_in.clone(),
            declaration,
            key.clone(),
            value,
            analyzed,
        )?;
    }
    Ok(())
}

pub(super) fn validate_one_property_value(
    declared_in: DbString,
    declaration: &PropertyTypeDef,
    key: DbString,
    value: &ValueExpr,
    analyzed: &AnalyzedStatement,
) -> Result<(), AnalysisError> {
    let Some(id) = analyzed.expr_ids.get(value) else {
        return Ok(());
    };
    match analyzed.expr_types.get(id) {
        AnalyzedType::Dynamic => Ok(()),
        AnalyzedType::Resolved(GqlType::Null) if !declaration.required => Ok(()),
        AnalyzedType::Resolved(GqlType::Null) => {
            Err(AnalysisError::SchemaRequiredPropertyMissing {
                property: key,
                declared_in,
                span: value.span(),
            })
        }
        AnalyzedType::Resolved(found)
            if property_type_compatible(declaration.value_type, found) =>
        {
            Ok(())
        }
        AnalyzedType::Resolved(found) => Err(AnalysisError::SchemaPropertyTypeMismatch {
            property: key,
            declared_in,
            expected: declaration.value_type,
            found: found.clone(),
            span: value.span(),
        }),
    }
}

pub(super) struct RequiredPropertyCheck<'a> {
    pub(super) declared_in: DbString,
    pub(super) declarations: &'a [PropertyTypeDef],
    pub(super) properties: &'a [(DbString, ValueExpr)],
    pub(super) stmt_index: usize,
    pub(super) binding: Option<BindingId>,
    pub(super) span: SourceSpan,
    pub(super) analyzed: &'a AnalyzedStatement,
}

pub(super) fn validate_required_properties(
    check: RequiredPropertyCheck<'_>,
) -> Result<(), AnalysisError> {
    for declaration in check.declarations.iter().filter(|decl| decl.required) {
        if declaration.default.is_some() {
            continue;
        }
        if check
            .properties
            .iter()
            .any(|(key, _)| *key == declaration.name)
        {
            continue;
        }
        if check.binding.is_some_and(|binding| {
            required_property_supplied(
                check.stmt_index,
                binding,
                declaration.name.clone(),
                check.analyzed,
            )
        }) {
            continue;
        }
        return Err(AnalysisError::SchemaRequiredPropertyMissing {
            property: declaration.name.clone(),
            declared_in: check.declared_in,
            span: check.span,
        });
    }
    Ok(())
}

fn required_property_supplied(
    stmt_index: usize,
    binding: BindingId,
    property: DbString,
    analyzed: &AnalyzedStatement,
) -> bool {
    analyzed.write_set.as_ref().is_some_and(|write_set| {
        write_set.entries.iter().any(|entry| {
            entry.statement_index > stmt_index
                && matches!(
                    &entry.kind,
                    WriteKind::SetProperty { target, key, .. }
                        if *target == binding && *key == property
                )
        })
    })
}

/// Index every `SET` value expression in the statement by its source span.
///
/// Built once per `schema::validate` call so the per-`SetProperty`-entry value
/// lookup is O(1) rather than re-scanning every `SET` item per write entry
/// (which was O(W×S) — one rescan of all S set values per W write entries).
pub(super) fn set_value_index(analyzed: &AnalyzedStatement) -> HashMap<SourceSpan, &ValueExpr> {
    let mut index = HashMap::new();
    let crate::Statement::Mutate(pipeline) = analyzed.source() else {
        return index;
    };
    for statement in &pipeline.statements {
        let MutationStatement::Set(items) = statement else {
            continue;
        };
        for item in items {
            match item {
                SetItem::Property { value, .. } => {
                    index.entry(value.span()).or_insert(value);
                }
                SetItem::PropertyMerge { properties, .. } => {
                    for (_, value) in properties {
                        index.entry(value.span()).or_insert(value);
                    }
                }
                _ => {}
            }
        }
    }
    index
}

pub(super) enum PropertyAgreement<'a> {
    Declared(&'a PropertyTypeDef, DbString),
    Undeclared(DbString),
    Disagree,
}

pub(super) fn property_agreement<'a>(
    types: &[&'a NodeTypeDef],
    key: DbString,
) -> PropertyAgreement<'a> {
    let Some(first_type) = types.first() else {
        return PropertyAgreement::Disagree;
    };
    let mut agreed: Option<(&PropertyTypeDef, DbString)> = None;
    let mut saw_undeclared = false;
    for node_type in types {
        match node_type.properties.iter().find(|decl| decl.name == key) {
            Some(_) if saw_undeclared => return PropertyAgreement::Disagree,
            Some(decl) => {
                if let Some((prior, _)) = agreed
                    && (prior.value_type != decl.value_type || prior.required != decl.required)
                {
                    return PropertyAgreement::Disagree;
                }
                agreed = Some((decl, node_type.name.clone()));
            }
            None if agreed.is_some() => return PropertyAgreement::Disagree,
            None => saw_undeclared = true,
        }
    }
    agreed
        .map(|(decl, name)| PropertyAgreement::Declared(decl, name))
        .unwrap_or(PropertyAgreement::Undeclared(first_type.name.clone()))
}

pub(super) fn validate_declared_property(
    edge_type: &EdgeTypeDef,
    key: DbString,
    graph_type: DbString,
    span: SourceSpan,
) -> Result<&PropertyTypeDef, AnalysisError> {
    edge_type
        .properties
        .iter()
        .find(|decl| decl.name == key)
        .ok_or(AnalysisError::SchemaUndeclaredProperty {
            property: key,
            declared_in: edge_type.name.clone(),
            graph_type,
            span,
        })
}

pub(super) fn undeclared_property(
    property: DbString,
    declared_in: DbString,
    graph_type: DbString,
    span: SourceSpan,
) -> Result<(), AnalysisError> {
    Err(AnalysisError::SchemaUndeclaredProperty {
        property,
        declared_in,
        graph_type,
        span,
    })
}

pub(super) fn validate_node_label_transition(
    types: Vec<&NodeTypeDef>,
    graph_type: &GraphTypeDef,
    span: SourceSpan,
    mutate: impl Fn(&mut LabelSet),
) -> Result<(), AnalysisError> {
    let mut first_invalid = None;
    for node_type in &types {
        let mut labels = node_type.key_labels.clone();
        mutate(&mut labels);
        if graph_type.find_node_type(&labels).is_none() {
            first_invalid.get_or_insert(labels);
        }
    }
    if let Some(labels) = first_invalid {
        return Err(AnalysisError::SchemaUnknownNodeType {
            labels,
            graph_type: graph_type.name.clone(),
            span,
        });
    }
    Ok(())
}

pub(super) fn property_type_compatible(declared: PropertyValueType, found: &GqlType) -> bool {
    use GqlType as G;
    use PropertyValueType as P;
    let found = found.strip_not_null();
    matches!(
        (declared, found),
        (P::Bool, G::Boolean)
            | (
                P::Int,
                G::Integer | G::Int8 | G::Int16 | G::Int32 | G::Int64 | G::SmallInt | G::BigInt
            )
            | (
                P::Uint,
                G::Uint8 | G::Uint16 | G::Uint32 | G::Uint64 | G::USmallInt | G::Uint | G::UBigInt
            )
            | (P::Int128, G::Int128)
            | (P::Uint128, G::Uint128)
            | (P::Float, G::Float | G::Float64 | G::Double)
            | (P::Float32, G::Float32 | G::Real)
            | (
                P::Decimal,
                G::Integer
                    | G::Int8
                    | G::Int16
                    | G::Int32
                    | G::Int64
                    | G::SmallInt
                    | G::BigInt
                    | G::Uint8
                    | G::Uint16
                    | G::Uint32
                    | G::Uint64
                    | G::USmallInt
                    | G::Uint
                    | G::UBigInt
                    | G::Int128
                    | G::Uint128
                    | G::Float
                    | G::Float64
                    | G::Double
                    | G::Float32
                    | G::Real
                    | G::Decimal
                    | G::DecimalExact(_),
            )
            | (P::String, G::String | G::CharacterString(_))
            | (P::Uuid, G::Uuid)
            | (P::Bytes, G::Bytes | G::ByteString(_))
            | (P::Json, G::Json)
            | (P::List, G::List(_) | G::BoundedList { .. })
            // Every record property declaration — open `RECORD` and closed `RECORD{..}`
            // alike — lowers to `P::RecordTyped` (catalog/property.rs), while a `RECORD{..}`
            // constructor always analyzes to the OPEN record type (bind/expr.rs). Accept the
            // open literal at this coarse type-compat gate; closed-record field-name-set +
            // per-field conformance is enforced at commit time by `RecordFieldTypes::matches`
            // → G2000 (ISO 39075:2024 §4.15.4). Without the open arm, a record write into any
            // RECORD-typed property in a GG02 graph is rejected at analysis even though the
            // runtime accepts and structurally validates it.
            | (P::Record, G::Record(RecordType::Open))
            | (P::RecordTyped, G::Record(_))
            | (P::ZonedDateTime, G::ZonedDateTime)
            | (P::LocalDateTime, G::LocalDateTime)
            | (P::Date, G::Date)
            | (P::ZonedTime, G::ZonedTime)
            | (P::LocalTime, G::LocalTime)
            | (P::Duration, G::Duration | G::DurationYearToMonth | G::DurationDayToSecond)
            | (P::DurationYearToMonth, G::Duration | G::DurationYearToMonth)
            | (P::DurationDayToSecond, G::Duration | G::DurationDayToSecond)
            | (P::Vector, G::Vector)
            | (P::Null, G::Null)
            | (P::Path, G::Path)
            | (P::NodeRef, G::NodeRef)
            | (P::EdgeRef, G::EdgeRef)
            | (P::GraphRef, G::GraphRef)
            | (P::TableRef, G::TableRef(_))
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use selene_core::{BindingTableId, EdgeId, GraphId, JsonValue, NodeId, Value, db_string};

    use super::*;

    #[test]
    fn property_type_compatibility_is_storage_tag_exact() {
        assert!(property_type_compatible(
            PropertyValueType::Int,
            &GqlType::Integer
        ));
        assert!(property_type_compatible(
            PropertyValueType::Int,
            &GqlType::BigInt
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Int,
            &GqlType::Uint64
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Int,
            &GqlType::Int128
        ));

        assert!(property_type_compatible(
            PropertyValueType::Uint,
            &GqlType::Uint32
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Uint,
            &GqlType::Integer
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Uint,
            &GqlType::Uint128
        ));

        assert!(property_type_compatible(
            PropertyValueType::Float,
            &GqlType::Float64
        ));
        assert!(property_type_compatible(
            PropertyValueType::Float,
            &GqlType::Double
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Float,
            &GqlType::Float32
        ));
        assert!(property_type_compatible(
            PropertyValueType::Float32,
            &GqlType::Float32
        ));
        assert!(property_type_compatible(
            PropertyValueType::Float32,
            &GqlType::Real
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Float32,
            &GqlType::Float
        ));

        assert!(property_type_compatible(
            PropertyValueType::Bytes,
            &GqlType::Bytes
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Bytes,
            &GqlType::String
        ));
        assert!(property_type_compatible(
            PropertyValueType::Json,
            &GqlType::Json
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Json,
            &GqlType::String
        ));

        assert!(property_type_compatible(
            PropertyValueType::List,
            &GqlType::List(Box::new(GqlType::Integer))
        ));
        assert!(property_type_compatible(
            PropertyValueType::Record,
            &GqlType::Record(RecordType::Open)
        ));
        assert!(property_type_compatible(
            PropertyValueType::RecordTyped,
            &GqlType::Record(RecordType::Closed(Vec::new()))
        ));
        // A `RECORD{..}` constructor always analyzes to the OPEN record type, so a
        // `RecordTyped`-declared property (the tag every record declaration lowers to)
        // must accept the open literal at this coarse gate; closed-field conformance is
        // deferred to the runtime (`RecordFieldTypes::matches` → G2000). Regression pin
        // for the analyzer/runtime divergence that made typed-RECORD writes unexecutable.
        assert!(property_type_compatible(
            PropertyValueType::RecordTyped,
            &GqlType::Record(RecordType::Open)
        ));
        // The gate stays closed for a genuine mismatch: a record literal does not satisfy
        // a scalar declaration.
        assert!(!property_type_compatible(
            PropertyValueType::Int,
            &GqlType::Record(RecordType::Open)
        ));
        assert!(property_type_compatible(
            PropertyValueType::Null,
            &GqlType::Null
        ));
        assert!(property_type_compatible(
            PropertyValueType::Uuid,
            &GqlType::Uuid
        ));
        assert!(!property_type_compatible(
            PropertyValueType::Uuid,
            &GqlType::String
        ));
    }

    #[test]
    fn property_type_compatibility_matches_runtime_storage_samples() {
        let zoned = "2026-05-07T12:34:56-04:00[America/New_York]";
        let samples = [
            (PropertyValueType::Bool, GqlType::Boolean, Value::Bool(true)),
            (PropertyValueType::Int, GqlType::Integer, Value::Int(-1)),
            (PropertyValueType::Uint, GqlType::Uint64, Value::Uint(1)),
            (
                PropertyValueType::Int128,
                GqlType::Int128,
                Value::Int128(-1),
            ),
            (
                PropertyValueType::Uint128,
                GqlType::Uint128,
                Value::Uint128(1),
            ),
            (
                PropertyValueType::Float,
                GqlType::Float64,
                Value::Float(1.25),
            ),
            (
                PropertyValueType::Float32,
                GqlType::Float32,
                Value::Float32(1.5),
            ),
            (
                PropertyValueType::Decimal,
                GqlType::Decimal,
                Value::Decimal("123.45".parse().unwrap()),
            ),
            (
                PropertyValueType::String,
                GqlType::String,
                Value::String(db_string("schema.type.string").unwrap()),
            ),
            (
                PropertyValueType::Uuid,
                GqlType::Uuid,
                Value::Uuid(uuid::Uuid::nil()),
            ),
            (
                PropertyValueType::Bytes,
                GqlType::Bytes,
                Value::Bytes(Arc::from([1_u8, 2, 3])),
            ),
            (
                PropertyValueType::Json,
                GqlType::Json,
                Value::Json(JsonValue::new(serde_json::json!({"a": 1})).unwrap()),
            ),
            (
                PropertyValueType::NodeRef,
                GqlType::NodeRef,
                Value::NodeRef(NodeId::new(1)),
            ),
            (
                PropertyValueType::EdgeRef,
                GqlType::EdgeRef,
                Value::EdgeRef(EdgeId::new(1)),
            ),
            (
                PropertyValueType::GraphRef,
                GqlType::GraphRef,
                Value::GraphRef(GraphId::new(1)),
            ),
            (
                PropertyValueType::TableRef,
                GqlType::TableRef(crate::BindingTableType::Any),
                Value::TableRef(BindingTableId::new(1)),
            ),
            (
                PropertyValueType::ZonedDateTime,
                GqlType::ZonedDateTime,
                Value::ZonedDateTime(Box::new(zoned.parse().unwrap())),
            ),
            (
                PropertyValueType::LocalDateTime,
                GqlType::LocalDateTime,
                Value::LocalDateTime("2026-05-07T12:34:56".parse().unwrap()),
            ),
            (
                PropertyValueType::Date,
                GqlType::Date,
                Value::Date("2026-05-07".parse().unwrap()),
            ),
            (
                PropertyValueType::ZonedTime,
                GqlType::ZonedTime,
                Value::ZonedTime(Box::new(zoned.parse().unwrap())),
            ),
            (
                PropertyValueType::LocalTime,
                GqlType::LocalTime,
                Value::LocalTime("12:34:56".parse().unwrap()),
            ),
            (
                PropertyValueType::Duration,
                GqlType::Duration,
                Value::Duration(Box::new("PT1H2S".parse().unwrap())),
            ),
            (
                PropertyValueType::DurationYearToMonth,
                GqlType::Duration,
                Value::Duration(Box::new("P1Y2M".parse().unwrap())),
            ),
            (
                PropertyValueType::DurationDayToSecond,
                GqlType::DurationDayToSecond,
                Value::Duration(Box::new("PT1H2S".parse().unwrap())),
            ),
            (PropertyValueType::Null, GqlType::Null, Value::Null),
        ];

        for (declared, gql_type, value) in samples {
            assert!(declared.matches(&value));
            assert!(property_type_compatible(declared, &gql_type));
        }
    }
}
