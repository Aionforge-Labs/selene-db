//! Closed graph type catalog definitions.

mod endpoint;
mod legacy_default_serde;
mod property_defaults;
mod property_element_types;
mod record_types;
mod structural;

use std::collections::BTreeSet;

use selene_core::{
    ByteStringType, CharacterStringType, DbString, DecimalType, LabelSet, PropertyValueType,
};
use serde::{Deserialize, Serialize};

pub use endpoint::EdgeEndpointDef;
pub use property_defaults::{PropertyDefaultRecordField, PropertyDefaultValue};
pub use property_element_types::PropertyElementType;
use record_types::validate_record_field_types;
pub use record_types::{RecordFieldType, RecordFieldTypeDef, RecordFieldTypes};

use crate::error::{GraphError, GraphResult};

/// Maximum supported nesting for catalog `LIST<T>` property element descriptors.
pub const MAX_LIST_TYPE_NESTING: u32 = 64;

/// Maximum supported nesting for catalog typed-`RECORD` field-type descriptors.
///
/// Shares the `LIST` budget: a single `depth` counter threads the heterogeneous
/// `LIST`/`RECORD` nesting tower. Impl-defined per ISO 39075:2024 §4.15.4 (IL015),
/// not an ISO constant.
pub const MAX_RECORD_TYPE_NESTING: u32 = MAX_LIST_TYPE_NESTING;

/// Definition of a closed graph type per ISO clause 18.
#[derive(
    Clone,
    Debug,
    Deserialize,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub struct GraphTypeDef {
    /// Graph type name.
    pub name: DbString,
    /// Node-type elements in graph-type order.
    pub node_types: Vec<NodeTypeDef>,
    /// Edge-type elements in graph-type order.
    pub edge_types: Vec<EdgeTypeDef>,
}

impl GraphTypeDef {
    /// Validate this graph type's structural invariants.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when the type contains duplicate
    /// names, invalid edge endpoint indexes, duplicate properties within a
    /// node/edge type, duplicate edge triples, or an empty node label set.
    pub fn validate(self) -> GraphResult<Self> {
        self.validate_ref()?;
        Ok(self)
    }

    /// Return the first node type matching `labels`.
    #[must_use]
    pub fn find_node_type(&self, labels: &LabelSet) -> Option<&NodeTypeDef> {
        self.node_types
            .iter()
            .find(|node_type| &node_type.key_labels == labels)
    }

    /// Return the first node-type index matching `labels`.
    #[must_use]
    pub fn find_node_type_index(&self, labels: &LabelSet) -> Option<u32> {
        self.node_types
            .iter()
            .position(|node_type| &node_type.key_labels == labels)
            .and_then(|index| u32::try_from(index).ok())
    }

    /// Return the node-type index matching `name`.
    #[must_use]
    pub fn node_type_index_for(&self, name: DbString) -> Option<u32> {
        self.node_types
            .iter()
            .position(|node_type| node_type.name == name)
            .and_then(|index| u32::try_from(index).ok())
    }

    /// Return the edge type matching `label` and observed endpoint node types.
    #[must_use]
    pub fn find_edge_type(
        &self,
        label: DbString,
        source_node_type: u32,
        target_node_type: u32,
    ) -> Option<&EdgeTypeDef> {
        self.edge_types.iter().find(|edge_type| {
            edge_type.label == label
                && edge_type
                    .source_node_type
                    .matches_node_type(source_node_type)
                && edge_type
                    .target_node_type
                    .matches_node_type(target_node_type)
        })
    }

    /// Match endpoints without assigning an orientation to an undirected edge.
    /// Ambiguous unordered declarations do not select an arbitrary property schema.
    #[must_use]
    pub fn find_mixed_edge_type(
        &self,
        label: DbString,
        first: u32,
        second: u32,
        directionality: selene_core::EdgeDirectionality,
    ) -> Option<&EdgeTypeDef> {
        if directionality == selene_core::EdgeDirectionality::Directed {
            return self.find_edge_type(label, first, second);
        }
        let mut matches = self.edge_types.iter().filter(|edge| {
            edge.label == label
                && ((edge.source_node_type.matches_node_type(first)
                    && edge.target_node_type.matches_node_type(second))
                    || (edge.source_node_type.matches_node_type(second)
                        && edge.target_node_type.matches_node_type(first)))
        });
        let matched = matches.next()?;
        matches.next().is_none().then_some(matched)
    }

    /// Return the first edge type carrying `label`.
    #[must_use]
    pub fn first_edge_type_with_label(&self, label: DbString) -> Option<&EdgeTypeDef> {
        self.edge_types
            .iter()
            .find(|edge_type| edge_type.label == label)
    }

    /// Return the edge-type index matching `name`.
    #[must_use]
    pub fn edge_type_index_for(&self, name: DbString) -> Option<u32> {
        self.edge_types
            .iter()
            .position(|edge_type| edge_type.name == name)
            .and_then(|index| u32::try_from(index).ok())
    }

    /// Return a copy with the named node type removed.
    ///
    /// Edge endpoint indexes are intentionally not rewritten. Callers that
    /// cannot tolerate positional drift must reject the drop before using this
    /// helper.
    #[must_use]
    pub fn without_node_type(&self, name: DbString) -> Option<Self> {
        let index = self
            .node_types
            .iter()
            .position(|node_type| node_type.name == name)?;
        let mut next = self.clone();
        next.node_types.remove(index);
        Some(next)
    }

    /// Return a copy with the named edge type removed.
    #[must_use]
    pub fn without_edge_type(&self, name: DbString) -> Option<Self> {
        let index = self
            .edge_types
            .iter()
            .position(|edge_type| edge_type.name == name)?;
        let mut next = self.clone();
        next.edge_types.remove(index);
        Some(next)
    }

    /// Validate the type without consuming it.
    ///
    /// Same checks as [`GraphTypeDef::validate`]; preferred when callers
    /// already hold a reference (recovery, [`crate::SharedGraph::from_graph`]
    /// re-validation) and cannot move the value.
    pub fn validate_ref(&self) -> GraphResult<()> {
        ensure_unique_names(
            "node type",
            self.node_types
                .iter()
                .map(|node_type| node_type.name.clone()),
        )?;
        ensure_unique_names(
            "edge type",
            self.edge_types
                .iter()
                .map(|edge_type| edge_type.name.clone()),
        )?;

        let mut seen_label_sets = BTreeSet::new();
        for node_type in &self.node_types {
            if node_type.key_labels.is_empty() {
                return Err(GraphError::Inconsistent {
                    reason: format!("node type {} has an empty label set", node_type.name),
                });
            }
            // Why: find_node_type_index uses first-match semantics, so two
            // node types with identical key_labels would leave the second
            // unreachable AND cause edge / property validation to dispatch
            // against the wrong type. Reject ambiguity at type-construction
            // time rather than letting it manifest as silent mis-typing.
            let label_key: Vec<DbString> = node_type.key_labels.iter().cloned().collect();
            if !seen_label_sets.insert(label_key) {
                return Err(GraphError::Inconsistent {
                    reason: format!(
                        "node type {} duplicates the key_labels of an earlier node type",
                        node_type.name
                    ),
                });
            }
            ensure_unique_names(
                "node property",
                node_type
                    .properties
                    .iter()
                    .map(|property| property.name.clone()),
            )?;
            validate_property_element_types(node_type.name.clone(), &node_type.properties)?;
        }

        let node_type_count = self.node_types.len();
        for (index, edge_type) in self.edge_types.iter().enumerate() {
            ensure_endpoint_index(
                node_type_count,
                &edge_type.source_node_type,
                edge_type.name.clone(),
            )?;
            ensure_endpoint_index(
                node_type_count,
                &edge_type.target_node_type,
                edge_type.name.clone(),
            )?;
            ensure_unique_names(
                "edge property",
                edge_type
                    .properties
                    .iter()
                    .map(|property| property.name.clone()),
            )?;
            validate_property_element_types(edge_type.name.clone(), &edge_type.properties)?;
            if self.edge_types[..index].iter().any(|previous| {
                previous.label == edge_type.label
                    && previous
                        .source_node_type
                        .overlaps(&edge_type.source_node_type)
                    && previous
                        .target_node_type
                        .overlaps(&edge_type.target_node_type)
            }) {
                return Err(GraphError::Inconsistent {
                    reason: format!(
                        "ambiguous edge type endpoints ({}, {}, {})",
                        edge_type.label, edge_type.source_node_type, edge_type.target_node_type
                    ),
                });
            }
        }
        Ok(())
    }
}

fn validate_property_element_types(
    type_name: DbString,
    properties: &[PropertyTypeDef],
) -> GraphResult<()> {
    for property in properties {
        let ty = property
            .structural_type()
            .map_err(|source| GraphError::Inconsistent {
                reason: source.to_string(),
            })?;
        if !ty.is_storable_descriptor() {
            return Err(
                selene_core::CoreError::from(selene_core::StoredValueError::QueryOnly {
                    family: "property descriptor",
                })
                .into(),
            );
        }

        if property.decimal_type.is_some() && property.value_type != PropertyValueType::Decimal {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "property {} on type {type_name} declares decimal precision for non-DECIMAL value type {}",
                    property.name, property.value_type
                ),
            });
        }
        if property.character_string_type.is_some()
            && property.value_type != PropertyValueType::String
        {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "property {} on type {type_name} declares character-string length for non-STRING value type {}",
                    property.name, property.value_type
                ),
            });
        }
        if property.byte_string_type.is_some() && property.value_type != PropertyValueType::Bytes {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "property {} on type {type_name} declares byte-string length for non-BYTES value type {}",
                    property.name, property.value_type
                ),
            });
        }
        if property.value_type == PropertyValueType::List {
            // Legacy bare LIST keeps its unconstrained element descriptor,
            // but still validates defaults and storage admission below.
            if let Some(element_type) = property.list_element_type.as_ref() {
                validate_property_element_type(
                    type_name.clone(),
                    property.name.clone(),
                    element_type,
                    1,
                )?;
            }
        } else if property.value_type == PropertyValueType::RecordTyped {
            // Bare RecordTyped is permissive (mirrors legacy untyped LIST): with no
            // declared field structure there is nothing to validate.
            if let Some(fields) = property.record_field_types.as_ref() {
                validate_record_field_types(type_name.clone(), property.name.clone(), fields, 1)?;
            }
        } else if property.list_element_type.is_some() {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "property {} on type {type_name} declares a list element type for non-LIST value type {}",
                    property.name, property.value_type
                ),
            });
        } else if property.record_field_types.is_some() {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "property {} on type {type_name} declares record field types for non-RECORD value type {}",
                    property.name, property.value_type
                ),
            });
        }
        crate::type_validator::validate_property_default(property)?;
    }
    Ok(())
}

fn validate_property_element_type(
    type_name: DbString,
    property_name: DbString,
    element_type: &PropertyElementType,
    depth: u32,
) -> GraphResult<()> {
    if depth > MAX_LIST_TYPE_NESTING {
        return Err(GraphError::Inconsistent {
            reason: format!(
                "property {property_name} on type {type_name} exceeds LIST nesting limit"
            ),
        });
    }
    match element_type {
        PropertyElementType::NotNull(inner) => {
            validate_property_element_type(type_name, property_name, inner, depth)
        }
        PropertyElementType::Scalar(
            PropertyValueType::List | PropertyValueType::Record | PropertyValueType::RecordTyped,
        ) => Err(GraphError::Inconsistent {
            reason: format!(
                "property {property_name} on type {type_name} uses unsupported LIST element type {}",
                element_type.value_type()
            ),
        }),
        PropertyElementType::Scalar(_)
        | PropertyElementType::CharacterString(_)
        | PropertyElementType::Decimal(_)
        | PropertyElementType::ByteString(_) => Ok(()),
        PropertyElementType::List(inner) => {
            validate_property_element_type(type_name, property_name, inner, depth + 1)
        }
    }
}

/// Node-type element.
#[derive(
    Clone,
    Debug,
    Deserialize,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub struct NodeTypeDef {
    /// Node type name.
    pub name: DbString,
    /// Defining label set for this node type.
    pub key_labels: LabelSet,
    /// Declared properties.
    pub properties: Vec<PropertyTypeDef>,
    /// Validation mode for undeclared-property writes.
    pub validation_mode: ValidationMode,
}

/// Edge-type element.
#[derive(
    Clone,
    Debug,
    Deserialize,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub struct EdgeTypeDef {
    /// Edge type name.
    pub name: DbString,
    /// Edge label.
    pub label: DbString,
    /// Source endpoint definition.
    pub source_node_type: EdgeEndpointDef,
    /// Target endpoint definition.
    pub target_node_type: EdgeEndpointDef,
    /// Declared properties.
    pub properties: Vec<PropertyTypeDef>,
    /// Validation mode for undeclared-property writes.
    pub validation_mode: ValidationMode,
}

/// Property declaration for a closed graph type.
#[derive(
    Clone,
    Debug,
    Deserialize,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub struct PropertyTypeDef {
    /// Property name.
    pub name: DbString,
    /// Declared value type.
    pub value_type: PropertyValueType,
    /// Declared element type when [`PropertyTypeDef::value_type`] is `List`.
    pub list_element_type: Option<PropertyElementType>,
    /// `true` means NOT NULL / required.
    pub required: bool,
    /// Default value materialized when the property is omitted on create.
    pub default: Option<PropertyDefaultValue>,
    /// Whether updates to this property are forbidden after creation.
    pub immutable: bool,
    /// Whether non-null property values must be unique within the declaring type.
    pub unique: bool,
    /// Declared decimal precision/scale when [`PropertyTypeDef::value_type`] is
    /// `Decimal`.
    pub decimal_type: Option<DecimalType>,
    /// Declared character-string length when [`PropertyTypeDef::value_type`] is
    /// `String`.
    pub character_string_type: Option<CharacterStringType>,
    /// Declared byte-string length when [`PropertyTypeDef::value_type`] is `Bytes`.
    pub byte_string_type: Option<ByteStringType>,
    /// Declared field types when [`PropertyTypeDef::value_type`] is `RecordTyped`.
    /// `Some` only for closed/typed `RECORD` declarations; `None` for open `Record`
    /// and every non-record value type (symmetric to
    /// [`PropertyTypeDef::list_element_type`]).
    pub record_field_types: Option<RecordFieldTypes>,
}

/// Closed-graph validation mode.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub enum ValidationMode {
    /// Reject undeclared-property writes.
    #[default]
    Strict,
    /// Allow undeclared-property writes and record a warning.
    Warn,
}

/// Behavior of a `DROP NODE TYPE` / `DROP EDGE TYPE` statement when the type
/// still has surviving instances or inbound type dependencies.
///
/// `Restrict` (the default when no behavior keyword is written) is the
/// Seam-B fix from the deletion-reclamation audit (Item 3): dropping a type
/// whose instances still exist would otherwise leave orphan instances whose
/// declared type no longer exists (a silent graph-type-consistency violation
/// on a closed GG02 graph). `Restrict` makes that rejection explicit and early.
/// `Cascade` (selene-db `IM_DROP_CASCADE` vendor extension) truncates the
/// instances first, then drops the type, atomically in one transaction.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum DropBehavior {
    /// Reject the drop when instances or inbound type dependencies remain; the
    /// type is not dropped and no `Change` is recorded (no partial state).
    #[default]
    Restrict,
    /// Truncate every instance of the type first (reusing the
    /// `Mutator::truncate_*` funnel), then drop the type — both in one
    /// transaction.
    Cascade,
}

fn ensure_unique_names(
    kind: &'static str,
    names: impl Iterator<Item = DbString>,
) -> GraphResult<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name.clone()) {
            return Err(GraphError::Inconsistent {
                reason: format!("duplicate {kind} name {name}"),
            });
        }
    }
    Ok(())
}

fn ensure_node_type_index(count: usize, index: u32, edge_name: DbString) -> GraphResult<()> {
    if usize::try_from(index).is_ok_and(|index| index < count) {
        return Ok(());
    }
    Err(GraphError::Inconsistent {
        reason: format!(
            "edge type {edge_name} references node type index {index}, but only {count} node types exist"
        ),
    })
}

fn ensure_endpoint_index(
    count: usize,
    endpoint: &EdgeEndpointDef,
    edge_name: DbString,
) -> GraphResult<()> {
    match endpoint {
        EdgeEndpointDef::Any => Ok(()),
        EdgeEndpointDef::NodeType(index) => ensure_node_type_index(count, *index, edge_name),
        EdgeEndpointDef::OneOf(indices) => {
            // Defense in depth: the `EdgeEndpointDef::one_of` constructor
            // already canonicalizes, but raw struct construction or rkyv/serde
            // decoding can bypass it. Reject malformed OneOf payloads here so
            // a single check at graph-type construction covers every path.
            if indices.len() < 2 {
                return Err(GraphError::Inconsistent {
                    reason: format!(
                        "edge type {edge_name} has a OneOf endpoint with {} indices; OneOf must enumerate at least two distinct node types (singletons must collapse to NodeType)",
                        indices.len()
                    ),
                });
            }
            for window in indices.windows(2) {
                if window[0] >= window[1] {
                    return Err(GraphError::Inconsistent {
                        reason: format!(
                            "edge type {edge_name} has a OneOf endpoint that is not sorted and deduplicated ({}, {})",
                            window[0], window[1]
                        ),
                    });
                }
            }
            for index in indices {
                ensure_node_type_index(count, *index, edge_name.clone())?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "graph_types_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "graph_types_property_default_tests.rs"]
mod property_default_tests;
