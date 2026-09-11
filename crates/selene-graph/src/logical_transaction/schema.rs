//! Owner adapters between logical names and runtime graph schema positions.

use crate::{EdgeTypeDef, GraphTypeDef, NodeTypeDef, ValidationMode};
use selene_core::logical::{CodecError as E, CodecResult, GraphDefinition};

/// Extract the complete logical schema, retaining explicit node and edge type names.
pub fn definition(def: &GraphTypeDef) -> CodecResult<GraphDefinition> {
    def.validate_ref().map_err(|_| E::Semantic)?;
    let mut definition = GraphDefinition {
        name: def.name.clone(),
        nodes: def
            .node_types
            .iter()
            .map(|node| {
                Ok((
                    node.name.clone(),
                    crate::mutator::catalog::core_node_type_def(node).map_err(|_| E::Semantic)?,
                ))
            })
            .collect::<CodecResult<_>>()?,
        edges: def
            .edge_types
            .iter()
            .map(|edge| {
                Ok((
                    edge.name.clone(),
                    crate::mutator::catalog::core_edge_type_def(def, edge)
                        .map_err(|_| E::Semantic)?,
                ))
            })
            .collect::<CodecResult<_>>()?,
    };
    for (original, (_, node)) in def.node_types.iter().zip(&mut definition.nodes) {
        preserve_open_records(&original.properties, &mut node.properties);
    }
    for (original, (_, edge)) in def.edge_types.iter().zip(&mut definition.edges) {
        preserve_open_records(&original.properties, &mut edge.properties);
    }
    Ok(definition)
}

fn preserve_open_records(
    original: &[crate::PropertyTypeDef],
    properties: &mut [selene_core::PropertyDef],
) {
    for (original, property) in original.iter().zip(properties) {
        if original.value_type == selene_core::PropertyValueType::Record {
            property.record_fields = Some(Box::new(selene_core::RecordFieldStructure::Open));
        }
    }
}

pub(super) fn materialize(def: &GraphDefinition) -> CodecResult<GraphTypeDef> {
    let mut result = GraphTypeDef {
        name: def.name.clone(),
        node_types: Vec::new(),
        edge_types: Vec::new(),
    };
    for (name, node) in &def.nodes {
        if node.key.is_some() {
            return Err(E::Invalid("legacy node key"));
        }
        result.node_types.push(NodeTypeDef {
            name: name.clone(),
            key_labels: node.labels.clone(),
            properties: node
                .properties
                .iter()
                .map(|p| crate::core_provider::decode_schema_property(p).map_err(|_| E::Semantic))
                .collect::<CodecResult<_>>()?,
            validation_mode: mode(node.validation_mode),
        });
    }
    for (name, edge) in &def.edges {
        result.edge_types.push(EdgeTypeDef {
            name: name.clone(),
            label: edge.label.clone(),
            source_node_type: crate::core_provider::decode_schema_edge_endpoint(
                &result,
                &edge.source_node_type,
                "source",
            )
            .map_err(|_| E::Semantic)?,
            target_node_type: crate::core_provider::decode_schema_edge_endpoint(
                &result,
                &edge.target_node_type,
                "target",
            )
            .map_err(|_| E::Semantic)?,
            properties: edge
                .properties
                .iter()
                .map(|p| crate::core_provider::decode_schema_property(p).map_err(|_| E::Semantic))
                .collect::<CodecResult<_>>()?,
            validation_mode: mode(edge.validation_mode),
        });
    }
    result.validate_ref().map_err(|_| E::Semantic)?;
    // An adapter may not silently erase an unsupported descriptor field.
    let normalized = definition(&result)?;
    let mut left = selene_core::logical::Encoder::new(Default::default())?;
    let mut right = selene_core::logical::Encoder::new(Default::default())?;
    left.graph_definition(def)?;
    right.graph_definition(&normalized)?;
    if left.finish() != right.finish() {
        return Err(E::Invalid("lossy schema adapter"));
    }
    Ok(result)
}
fn mode(mode: selene_core::ValidationMode) -> ValidationMode {
    match mode {
        selene_core::ValidationMode::Strict => ValidationMode::Strict,
        selene_core::ValidationMode::Warn => ValidationMode::Warn,
    }
}
