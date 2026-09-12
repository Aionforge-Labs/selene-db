//! Additive catalog ALTER helpers.

use std::sync::Arc;

use selene_core::{Change, DbString, SchemaChange};
use smallvec::SmallVec;

use super::catalog::{core_edge_endpoint_def, core_property_def, implicit_graph_type_id};
use crate::{EdgeEndpointDef, GraphError, GraphResult, GraphTypeDef, Mutator, PropertyTypeDef};

impl<'tx, 'g> Mutator<'tx, 'g> {
    /// Add nullable properties to an existing node type.
    ///
    /// The named node type is replaced at its existing vector position so edge
    /// endpoint indexes retain their identity. Exact property repeats are
    /// idempotent; a conflicting descriptor or a genuinely new required
    /// property is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when the graph is open, the node
    /// type does not exist, a new property is required, or an existing property
    /// is redefined with a different descriptor. Invalid defaults and property
    /// descriptors that cannot round-trip losslessly through the durable schema
    /// codec are also rejected before transaction state changes.
    pub fn alter_node_type(
        &mut self,
        name: DbString,
        properties: Vec<PropertyTypeDef>,
    ) -> GraphResult<()> {
        let mut graph_type = self.current_graph_type()?;
        let Some(index) = graph_type.node_type_index_for(name.clone()) else {
            return Err(GraphError::Inconsistent {
                reason: format!("node type {name} does not exist"),
            });
        };
        let mut node_type = graph_type.node_types[index as usize].clone();
        let mut added = SmallVec::new();
        for property in properties {
            merge_added_node_property(&name, &mut node_type.properties, &mut added, property)?;
        }
        if added.is_empty() {
            return Ok(());
        }
        graph_type.node_types[index as usize] = node_type;
        graph_type.validate_ref()?;

        let graph_id = self.txn.read().graph_id();
        self.txn.guard_mut().meta.bound_type = Some(Arc::new(graph_type));
        self.txn.changes.push(Change::SchemaChanged {
            graph: graph_id,
            change: SchemaChange::NodeTypeAlteredV2 {
                graph_type: implicit_graph_type_id(),
                label: name,
                properties: added,
            },
        });
        Ok(())
    }

    /// Additively widen an existing edge type.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when the graph is open, the edge
    /// type does not exist, an endpoint would narrow, a new property is
    /// required, or a duplicate property has a different descriptor. Invalid
    /// defaults and property or endpoint descriptors that cannot round-trip
    /// losslessly through the durable schema codec are rejected before
    /// transaction state changes.
    pub fn alter_edge_type(
        &mut self,
        name: DbString,
        source_node_type: Option<EdgeEndpointDef>,
        target_node_type: Option<EdgeEndpointDef>,
        properties: Vec<PropertyTypeDef>,
    ) -> GraphResult<()> {
        let mut graph_type = self.current_graph_type()?;
        let Some(index) = graph_type.edge_type_index_for(name.clone()) else {
            return Err(GraphError::Inconsistent {
                reason: format!("edge type {name} does not exist"),
            });
        };
        let mut edge_type = graph_type.edge_types[index as usize].clone();
        let source_node_type = encode_changed_endpoint(
            &graph_type,
            &name,
            "source",
            &edge_type.source_node_type,
            source_node_type,
        )?;
        if let Some((next, _)) = source_node_type.as_ref() {
            edge_type.source_node_type = next.clone();
        }
        let target_node_type = encode_changed_endpoint(
            &graph_type,
            &name,
            "target",
            &edge_type.target_node_type,
            target_node_type,
        )?;
        if let Some((next, _)) = target_node_type.as_ref() {
            edge_type.target_node_type = next.clone();
        }
        let mut added = SmallVec::new();
        for property in properties {
            merge_added_edge_property(&name, &mut edge_type.properties, &mut added, property)?;
        }
        if source_node_type.is_none() && target_node_type.is_none() && added.is_empty() {
            return Ok(());
        }
        graph_type.edge_types[index as usize] = edge_type;
        graph_type.validate_ref()?;

        let graph_id = self.txn.read().graph_id();
        self.txn.guard_mut().meta.bound_type = Some(Arc::new(graph_type));
        self.txn.changes.push(Change::SchemaChanged {
            graph: graph_id,
            change: SchemaChange::EdgeTypeAlteredV2 {
                graph_type: implicit_graph_type_id(),
                name,
                source_node_type: source_node_type.map(|(_, encoded)| encoded),
                target_node_type: target_node_type.map(|(_, encoded)| encoded),
                properties: added,
            },
        });
        Ok(())
    }
}

fn merge_added_node_property(
    node_type: &DbString,
    properties: &mut Vec<PropertyTypeDef>,
    added: &mut SmallVec<[selene_core::PropertyDef; 8]>,
    property: PropertyTypeDef,
) -> GraphResult<()> {
    if let Some(existing) = properties
        .iter()
        .find(|candidate| candidate.name == property.name)
    {
        if existing == &property {
            return Ok(());
        }
        return Err(GraphError::Inconsistent {
            reason: format!(
                "ALTER NODE TYPE :{node_type} cannot redefine property {}",
                property.name
            ),
        });
    }
    if property.required {
        return Err(GraphError::Inconsistent {
            reason: format!(
                "ALTER NODE TYPE :{node_type} cannot add required property {}",
                property.name
            ),
        });
    }
    crate::type_validator::validate_property_default(&property)?;
    let encoded = core_property_def(&property)?;
    let decoded =
        super::schema_event::property(&encoded).map_err(|error| GraphError::Inconsistent {
            reason: format!(
                "ALTER NODE TYPE :{node_type} property {} cannot be represented durably: {error}",
                property.name
            ),
        })?;
    if decoded != property {
        return Err(GraphError::Inconsistent {
            reason: format!(
                "ALTER NODE TYPE :{node_type} property {} cannot be represented losslessly in WAL",
                property.name
            ),
        });
    }
    properties.push(property);
    added.push(encoded);
    Ok(())
}

fn merge_added_edge_property(
    edge_type: &DbString,
    properties: &mut Vec<PropertyTypeDef>,
    added: &mut SmallVec<[selene_core::PropertyDef; 4]>,
    property: PropertyTypeDef,
) -> GraphResult<()> {
    if let Some(existing) = properties
        .iter()
        .find(|candidate| candidate.name == property.name)
    {
        if existing == &property {
            return Ok(());
        }
        return Err(GraphError::Inconsistent {
            reason: format!(
                "ALTER EDGE TYPE :{edge_type} cannot redefine property {}",
                property.name
            ),
        });
    }
    if property.required {
        return Err(GraphError::Inconsistent {
            reason: format!(
                "ALTER EDGE TYPE :{edge_type} cannot add required property {}",
                property.name
            ),
        });
    }
    crate::type_validator::validate_property_default(&property)?;
    let encoded = core_property_def(&property)?;
    let decoded =
        super::schema_event::property(&encoded).map_err(|error| GraphError::Inconsistent {
            reason: format!(
                "ALTER EDGE TYPE :{edge_type} property {} cannot be represented durably: {error}",
                property.name
            ),
        })?;
    if decoded != property {
        return Err(GraphError::Inconsistent {
            reason: format!(
                "ALTER EDGE TYPE :{edge_type} property {} cannot be represented losslessly in WAL",
                property.name
            ),
        });
    }
    properties.push(property);
    added.push(encoded);
    Ok(())
}

fn encode_changed_endpoint(
    graph_type: &GraphTypeDef,
    edge_type: &DbString,
    role: &'static str,
    current: &EdgeEndpointDef,
    next: Option<EdgeEndpointDef>,
) -> GraphResult<Option<(EdgeEndpointDef, selene_core::EdgeEndpointDef)>> {
    let Some(next) = next else {
        return Ok(None);
    };
    ensure_endpoint_widening(role, edge_type, current, &next)?;
    if current == &next {
        return Ok(None);
    }
    let encoded = core_edge_endpoint_def(graph_type, edge_type.clone(), &next)?;
    let decoded = super::schema_event::endpoint(graph_type, &encoded, role)
        .map_err(|error| GraphError::Inconsistent {
            reason: format!(
                "ALTER EDGE TYPE :{edge_type} {role} endpoint cannot be represented durably: {error}"
            ),
        })?;
    if decoded != next {
        return Err(GraphError::Inconsistent {
            reason: format!(
                "ALTER EDGE TYPE :{edge_type} {role} endpoint cannot be represented losslessly in WAL"
            ),
        });
    }
    Ok(Some((next, encoded)))
}

fn ensure_endpoint_widening(
    endpoint_name: &'static str,
    edge_type: &DbString,
    current: &EdgeEndpointDef,
    next: &EdgeEndpointDef,
) -> GraphResult<()> {
    if endpoint_is_subset(current, next) {
        return Ok(());
    }
    Err(GraphError::Inconsistent {
        reason: format!(
            "ALTER EDGE TYPE :{edge_type} {endpoint_name} endpoint would narrow from {current} to {next}"
        ),
    })
}

fn endpoint_is_subset(current: &EdgeEndpointDef, next: &EdgeEndpointDef) -> bool {
    match (current, next) {
        (_, EdgeEndpointDef::Any) => true,
        (EdgeEndpointDef::Any, _) => matches!(next, EdgeEndpointDef::Any),
        (EdgeEndpointDef::NodeType(current), EdgeEndpointDef::NodeType(next)) => current == next,
        (EdgeEndpointDef::NodeType(current), EdgeEndpointDef::OneOf(next)) => {
            next.binary_search(current).is_ok()
        }
        (EdgeEndpointDef::OneOf(current), EdgeEndpointDef::OneOf(next)) => current
            .iter()
            .all(|current| next.binary_search(current).is_ok()),
        (EdgeEndpointDef::OneOf(_), EdgeEndpointDef::NodeType(_)) => false,
    }
}

#[cfg(test)]
mod tests;
