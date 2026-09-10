//! Semantic storage admission for every logical property/default payload.

use crate::{Change, CoreResult, PropertyDef, SchemaChange, StoredValue};

impl Change {
    /// Reject query-only or unbounded property/default payloads before any
    /// mutation, provider callback, or durable publication is performed.
    pub fn validate_stored_values(&self) -> CoreResult<()> {
        match self {
            Self::NodeCreated { properties, .. } | Self::EdgeCreated { properties, .. } => {
                properties.validate_stored_values()
            }
            Self::NodeUpdated {
                properties_diff, ..
            }
            | Self::EdgeUpdated {
                properties_diff, ..
            } => properties_diff.validate_stored_values(),
            Self::SchemaChanged { change, .. } => change.validate_stored_values(),
            Self::NodeDeleted { .. }
            | Self::EdgeDeleted { .. }
            | Self::NodePropertyRemoved { .. }
            | Self::EdgePropertyRemoved { .. }
            | Self::NodeLabelRemoved { .. }
            | Self::NodesOfTypeTruncated { .. }
            | Self::EdgesOfTypeTruncated { .. }
            | Self::GraphReset { .. } => Ok(()),
        }
    }
}

impl SchemaChange {
    /// Validate property defaults in every schema payload, including legacy
    /// carriers. This defines admission only, not the format-2 byte encoding.
    pub fn validate_stored_values(&self) -> CoreResult<()> {
        match self {
            Self::GraphTypeCreated { graph_type } => {
                for def in graph_type.node_types.values() {
                    properties(&def.properties)?;
                }
                for def in graph_type.edge_types.values() {
                    properties(&def.properties)?;
                }
                for def in graph_type.record_types.values() {
                    properties(&def.fields)?;
                }
                Ok(())
            }
            Self::NodeTypeAdded { def, .. } => legacy_properties(&def.properties),
            Self::EdgeTypeAdded { def, .. } => legacy_properties(&def.properties),
            Self::NodeTypeAddedV2 { def, .. } => properties(&def.properties),
            Self::EdgeTypeAddedV2 { def, .. } => properties(&def.properties),
            Self::NodeTypeAlteredV2 {
                properties: fields, ..
            } => properties(fields),
            Self::EdgeTypeAlteredV2 {
                properties: fields, ..
            } => properties(fields),
            Self::RecordTypeAdded { def, .. } => properties(&def.fields),
            Self::GraphCreated { .. }
            | Self::GraphDropped { .. }
            | Self::GraphTypeDropped { .. }
            | Self::NodeTypeDropped { .. }
            | Self::EdgeTypeDropped { .. }
            | Self::PropertyIndexCreated { .. }
            | Self::PropertyIndexDropped { .. }
            | Self::PropertyIndexCreatedNamed { .. }
            | Self::CompositePropertyIndexCreated { .. }
            | Self::CompositePropertyIndexDropped { .. }
            | Self::VectorIndexCreated { .. }
            | Self::VectorIndexDropped { .. }
            | Self::TextIndexCreated { .. }
            | Self::TextIndexDropped { .. }
            | Self::EdgePropertyIndexCreated { .. }
            | Self::EdgePropertyIndexDropped { .. } => Ok(()),
        }
    }
}

fn properties(fields: &[PropertyDef]) -> CoreResult<()> {
    for field in fields {
        if let Some(value) = &field.default {
            StoredValue::validate(value)?;
        }
        field.value_type.validate_stored_descriptor()?;
        if let Some(fields) = field.record_fields.as_deref() {
            fields.validate_stored_descriptor()?;
        }
    }
    Ok(())
}

fn legacy_properties(fields: &[crate::PropertyDefV1]) -> CoreResult<()> {
    for field in fields {
        if let Some(value) = &field.default {
            StoredValue::validate(value)?;
        }
        field.value_type.validate_stored_descriptor()?;
    }
    Ok(())
}
