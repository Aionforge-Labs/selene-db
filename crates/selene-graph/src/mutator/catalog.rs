//! Catalog mutation methods for the transaction mutator.

use std::sync::Arc;

use selene_core::{
    ByteStringType, Change, CharacterStringType, DbString, EdgeEndpointDef as CoreEdgeEndpointDef,
    GraphTypeId, LabelSet, PredefinedValueType, PropertyDef, PropertyValueType, SchemaChange,
    ValueType,
};
use smallvec::SmallVec;

use crate::{
    DropBehavior, EdgeEndpointDef, EdgeTypeDef, GraphError, GraphResult, GraphTypeDef, Mutator,
    NodeTypeDef, PropertyElementType, PropertyTypeDef, RecordFieldType, RecordFieldTypes,
    ValidationMode,
    graph_types::{MAX_LIST_TYPE_NESTING, MAX_RECORD_TYPE_NESTING},
};

const OPEN_GRAPH_CATALOG_DDL: &str =
    "open graph (GG01) does not support catalog type DDL -- use a closed graph (GG02)";

impl<'tx, 'g> Mutator<'tx, 'g> {
    /// Add a node type to the transaction-local closed graph type.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when the graph is open, the type
    /// already exists, or the resulting graph type is structurally invalid.
    pub fn create_node_type(
        &mut self,
        name: DbString,
        key_labels: LabelSet,
        properties: Vec<PropertyTypeDef>,
        validation_mode: ValidationMode,
    ) -> GraphResult<()> {
        let mut graph_type = self.current_graph_type()?;
        if graph_type
            .node_types
            .iter()
            .any(|node_type| node_type.name == name)
        {
            return Err(GraphError::Inconsistent {
                reason: format!("node type {name} already exists"),
            });
        }
        let node_type = NodeTypeDef {
            name: name.clone(),
            key_labels,
            properties,
            validation_mode,
        };
        graph_type.node_types.push(node_type.clone());
        graph_type.validate_ref()?;
        let graph_id = self.txn.read().graph_id();
        self.txn.guard_mut().meta.bound_type = Some(Arc::new(graph_type));
        self.txn.changes.push(Change::SchemaChanged {
            graph: graph_id,
            change: SchemaChange::NodeTypeAddedV2 {
                graph_type: implicit_graph_type_id(),
                label: name,
                def: core_node_type_def(&node_type)?,
            },
        });
        Ok(())
    }

    /// Add an edge type to the transaction-local closed graph type.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when the graph is open, the type
    /// already exists, an endpoint index is invalid, or the resulting graph
    /// type is structurally invalid.
    pub fn create_edge_type(
        &mut self,
        name: DbString,
        label: DbString,
        source_node_type: EdgeEndpointDef,
        target_node_type: EdgeEndpointDef,
        properties: Vec<PropertyTypeDef>,
        validation_mode: ValidationMode,
    ) -> GraphResult<()> {
        let mut graph_type = self.current_graph_type()?;
        if graph_type
            .edge_types
            .iter()
            .any(|edge_type| edge_type.name == name)
        {
            return Err(GraphError::Inconsistent {
                reason: format!("edge type {name} already exists"),
            });
        }
        let edge_type = EdgeTypeDef {
            name,
            label: label.clone(),
            source_node_type,
            target_node_type,
            properties,
            validation_mode,
        };
        graph_type.edge_types.push(edge_type.clone());
        graph_type.validate_ref()?;
        let graph_id = self.txn.read().graph_id();
        self.txn.guard_mut().meta.bound_type = Some(Arc::new(graph_type.clone()));
        self.txn.changes.push(Change::SchemaChanged {
            graph: graph_id,
            change: SchemaChange::EdgeTypeAddedV2 {
                graph_type: implicit_graph_type_id(),
                label,
                def: core_edge_type_def(&graph_type, &edge_type)?,
            },
        });
        Ok(())
    }

    /// Drop a node type from the transaction-local closed graph type.
    ///
    /// `behavior` selects the surviving-instance / inbound-dependency policy
    /// (deletion-reclamation audit Item 3, Seam B):
    ///
    /// * [`DropBehavior::Restrict`] (the default) rejects the drop with
    ///   [`GraphError::Inconsistent`] when any instance still carries `name` as
    ///   its key label, or when an edge type still references the node type
    ///   (dangling-endpoint guard). Nothing is removed — no `Change` is recorded
    ///   and the bound graph type is left intact (no partial state).
    /// * [`DropBehavior::Cascade`] (`IM_DROP_CASCADE`) truncates every instance
    ///   first via [`Mutator::truncate_node_type`] (which also removes incident
    ///   edges, so no dangling endpoints remain), then drops the type. Both the
    ///   truncate change(s) and the [`SchemaChange::NodeTypeDropped`] land in the
    ///   same transaction, so commit and WAL replay are atomic.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when the graph is open, the type
    /// does not exist, `Restrict` finds surviving instances or an inbound edge
    /// dependency, or any edge endpoint would require positional endpoint
    /// reindexing.
    pub fn drop_node_type(&mut self, name: DbString, behavior: DropBehavior) -> GraphResult<()> {
        let graph_type = self.current_graph_type()?;
        let removed_index = graph_type
            .node_type_index_for(name.clone())
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!("node type {name} does not exist"),
            })?;
        match behavior {
            DropBehavior::Restrict => {
                // Seam-B fix: a surviving instance whose declared type is being
                // dropped would become an orphan on commit. Reject early with a
                // message that blames the drop, not the instance.
                let live = self
                    .txn
                    .read()
                    .nodes_with_label(&name)
                    .map_or(0, roaring::RoaringBitmap::len);
                if live > 0 {
                    return Err(GraphError::Inconsistent {
                        reason: format!(
                            "cannot drop node type {name}: {live} instance(s) still exist; use CASCADE to remove them"
                        ),
                    });
                }
                // Type-dependency: an edge type that directly references this
                // node type would be left with a dangling endpoint. Recursive
                // type cascade is out of scope (Item 3 is instance cascade only).
                for edge_type in &graph_type.edge_types {
                    if endpoint_references_node(&edge_type.source_node_type, removed_index)
                        || endpoint_references_node(&edge_type.target_node_type, removed_index)
                    {
                        return Err(GraphError::Inconsistent {
                            reason: format!(
                                "cannot drop node type {name}: edge type {} still references it",
                                edge_type.name
                            ),
                        });
                    }
                }
            }
            DropBehavior::Cascade => {
                // Truncate instances FIRST (reuses the BRIEF-150 funnel); this
                // also removes incident edges, so no dangling endpoint remains.
                self.truncate_node_type(name.clone())?;
            }
        }
        // Shared schema-drop step. The positional-reindexing guard still applies
        // to BOTH paths: if a surviving edge type references a node-type index
        // at or after the removed slot, the drop must reject (recursive type
        // cascade is out of scope). After a CASCADE truncate of `name`'s own
        // instances, an edge type that referenced `name` directly is structurally
        // empty but still declared, so this guard governs the type relationship.
        for edge_type in &graph_type.edge_types {
            if endpoint_depends_on_shifted_node(&edge_type.source_node_type, removed_index)
                || endpoint_depends_on_shifted_node(&edge_type.target_node_type, removed_index)
            {
                return Err(GraphError::Inconsistent {
                    reason: format!(
                        "cannot drop node type {name}: edge type {} still depends on node-type indexes that would require reindexing",
                        edge_type.name
                    ),
                });
            }
        }
        let next = graph_type
            .without_node_type(name.clone())
            .expect("node type existed above");
        next.validate_ref()?;
        let graph_id = self.txn.read().graph_id();
        self.txn.guard_mut().meta.bound_type = Some(Arc::new(next));
        self.txn.changes.push(Change::SchemaChanged {
            graph: graph_id,
            change: SchemaChange::NodeTypeDropped {
                graph_type: implicit_graph_type_id(),
                name,
            },
        });
        Ok(())
    }

    /// Drop an edge type from the transaction-local closed graph type.
    ///
    /// `behavior` selects the surviving-instance policy (deletion-reclamation
    /// audit Item 3, Seam B). Edge types have no inbound type dependency, so
    /// only the instance check applies:
    ///
    /// * [`DropBehavior::Restrict`] (the default) rejects with
    ///   [`GraphError::Inconsistent`] when any edge still carries `name`; nothing
    ///   is removed and no `Change` is recorded.
    /// * [`DropBehavior::Cascade`] (`IM_DROP_CASCADE`) truncates every edge of
    ///   the type first via [`Mutator::truncate_edge_type`], then drops the type,
    ///   atomically in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when the graph is open, the type
    /// does not exist, `Restrict` finds surviving instances, or the resulting
    /// graph type is structurally invalid.
    pub fn drop_edge_type(&mut self, name: DbString, behavior: DropBehavior) -> GraphResult<()> {
        let graph_type = self.current_graph_type()?;
        if graph_type.edge_type_index_for(name.clone()).is_none() {
            return Err(GraphError::Inconsistent {
                reason: format!("edge type {name} does not exist"),
            });
        }
        match behavior {
            DropBehavior::Restrict => {
                let live = self
                    .txn
                    .read()
                    .edges_with_label(&name)
                    .map_or(0, roaring::RoaringBitmap::len);
                if live > 0 {
                    return Err(GraphError::Inconsistent {
                        reason: format!(
                            "cannot drop edge type {name}: {live} instance(s) still exist; use CASCADE to remove them"
                        ),
                    });
                }
            }
            DropBehavior::Cascade => {
                self.truncate_edge_type(name.clone())?;
            }
        }
        let next = graph_type
            .without_edge_type(name.clone())
            .expect("edge type existed above");
        next.validate_ref()?;
        let graph_id = self.txn.read().graph_id();
        self.txn.guard_mut().meta.bound_type = Some(Arc::new(next));
        self.txn.changes.push(Change::SchemaChanged {
            graph: graph_id,
            change: SchemaChange::EdgeTypeDropped {
                graph_type: implicit_graph_type_id(),
                name,
            },
        });
        Ok(())
    }

    pub(super) fn current_graph_type(&self) -> GraphResult<GraphTypeDef> {
        self.txn
            .read()
            .meta
            .bound_type
            .as_deref()
            .cloned()
            .ok_or_else(|| GraphError::Inconsistent {
                reason: OPEN_GRAPH_CATALOG_DDL.to_owned(),
            })
    }
}

/// Return the implicit graph type ID used while one bound type is allowed per graph.
///
/// Future multi-type-bound graph work should replace this sentinel with a real
/// graph-type allocator and preserve the ID across WAL replay.
pub(super) fn implicit_graph_type_id() -> GraphTypeId {
    GraphTypeId::new(1).expect("implicit graph type id")
}

/// Whether `endpoint` directly references the node-type at `removed_index`.
///
/// Distinct from [`endpoint_depends_on_shifted_node`], which is the broader
/// positional-reindexing guard (>= removed_index). This narrower check powers
/// the clear RESTRICT message "edge type E still references it" for the direct
/// dangling-endpoint case.
fn endpoint_references_node(endpoint: &EdgeEndpointDef, removed_index: u32) -> bool {
    match endpoint {
        EdgeEndpointDef::Any => false,
        EdgeEndpointDef::NodeType(index) => *index == removed_index,
        EdgeEndpointDef::OneOf(indices) => indices.contains(&removed_index),
    }
}

fn endpoint_depends_on_shifted_node(endpoint: &EdgeEndpointDef, removed_index: u32) -> bool {
    // Why: node_type_index() returns None for OneOf, so the prior helper would
    // have silently let DROP NODE TYPE succeed when an OneOf endpoint depended
    // on the removed (or shifted) node. Walk each candidate index explicitly.
    match endpoint {
        EdgeEndpointDef::Any => false,
        EdgeEndpointDef::NodeType(index) => *index >= removed_index,
        EdgeEndpointDef::OneOf(indices) => indices.iter().any(|index| *index >= removed_index),
    }
}

pub(crate) fn core_node_type_def(node_type: &NodeTypeDef) -> GraphResult<selene_core::NodeTypeDef> {
    Ok(selene_core::NodeTypeDef {
        labels: node_type.key_labels.clone(),
        properties: core_node_properties(&node_type.properties)?,
        key: None,
        validation_mode: core_validation_mode(node_type.validation_mode),
    })
}

pub(crate) fn core_edge_type_def(
    graph_type: &GraphTypeDef,
    edge_type: &EdgeTypeDef,
) -> GraphResult<selene_core::EdgeTypeDef> {
    Ok(selene_core::EdgeTypeDef {
        label: edge_type.label.clone(),
        source_node_type: core_edge_endpoint_def(
            graph_type,
            edge_type.name.clone(),
            &edge_type.source_node_type,
        )?,
        target_node_type: core_edge_endpoint_def(
            graph_type,
            edge_type.name.clone(),
            &edge_type.target_node_type,
        )?,
        properties: core_edge_properties(&edge_type.properties)?,
        validation_mode: core_validation_mode(edge_type.validation_mode),
    })
}

pub(super) fn core_edge_endpoint_def(
    graph_type: &GraphTypeDef,
    edge_name: DbString,
    endpoint: &EdgeEndpointDef,
) -> GraphResult<CoreEdgeEndpointDef> {
    match endpoint {
        EdgeEndpointDef::Any => Ok(CoreEdgeEndpointDef::Any),
        EdgeEndpointDef::NodeType(index) => graph_type
            .node_types
            .get(*index as usize)
            .map(|node_type| {
                CoreEdgeEndpointDef::NodeType(selene_core::NodeTypeRef(node_type.name.clone()))
            })
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!("edge type {edge_name} references invalid node type {index}"),
            }),
        EdgeEndpointDef::OneOf(indices) => {
            let mut refs: SmallVec<[selene_core::NodeTypeRef; 4]> = SmallVec::new();
            for index in indices {
                let node_type = graph_type.node_types.get(*index as usize).ok_or_else(|| {
                    GraphError::Inconsistent {
                        reason: format!(
                            "edge type {edge_name} OneOf endpoint references invalid node type {index}"
                        ),
                    }
                })?;
                refs.push(selene_core::NodeTypeRef(node_type.name.clone()));
            }
            Ok(CoreEdgeEndpointDef::OneOf(refs))
        }
    }
}

fn core_node_properties(properties: &[PropertyTypeDef]) -> GraphResult<SmallVec<[PropertyDef; 8]>> {
    properties.iter().map(core_property_def).collect()
}

fn core_edge_properties(properties: &[PropertyTypeDef]) -> GraphResult<SmallVec<[PropertyDef; 4]>> {
    properties.iter().map(core_property_def).collect()
}

pub(super) fn core_property_def(property: &PropertyTypeDef) -> GraphResult<PropertyDef> {
    Ok(PropertyDef {
        name: property.name.clone(),
        value_type: core_value_type(
            property.value_type,
            property.list_element_type.as_ref(),
            property.decimal_type,
            property.character_string_type,
            property.byte_string_type,
            property.required,
        )?,
        nullable: !property.required,
        default: property
            .default
            .as_ref()
            .map(|default| default.to_value())
            .transpose()?,
        immutable: property.immutable,
        unique: property.unique,
        record_fields: core_record_fields(
            property.value_type,
            property.record_field_types.as_ref(),
        )?,
    })
}

const fn core_validation_mode(mode: ValidationMode) -> selene_core::ValidationMode {
    match mode {
        ValidationMode::Strict => selene_core::ValidationMode::Strict,
        ValidationMode::Warn => selene_core::ValidationMode::Warn,
    }
}

fn core_value_type(
    value_type: PropertyValueType,
    list_element_type: Option<&PropertyElementType>,
    decimal_type: Option<selene_core::DecimalType>,
    character_string_type: Option<CharacterStringType>,
    byte_string_type: Option<ByteStringType>,
    required: bool,
) -> GraphResult<ValueType> {
    let mut value_type = if value_type == PropertyValueType::List {
        let element_type = list_element_type.ok_or_else(|| GraphError::Inconsistent {
            reason: "LIST property definition is missing element type".to_owned(),
        })?;
        ValueType::list_of(core_element_value_type(element_type, 1)?)
    } else {
        core_scalar_value_type(
            value_type,
            decimal_type,
            character_string_type,
            byte_string_type,
        )
    };
    value_type.not_null = required;
    Ok(value_type)
}

fn core_element_value_type(
    element_type: &PropertyElementType,
    depth: u32,
) -> GraphResult<ValueType> {
    if depth > MAX_LIST_TYPE_NESTING {
        return Err(GraphError::Inconsistent {
            reason: "LIST property definition exceeds nesting limit".to_owned(),
        });
    }
    match element_type {
        PropertyElementType::Scalar(value_type) => {
            Ok(core_scalar_value_type(*value_type, None, None, None))
        }
        PropertyElementType::CharacterString(character_string_type) => Ok(core_scalar_value_type(
            PropertyValueType::String,
            None,
            Some(*character_string_type),
            None,
        )),
        PropertyElementType::Decimal(decimal_type) => Ok(core_scalar_value_type(
            PropertyValueType::Decimal,
            Some(*decimal_type),
            None,
            None,
        )),
        PropertyElementType::ByteString(byte_string_type) => Ok(core_scalar_value_type(
            PropertyValueType::Bytes,
            None,
            None,
            Some(*byte_string_type),
        )),
        PropertyElementType::List(inner) => Ok(ValueType::list_of(core_element_value_type(
            inner,
            depth + 1,
        )?)),
        PropertyElementType::NotNull(inner) => {
            let mut value_type = core_element_value_type(inner, depth)?;
            value_type.not_null = true;
            Ok(value_type)
        }
    }
}

fn core_scalar_value_type(
    value_type: PropertyValueType,
    decimal_type: Option<selene_core::DecimalType>,
    character_string_type: Option<CharacterStringType>,
    byte_string_type: Option<ByteStringType>,
) -> ValueType {
    let predefined = match value_type {
        PropertyValueType::Bool => Some(PredefinedValueType::Bool),
        PropertyValueType::Int => Some(PredefinedValueType::Int),
        PropertyValueType::Uint => Some(PredefinedValueType::Uint),
        PropertyValueType::Int128 => Some(PredefinedValueType::Int128),
        PropertyValueType::Uint128 => Some(PredefinedValueType::Uint128),
        PropertyValueType::Float => Some(PredefinedValueType::Float),
        PropertyValueType::Float32 => Some(PredefinedValueType::Float32),
        PropertyValueType::Decimal => Some(PredefinedValueType::Decimal),
        PropertyValueType::String => Some(PredefinedValueType::String),
        PropertyValueType::Bytes => Some(PredefinedValueType::Bytes),
        PropertyValueType::Path => Some(PredefinedValueType::Path),
        PropertyValueType::NodeRef => Some(PredefinedValueType::NodeRef),
        PropertyValueType::EdgeRef => Some(PredefinedValueType::EdgeRef),
        PropertyValueType::GraphRef => Some(PredefinedValueType::GraphRef),
        PropertyValueType::TableRef => Some(PredefinedValueType::TableRef),
        PropertyValueType::ZonedDateTime => Some(PredefinedValueType::ZonedDateTime),
        PropertyValueType::LocalDateTime => Some(PredefinedValueType::LocalDateTime),
        PropertyValueType::Date => Some(PredefinedValueType::Date),
        PropertyValueType::ZonedTime => Some(PredefinedValueType::ZonedTime),
        PropertyValueType::LocalTime => Some(PredefinedValueType::LocalTime),
        PropertyValueType::Duration => Some(PredefinedValueType::Duration),
        PropertyValueType::DurationYearToMonth => Some(PredefinedValueType::DurationYearToMonth),
        PropertyValueType::DurationDayToSecond => Some(PredefinedValueType::DurationDayToSecond),
        PropertyValueType::Uuid => Some(PredefinedValueType::Uuid),
        PropertyValueType::Vector => Some(PredefinedValueType::Vector),
        PropertyValueType::Json => Some(PredefinedValueType::Json),
        PropertyValueType::List
        | PropertyValueType::Record
        | PropertyValueType::RecordTyped
        | PropertyValueType::Null => None,
    };
    ValueType {
        predefined,
        decimal_type: if value_type == PropertyValueType::Decimal {
            decimal_type
        } else {
            None
        },
        character_string_type: if value_type == PropertyValueType::String {
            character_string_type
        } else {
            None
        },
        byte_string_type: if value_type == PropertyValueType::Bytes {
            byte_string_type
        } else {
            None
        },
        union: None,
        list_of: None,
        record: None,
        not_null: false,
        cardinality: selene_core::ValueTypeCardinality::ExactlyOne,
    }
}

/// Convert the rkyv-side typed-`RECORD` descriptor into the serde/WAL counterpart carried
/// on [`PropertyDef::record_fields`]. Returns `None` for every non-record property,
/// `Some(Open)` for an open/bare `RECORD` (no declared fields), and `Some(Closed(..))`
/// for a closed/typed `RECORD{..}`.
// Why: a RECORD property's record-ness must survive WAL replay; it rides
// `PropertyDef.record_fields` (structural-inline), not `ValueType.record`. The open/bare
// form carries no field list, so it persists as `Some(Open)` — without that marker WAL
// recovery cannot tell an open record from a scalar `Null` and degrades it to `Null`.
fn core_record_fields(
    value_type: PropertyValueType,
    fields: Option<&RecordFieldTypes>,
) -> GraphResult<Option<Box<selene_core::RecordFieldStructure>>> {
    match (value_type, fields) {
        (PropertyValueType::RecordTyped, Some(fields)) => {
            Ok(Some(Box::new(core_record_field_structure(fields, 1)?)))
        }
        (PropertyValueType::RecordTyped, None) => {
            Ok(Some(Box::new(selene_core::RecordFieldStructure::Open)))
        }
        _ => Ok(None),
    }
}

fn core_record_field_structure(
    fields: &RecordFieldTypes,
    depth: u32,
) -> GraphResult<selene_core::RecordFieldStructure> {
    if depth > MAX_RECORD_TYPE_NESTING {
        return Err(GraphError::Inconsistent {
            reason: "RECORD property definition exceeds nesting limit".to_owned(),
        });
    }
    let defs = fields
        .0
        .iter()
        .map(|field| {
            Ok(selene_core::RecordFieldStructureDef {
                name: field.name.clone(),
                field_type: core_record_field_structure_type(&field.field_type, depth)?,
                required: field.required,
            })
        })
        .collect::<GraphResult<Vec<_>>>()?;
    Ok(selene_core::RecordFieldStructure::Closed(defs))
}

fn core_record_field_structure_type(
    field_type: &RecordFieldType,
    depth: u32,
) -> GraphResult<selene_core::RecordFieldStructureType> {
    Ok(match field_type {
        RecordFieldType::Scalar(value_type) => {
            selene_core::RecordFieldStructureType::Scalar(*value_type)
        }
        RecordFieldType::CharacterString(character_string_type) => {
            selene_core::RecordFieldStructureType::CharacterString(*character_string_type)
        }
        RecordFieldType::Decimal(decimal_type) => {
            selene_core::RecordFieldStructureType::Decimal(*decimal_type)
        }
        RecordFieldType::ByteString(byte_string_type) => {
            selene_core::RecordFieldStructureType::ByteString(*byte_string_type)
        }
        RecordFieldType::List(inner) => selene_core::RecordFieldStructureType::List(Box::new(
            core_record_field_structure_type(inner, depth + 1)?,
        )),
        RecordFieldType::OpenRecord => selene_core::RecordFieldStructureType::Record(Box::new(
            selene_core::RecordFieldStructure::Open,
        )),
        RecordFieldType::Record(inner) => selene_core::RecordFieldStructureType::Record(Box::new(
            core_record_field_structure(inner, depth + 1)?,
        )),
        RecordFieldType::NotNull(inner) => selene_core::RecordFieldStructureType::NotNull(
            Box::new(core_record_field_structure_type(inner, depth)?),
        ),
    })
}

#[cfg(test)]
mod tests;
