//! Store-assignment conversion for closed graph property writes.

use std::{borrow::Cow, sync::Arc};

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use selene_core::{
    ByteStringType, CharacterStringCoercionError, CharacterStringType, DbString, DecimalType,
    LabelSet, NodeId, PropertyDiff, PropertyMap, PropertyValueType, Value,
    coerce_character_string_to_type, db_string, round_decimal_to_type,
};

use crate::error::{GraphError, GraphResult, StoreAssignmentError, StoreAssignmentException};
use crate::graph_types::{
    GraphTypeDef, PropertyElementType, PropertyTypeDef, RecordFieldType, RecordFieldTypes,
};

pub(crate) fn coerce_node_properties(
    graph: &crate::SeleneGraph,
    labels: &LabelSet,
    props: &mut PropertyMap,
) -> GraphResult<()> {
    let Some(graph_type) = graph.meta.bound_type.as_deref() else {
        return Ok(());
    };
    let Some(node_type) = graph_type.find_node_type(labels) else {
        return Ok(());
    };
    coerce_property_map(&node_type.properties, props)
}

pub(crate) fn coerce_edge_properties(
    graph: &crate::SeleneGraph,
    label: DbString,
    source: NodeId,
    target: NodeId,
    directionality: selene_core::EdgeDirectionality,
    props: &mut PropertyMap,
) -> GraphResult<()> {
    let Some(edge_type) = edge_type(graph, label, source, target, directionality) else {
        return Ok(());
    };
    coerce_property_map(&edge_type.properties, props)
}

pub(crate) fn coerce_node_property_diff(
    graph: &crate::SeleneGraph,
    labels: &LabelSet,
    diff: &mut PropertyDiff,
) -> GraphResult<()> {
    let Some(graph_type) = graph.meta.bound_type.as_deref() else {
        return Ok(());
    };
    let Some(node_type) = graph_type.find_node_type(labels) else {
        return Ok(());
    };
    coerce_property_diff(&node_type.properties, diff)
}

pub(crate) fn coerce_edge_property_diff(
    graph: &crate::SeleneGraph,
    edge: selene_core::EdgeId,
    diff: &mut PropertyDiff,
) -> GraphResult<()> {
    let Some(label) = graph.edge_label(edge).cloned() else {
        return Ok(());
    };
    let Some((source, target)) = graph.edge_endpoints(edge) else {
        return Ok(());
    };
    let Some(edge_type) = edge_type(
        graph,
        label,
        source,
        target,
        graph
            .edge_directionality(edge)
            .ok_or(GraphError::EdgeNotFound { id: edge })?,
    ) else {
        return Ok(());
    };
    coerce_property_diff(&edge_type.properties, diff)
}

fn edge_type(
    graph: &crate::SeleneGraph,
    label: DbString,
    source: NodeId,
    target: NodeId,
    directionality: selene_core::EdgeDirectionality,
) -> Option<&crate::graph_types::EdgeTypeDef> {
    let graph_type = graph.meta.bound_type.as_deref()?;
    let source_type = node_type_index_for_node(graph, graph_type, source)?;
    let target_type = node_type_index_for_node(graph, graph_type, target)?;
    graph_type.find_mixed_edge_type(label, source_type, target_type, directionality)
}

fn node_type_index_for_node(
    graph: &crate::SeleneGraph,
    graph_type: &GraphTypeDef,
    node: NodeId,
) -> Option<u32> {
    let labels = graph.node_labels(node)?;
    graph_type.find_node_type_index(labels)
}

fn coerce_property_map(
    declarations: &[PropertyTypeDef],
    props: &mut PropertyMap,
) -> GraphResult<()> {
    for declaration in declarations {
        if !declaration_may_coerce(declaration) {
            continue;
        }
        let Some(mut value) = props.remove(&declaration.name) else {
            continue;
        };
        coerce_property_value(declaration, &mut value)?;
        props.set(declaration.name.clone(), value)?;
    }
    Ok(())
}

fn coerce_property_diff(
    declarations: &[PropertyTypeDef],
    diff: &mut PropertyDiff,
) -> GraphResult<()> {
    for (key, value) in &mut diff.set {
        let Some(declaration) = declarations.iter().find(|decl| decl.name == *key) else {
            continue;
        };
        if declaration_may_coerce(declaration) {
            coerce_property_value(declaration, value)?;
        }
    }
    Ok(())
}

fn declaration_may_coerce(declaration: &PropertyTypeDef) -> bool {
    match declaration.value_type {
        PropertyValueType::Decimal => true,
        PropertyValueType::String => declaration.character_string_type.is_some(),
        PropertyValueType::Bytes => declaration.byte_string_type.is_some(),
        PropertyValueType::List => declaration.list_element_type.is_some(),
        PropertyValueType::Record | PropertyValueType::RecordTyped => {
            declaration.record_field_types.is_some()
        }
        _ => false,
    }
}

fn coerce_property_value(declaration: &PropertyTypeDef, value: &mut Value) -> GraphResult<()> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    match declaration.value_type {
        PropertyValueType::Decimal => {
            coerce_decimal_property(value, declaration.decimal_type, declaration.name.clone())
        }
        PropertyValueType::String => match declaration.character_string_type {
            Some(target) => coerce_character_string(value, target, declaration.name.clone()),
            None => Ok(()),
        },
        PropertyValueType::Bytes => match declaration.byte_string_type {
            Some(target) => coerce_byte_string(value, target, declaration.name.clone()),
            None => Ok(()),
        },
        PropertyValueType::List => match (&declaration.list_element_type, value) {
            (Some(element_type), Value::List(values)) => {
                for value in values {
                    coerce_element_value(element_type, value, &declaration.name)?;
                }
                Ok(())
            }
            _ => Ok(()),
        },
        PropertyValueType::Record | PropertyValueType::RecordTyped => {
            match &declaration.record_field_types {
                Some(fields) if matches!(value, Value::Record(_) | Value::RecordTyped(_)) => {
                    coerce_record_value(fields, value, &declaration.name)
                }
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

fn coerce_element_value(
    element_type: &PropertyElementType,
    value: &mut Value,
    property: &DbString,
) -> GraphResult<()> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    match element_type {
        PropertyElementType::NotNull(inner) => coerce_element_value(inner, value, property),
        PropertyElementType::Scalar(PropertyValueType::Decimal) => {
            coerce_decimal_property(value, None, property.clone())
        }
        PropertyElementType::Scalar(_) => Ok(()),
        PropertyElementType::CharacterString(target) => {
            coerce_character_string(value, *target, property.clone())
        }
        PropertyElementType::Decimal(target) => {
            coerce_decimal_property(value, Some(*target), property.clone())
        }
        PropertyElementType::ByteString(target) => {
            coerce_byte_string(value, *target, property.clone())
        }
        PropertyElementType::List(inner) => match value {
            Value::List(values) => {
                for value in values {
                    coerce_element_value(inner, value, property)?;
                }
                Ok(())
            }
            _ => Ok(()),
        },
    }
}

fn coerce_record_field_value(
    field_type: &RecordFieldType,
    value: &mut Value,
    property: &DbString,
) -> GraphResult<()> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    match field_type {
        RecordFieldType::NotNull(inner) => coerce_record_field_value(inner, value, property),
        RecordFieldType::Scalar(PropertyValueType::Decimal) => {
            coerce_decimal_property(value, None, property.clone())
        }
        RecordFieldType::Scalar(_) | RecordFieldType::OpenRecord => Ok(()),
        RecordFieldType::CharacterString(target) => {
            coerce_character_string(value, *target, property.clone())
        }
        RecordFieldType::Decimal(target) => {
            coerce_decimal_property(value, Some(*target), property.clone())
        }
        RecordFieldType::ByteString(target) => coerce_byte_string(value, *target, property.clone()),
        RecordFieldType::List(inner) => match value {
            Value::List(values) => {
                for value in values {
                    coerce_record_field_value(inner, value, property)?;
                }
                Ok(())
            }
            _ => Ok(()),
        },
        RecordFieldType::Record(fields) => coerce_record_value(fields, value, property),
    }
}

fn coerce_record_value(
    fields: &RecordFieldTypes,
    value: &mut Value,
    property: &DbString,
) -> GraphResult<()> {
    match value {
        Value::Record(record) => match record.as_mut() {
            selene_core::Record::Open(values) => {
                for (name, field_value) in values {
                    let Some(field) = fields.0.iter().find(|field| field.name == *name) else {
                        continue;
                    };
                    coerce_record_field_value(&field.field_type, field_value, property)?;
                }
                Ok(())
            }
            _ => Ok(()),
        },
        Value::RecordTyped(record) => {
            for (field, slot) in fields.0.iter().zip(record.values.iter_mut()) {
                let Some(value) = slot else {
                    continue;
                };
                coerce_record_field_value(&field.field_type, value, property)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn coerce_character_string(
    value: &mut Value,
    target: CharacterStringType,
    property: DbString,
) -> GraphResult<()> {
    let Value::String(value) = value else {
        return Ok(());
    };
    // The shared core coercion keeps store assignment on the same IV023
    // space-only truncation policy as character CAST and DEFAULT descriptor
    // coercion.
    let coerced = coerce_character_string_to_type(value.as_str(), target).map_err(|err| {
        let detail = match err {
            CharacterStringCoercionError::SourceLengthOverflow => {
                "character string source length exceeds supported range"
            }
            CharacterStringCoercionError::TargetMinOverflow => {
                "character string target minimum length exceeds supported range"
            }
            CharacterStringCoercionError::TargetMaxOverflow => {
                "character string target maximum length exceeds supported range"
            }
            CharacterStringCoercionError::NonSpaceTruncation => {
                "character string assignment would truncate non-space trailing characters"
            }
        };
        assignment_error(
            property,
            StoreAssignmentException::StringDataRightTruncation,
            detail,
        )
    })?;
    match coerced {
        Cow::Borrowed(coerced) if coerced == value.as_str() => Ok(()),
        Cow::Borrowed(coerced) => {
            *value = db_string(coerced).map_err(GraphError::Core)?;
            Ok(())
        }
        Cow::Owned(coerced) => {
            *value = DbString::from_string(coerced).map_err(GraphError::Core)?;
            Ok(())
        }
    }
}

fn coerce_byte_string(
    value: &mut Value,
    target: ByteStringType,
    property: DbString,
) -> GraphResult<()> {
    let Value::Bytes(value) = value else {
        return Ok(());
    };
    let len = u64::try_from(value.len()).map_err(|_| {
        assignment_error(
            property.clone(),
            StoreAssignmentException::StringDataRightTruncation,
            "byte string source length exceeds supported range",
        )
    })?;
    if target.matches_len(value.len()) {
        return Ok(());
    }
    if len > target.max_len {
        let max_len = usize::try_from(target.max_len).map_err(|_| {
            assignment_error(
                property.clone(),
                StoreAssignmentException::StringDataRightTruncation,
                "byte string target maximum length exceeds supported range",
            )
        })?;
        if value[max_len..].iter().any(|byte| *byte != 0) {
            return Err(assignment_error(
                property,
                StoreAssignmentException::StringDataRightTruncation,
                "byte string assignment would truncate non-zero trailing bytes",
            ));
        }
        let mut bytes = value.as_ref().to_vec();
        bytes.truncate(max_len);
        *value = Arc::from(bytes);
    } else if len < target.min_len {
        let min_len = usize::try_from(target.min_len).map_err(|_| {
            assignment_error(
                property.clone(),
                StoreAssignmentException::StringDataRightTruncation,
                "byte string target minimum length exceeds supported range",
            )
        })?;
        let mut bytes = value.as_ref().to_vec();
        bytes.resize(min_len, 0);
        *value = Arc::from(bytes);
    }
    Ok(())
}

fn coerce_decimal_property(
    value: &mut Value,
    target: Option<DecimalType>,
    property: DbString,
) -> GraphResult<()> {
    let decimal = numeric_to_decimal(value, property.clone())?;
    let coerced = match target {
        Some(target) => round_decimal_to_type(decimal, target).ok_or_else(|| {
            assignment_error(
                property,
                StoreAssignmentException::NumericValueOutOfRange,
                "numeric assignment cannot be represented by declared DECIMAL precision/scale",
            )
        })?,
        None => decimal,
    };
    if !matches!(value, Value::Decimal(current) if *current == coerced) {
        *value = Value::Decimal(coerced);
    }
    Ok(())
}

fn numeric_to_decimal(value: &Value, property: DbString) -> GraphResult<Decimal> {
    match value {
        Value::Int(value) => Ok(Decimal::from(*value)),
        Value::Uint(value) => Ok(Decimal::from(*value)),
        Value::Int128(value) => Decimal::try_from_i128_with_scale(*value, 0).map_err(|_| {
            assignment_error(
                property,
                StoreAssignmentException::NumericValueOutOfRange,
                "INT128 assignment exceeds DECIMAL range",
            )
        }),
        Value::Uint128(value) => Decimal::from_u128(*value).ok_or_else(|| {
            assignment_error(
                property,
                StoreAssignmentException::NumericValueOutOfRange,
                "UINT128 assignment exceeds DECIMAL range",
            )
        }),
        Value::Float(value) => Decimal::from_f64(*value).ok_or_else(|| {
            assignment_error(
                property,
                StoreAssignmentException::NumericValueOutOfRange,
                "FLOAT assignment has no DECIMAL image",
            )
        }),
        Value::Float32(value) => Decimal::from_f32(*value).ok_or_else(|| {
            assignment_error(
                property,
                StoreAssignmentException::NumericValueOutOfRange,
                "FLOAT32 assignment has no DECIMAL image",
            )
        }),
        Value::Decimal(value) => Ok(*value),
        value => Err(assignment_error(
            property,
            StoreAssignmentException::NumericValueOutOfRange,
            format!("{} is not assignable to DECIMAL", value.variant_name()),
        )),
    }
}

fn assignment_error(
    property: DbString,
    exception: StoreAssignmentException,
    reason: impl Into<String>,
) -> GraphError {
    GraphError::StoreAssignment(Box::new(StoreAssignmentError {
        property,
        exception,
        reason: reason.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(
        name: DbString,
        value_type: PropertyValueType,
        character_string_type: Option<CharacterStringType>,
        byte_string_type: Option<ByteStringType>,
    ) -> PropertyTypeDef {
        PropertyTypeDef {
            name,
            value_type,
            list_element_type: None,
            required: false,
            default: None,
            immutable: false,
            unique: false,
            decimal_type: None,
            character_string_type,
            byte_string_type,
            record_field_types: None,
        }
    }

    fn descriptor_declarations() -> (DbString, DbString, Vec<PropertyTypeDef>) {
        let text = db_string("text").expect("test property name is valid");
        let bytes = db_string("bytes").expect("test property name is valid");
        let declarations = vec![
            declaration(
                text.clone(),
                PropertyValueType::String,
                Some(CharacterStringType::new(1, 64).expect("test descriptor is valid")),
                None,
            ),
            declaration(
                bytes.clone(),
                PropertyValueType::Bytes,
                None,
                Some(ByteStringType::new(1, 64).expect("test descriptor is valid")),
            ),
        ];
        (text, bytes, declarations)
    }

    #[test]
    fn property_map_in_bounds_descriptor_values_reuse_storage() {
        let (text, bytes, declarations) = descriptor_declarations();
        let original_text = db_string("already-valid").expect("test value is valid");
        let original_bytes = Arc::<[u8]>::from([1_u8, 2, 3, 4]);
        let mut props = PropertyMap::compact(
            [text.clone(), bytes.clone()],
            [
                Some(Value::String(original_text.clone())),
                Some(Value::Bytes(Arc::clone(&original_bytes))),
            ],
        )
        .expect("test property map is valid");

        coerce_property_map(&declarations, &mut props).expect("coercion succeeds");

        let Value::String(stored_text) = props.get(&text).expect("text property remains") else {
            panic!("text property stays a string");
        };
        assert!(std::ptr::eq(stored_text.as_str(), original_text.as_str()));
        let Value::Bytes(stored_bytes) = props.get(&bytes).expect("bytes property remains") else {
            panic!("bytes property stays bytes");
        };
        assert!(Arc::ptr_eq(stored_bytes, &original_bytes));
        assert!(matches!(props, PropertyMap::Compact { .. }));
    }

    #[test]
    fn property_diff_in_bounds_descriptor_values_reuse_storage() {
        let (text, bytes, declarations) = descriptor_declarations();
        let original_text = db_string("already-valid").expect("test value is valid");
        let original_bytes = Arc::<[u8]>::from([1_u8, 2, 3, 4]);
        let mut diff = PropertyDiff::new(
            [
                (text.clone(), Value::String(original_text.clone())),
                (bytes.clone(), Value::Bytes(Arc::clone(&original_bytes))),
            ],
            [],
        )
        .expect("test property diff is valid");

        coerce_property_diff(&declarations, &mut diff).expect("coercion succeeds");

        let Value::String(stored_text) = diff
            .set
            .iter()
            .find(|(key, _)| key == &text)
            .map(|(_, value)| value)
            .expect("text property remains")
        else {
            panic!("text property stays a string");
        };
        assert!(std::ptr::eq(stored_text.as_str(), original_text.as_str()));
        let Value::Bytes(stored_bytes) = diff
            .set
            .iter()
            .find(|(key, _)| key == &bytes)
            .map(|(_, value)| value)
            .expect("bytes property remains")
        else {
            panic!("bytes property stays bytes");
        };
        assert!(Arc::ptr_eq(stored_bytes, &original_bytes));
    }
}
