//! EXTENDS property-composition helpers for catalog DDL.

use selene_core::DbString;
use selene_graph::{GraphTypeDef, PropertyDefaultValue, PropertyElementType, PropertyTypeDef};

use super::property::{render_property_default_value, render_property_value_type};
use crate::{ExecutorError, SourceSpan};

pub(super) fn compose_node_properties(
    graph_type: &GraphTypeDef,
    child: DbString,
    parent: DbString,
    child_properties: Vec<PropertyTypeDef>,
    span: SourceSpan,
) -> Result<Vec<PropertyTypeDef>, ExecutorError> {
    if let Some(index) = graph_type.node_type_index_for(parent.clone()) {
        let parent_properties = &graph_type.node_types[index as usize].properties;
        return merge_properties(parent_properties, child, child_properties, span);
    }
    if graph_type.edge_type_index_for(parent.clone()).is_some() {
        return Err(ExecutorError::GraphTypeViolation {
            message: format!(
                "CREATE NODE TYPE :{child} EXTENDS :{parent} - parent '{parent}' is an edge type; node types may only extend node types"
            ),
            span,
        });
    }
    Err(ExecutorError::GraphTypeViolation {
        message: format!(
            "CREATE NODE TYPE :{child} EXTENDS :{parent} - parent node type '{parent}' is not declared in this graph"
        ),
        span,
    })
}

pub(super) fn compose_edge_properties(
    graph_type: &GraphTypeDef,
    child: DbString,
    parent: DbString,
    child_properties: Vec<PropertyTypeDef>,
    span: SourceSpan,
) -> Result<Vec<PropertyTypeDef>, ExecutorError> {
    if let Some(index) = graph_type.edge_type_index_for(parent.clone()) {
        let parent_properties = &graph_type.edge_types[index as usize].properties;
        return merge_properties(parent_properties, child, child_properties, span);
    }
    if graph_type.node_type_index_for(parent.clone()).is_some() {
        return Err(ExecutorError::GraphTypeViolation {
            message: format!(
                "CREATE EDGE TYPE :{child} EXTENDS :{parent} - parent '{parent}' is a node type; edge types may only extend edge types"
            ),
            span,
        });
    }
    Err(ExecutorError::GraphTypeViolation {
        message: format!(
            "CREATE EDGE TYPE :{child} EXTENDS :{parent} - parent edge type '{parent}' is not declared in this graph"
        ),
        span,
    })
}

fn merge_properties(
    parent_properties: &[PropertyTypeDef],
    child: DbString,
    child_properties: Vec<PropertyTypeDef>,
    span: SourceSpan,
) -> Result<Vec<PropertyTypeDef>, ExecutorError> {
    let mut merged = parent_properties.to_vec();
    for child_property in child_properties {
        if let Some(parent_property) = merged
            .iter()
            .find(|property| property.name == child_property.name)
        {
            check_property_match(parent_property, &child_property, child.clone(), span)?;
            continue;
        }
        merged.push(child_property);
    }
    Ok(merged)
}

fn check_property_match(
    parent: &PropertyTypeDef,
    child: &PropertyTypeDef,
    child_type: DbString,
    span: SourceSpan,
) -> Result<(), ExecutorError> {
    if parent.value_type != child.value_type {
        return Err(property_conflict(
            parent,
            child_type,
            "value type",
            render_property_value_type(
                parent.value_type,
                parent.list_element_type.as_ref(),
                parent.record_field_types.as_ref(),
                parent.decimal_type,
                parent.character_string_type,
                parent.byte_string_type,
            ),
            render_property_value_type(
                child.value_type,
                child.list_element_type.as_ref(),
                child.record_field_types.as_ref(),
                child.decimal_type,
                child.character_string_type,
                child.byte_string_type,
            ),
            span,
        ));
    }
    if parent.decimal_type != child.decimal_type {
        return Err(property_conflict(
            parent,
            child_type,
            "decimal precision/scale",
            render_decimal(parent.decimal_type),
            render_decimal(child.decimal_type),
            span,
        ));
    }
    if parent.character_string_type != child.character_string_type {
        return Err(property_conflict(
            parent,
            child_type,
            "character-string length",
            render_character_string(parent.character_string_type),
            render_character_string(child.character_string_type),
            span,
        ));
    }
    if parent.byte_string_type != child.byte_string_type {
        return Err(property_conflict(
            parent,
            child_type,
            "byte-string length",
            render_byte_string(parent.byte_string_type),
            render_byte_string(child.byte_string_type),
            span,
        ));
    }
    if parent.list_element_type != child.list_element_type {
        return Err(property_conflict(
            parent,
            child_type,
            "list element type",
            render_list_element(parent.list_element_type.as_ref()),
            render_list_element(child.list_element_type.as_ref()),
            span,
        ));
    }
    if parent.required != child.required {
        return Err(property_conflict(
            parent,
            child_type,
            "NOT NULL constraint",
            render_bool(parent.required),
            render_bool(child.required),
            span,
        ));
    }
    if parent.default != child.default {
        return Err(property_conflict(
            parent,
            child_type,
            "DEFAULT value",
            render_default(parent.default.as_ref()),
            render_default(child.default.as_ref()),
            span,
        ));
    }
    if parent.immutable != child.immutable {
        return Err(property_conflict(
            parent,
            child_type,
            "IMMUTABLE constraint",
            render_bool(parent.immutable),
            render_bool(child.immutable),
            span,
        ));
    }
    if parent.unique != child.unique {
        return Err(property_conflict(
            parent,
            child_type,
            "UNIQUE constraint",
            render_bool(parent.unique),
            render_bool(child.unique),
            span,
        ));
    }
    Ok(())
}

fn render_decimal(value: Option<selene_core::DecimalType>) -> String {
    match value {
        Some(value) if value.scale == 0 => format!("DECIMAL({})", value.precision),
        Some(value) => format!("DECIMAL({}, {})", value.precision, value.scale),
        None => "DECIMAL".to_owned(),
    }
}

fn render_character_string(value: Option<selene_core::CharacterStringType>) -> String {
    match value {
        Some(value) if value.min_len == 0 => format!("STRING({})", value.max_len),
        Some(value) => format!("STRING({}, {})", value.min_len, value.max_len),
        None => "STRING".to_owned(),
    }
}

fn render_byte_string(value: Option<selene_core::ByteStringType>) -> String {
    match value {
        Some(value) if value.min_len == 0 => format!("BYTES({})", value.max_len),
        Some(value) => format!("BYTES({}, {})", value.min_len, value.max_len),
        None => "BYTES".to_owned(),
    }
}

fn property_conflict(
    parent: &PropertyTypeDef,
    child_type: DbString,
    field: &'static str,
    parent_value: String,
    child_value: String,
    span: SourceSpan,
) -> ExecutorError {
    ExecutorError::GraphTypeViolation {
        message: format!(
            "property '{}' redeclared with different {field} (parent: {parent_value}, child: {child_value}) on child type {child_type}",
            parent.name
        ),
        span,
    }
}

fn render_bool(value: bool) -> String {
    value.to_string()
}

fn render_default(default: Option<&PropertyDefaultValue>) -> String {
    match default {
        None => "none".to_owned(),
        Some(value) => render_property_default_value(value)
            .unwrap_or_else(|_| "<unsupported-default>".to_owned()),
    }
}

fn render_list_element(element: Option<&PropertyElementType>) -> String {
    match element {
        None => "none".to_owned(),
        Some(PropertyElementType::Scalar(value_type)) => {
            render_property_value_type(*value_type, None, None, None, None, None)
        }
        Some(PropertyElementType::Decimal(decimal_type)) => render_decimal(Some(*decimal_type)),
        Some(PropertyElementType::CharacterString(character_string_type)) => {
            render_character_string(Some(*character_string_type))
        }
        Some(PropertyElementType::ByteString(byte_string_type)) => {
            render_byte_string(Some(*byte_string_type))
        }
        Some(PropertyElementType::List(inner)) => {
            format!("LIST<{}>", render_list_element(Some(inner)))
        }
        Some(PropertyElementType::NotNull(inner)) => {
            format!("{} NOT NULL", render_list_element(Some(inner)))
        }
        Some(_) => "<unsupported-element>".to_owned(),
    }
}
