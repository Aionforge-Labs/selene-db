//! Facade-owned closed graph-type definitions.

use selene_core::{LabelSet, db_string};
use selene_graph::{GraphTypeDef, NodeTypeDef, ValidationMode};

use crate::{Error, PathSegment, Result};

mod conversion;
mod property;
pub use property::PropertyDefinition;

/// A named node type with exactly one defining key label (IL003).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeTypeDefinition {
    name: PathSegment,
    labels: Vec<PathSegment>,
    properties: Vec<PropertyDefinition>,
}

impl NodeTypeDefinition {
    /// Construct a node type from validated logical names.
    ///
    /// # Errors
    ///
    /// Returns an invalid-definition error unless `labels` contains exactly
    /// one label.
    pub fn new(name: PathSegment, labels: Vec<PathSegment>) -> Result<Self> {
        if labels.len() != 1 {
            return Err(Error::invalid_graph_type(
                "IL003 requires exactly one node key label",
            ));
        }
        Ok(Self {
            name,
            labels,
            properties: Vec::new(),
        })
    }

    /// Append a validated property. Duplicate names fail when the graph type is built.
    #[must_use]
    pub fn with_property(mut self, property: PropertyDefinition) -> Self {
        self.properties.push(property);
        self
    }
}

/// Endpoint-oriented edge declaration; accepts the engine's existing mixed-edge semantics.
///
/// Endpoint names refer to node-type names, not labels or physical positions.
/// This does not declare a directed-only edge type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EdgeTypeDefinition {
    name: PathSegment,
    label: PathSegment,
    source: PathSegment,
    target: PathSegment,
    properties: Vec<PropertyDefinition>,
}
impl EdgeTypeDefinition {
    /// Declare a named edge between two node types; endpoints are resolved at build.
    #[must_use]
    pub fn new(
        name: PathSegment,
        label: PathSegment,
        source: PathSegment,
        target: PathSegment,
    ) -> Self {
        Self {
            name,
            label,
            source,
            target,
            properties: Vec::new(),
        }
    }
    /// Append a validated property. Duplicate names fail at graph-type build.
    #[must_use]
    pub fn with_property(mut self, property: PropertyDefinition) -> Self {
        self.properties.push(property);
        self
    }
}

/// Validated Rust closed-graph schema, with native properties/defaults and existing rules.
/// This richer Rust API does not enable the GQL catalog grammar's unsupported GG02 subset.
///
/// ```
/// use selene_db::{GraphTypeDefinition, NodeTypeDefinition, PropertyDefinition, PathSegment, Type, Value};
/// let item = PathSegment::regular("Item")?;
/// let definition = GraphTypeDefinition::builder()
///     .with_node_type(NodeTypeDefinition::new(item.clone(), vec![item])?
///         .with_property(PropertyDefinition::new(PathSegment::regular("id")?, Type::INT64.with_nullability(false))?.unique())
///         .with_property(PropertyDefinition::new(PathSegment::regular("active")?, Type::BOOLEAN)?
///             .with_default(Value::Bool(true))?))
///     .build()?;
/// // Install via Catalog::create_graph_type, then create a graph bound to its path.
/// assert!(PropertyDefinition::new(PathSegment::regular("query_only")?, Type::NODE).is_err());
/// assert!(PropertyDefinition::new(PathSegment::regular("bad_default")?, Type::INT64)?
///     .with_default(Value::Bool(true)).is_err());
/// # Ok::<(), selene_db::Error>(())
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphTypeDefinition {
    node_types: Vec<NodeTypeDefinition>,
    edge_types: Vec<EdgeTypeDefinition>,
}

impl GraphTypeDefinition {
    /// Start a graph-type definition.
    #[must_use]
    pub fn builder() -> GraphTypeBuilder {
        GraphTypeBuilder::default()
    }

    pub(crate) fn into_runtime(self, name: &PathSegment) -> Result<GraphTypeDef> {
        let endpoint = |name: &PathSegment| {
            self.node_types
                .iter()
                .position(|n| &n.name == name)
                .and_then(|n| u32::try_from(n).ok())
                .map(selene_graph::EdgeEndpointDef::NodeType)
                .ok_or_else(|| Error::invalid_graph_type("edge endpoint names an absent node type"))
        };
        let edges = self
            .edge_types
            .iter()
            .map(|edge| {
                Ok(selene_graph::EdgeTypeDef {
                    name: db_string(edge.name.display())
                        .map_err(Error::invalid_graph_type_source)?,
                    label: db_string(edge.label.display())
                        .map_err(Error::invalid_graph_type_source)?,
                    source_node_type: endpoint(&edge.source)?,
                    target_node_type: endpoint(&edge.target)?,
                    properties: edge.properties.iter().map(|p| p.0.clone()).collect(),
                    validation_mode: ValidationMode::Strict,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let runtime = GraphTypeDef {
            name: db_string(name.display()).map_err(Error::invalid_graph_type_source)?,
            node_types: self
                .node_types
                .into_iter()
                .map(|node| {
                    let labels = node
                        .labels
                        .into_iter()
                        .map(|label| db_string(label.display()))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(NodeTypeDef {
                        name: db_string(node.name.display())?,
                        key_labels: LabelSet::from_iter(labels),
                        properties: node.properties.into_iter().map(|p| p.0).collect(),
                        validation_mode: ValidationMode::Strict,
                    })
                })
                .collect::<std::result::Result<Vec<_>, selene_core::CoreError>>()
                .map_err(Error::invalid_graph_type_source)?,
            edge_types: edges,
        };
        runtime.validate().map_err(Error::invalid_graph_type_source)
    }
}

/// Builder for a facade-owned closed graph type.
#[derive(Clone, Debug, Default)]
pub struct GraphTypeBuilder {
    node_types: Vec<NodeTypeDefinition>,
    edge_types: Vec<EdgeTypeDefinition>,
}

impl GraphTypeBuilder {
    /// Append one endpoint-oriented mixed edge type.
    #[must_use]
    pub fn with_edge_type(mut self, edge: EdgeTypeDefinition) -> Self {
        self.edge_types.push(edge);
        self
    }
    /// Append one node type.
    #[must_use]
    pub fn with_node_type(mut self, node_type: NodeTypeDefinition) -> Self {
        self.node_types.push(node_type);
        self
    }

    /// Validate and finish the definition.
    ///
    /// # Errors
    ///
    /// Returns an invalid-definition error when no node type was declared or
    /// when node names or defining label sets are duplicated.
    pub fn build(self) -> Result<GraphTypeDefinition> {
        let distinct = |names: Vec<&PathSegment>| {
            names
                .iter()
                .map(|name| name.canonical())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == names.len()
        };
        if !distinct(self.node_types.iter().map(|n| &n.name).collect())
            || !distinct(self.edge_types.iter().map(|e| &e.name).collect())
            || self
                .node_types
                .iter()
                .any(|n| !distinct(n.properties.iter().map(|p| &p.1).collect()))
            || self
                .edge_types
                .iter()
                .any(|e| !distinct(e.properties.iter().map(|p| &p.1).collect()))
        {
            return Err(Error::invalid_graph_type("duplicate canonical schema name"));
        }
        let count = self
            .node_types
            .len()
            .saturating_add(self.edge_types.len())
            .saturating_add(
                self.node_types
                    .iter()
                    .map(|n| n.properties.len())
                    .sum::<usize>(),
            )
            .saturating_add(
                self.edge_types
                    .iter()
                    .map(|e| e.properties.len())
                    .sum::<usize>(),
            );
        if count > 4096 {
            return Err(Error::invalid_graph_type(
                "schema exceeds 4096 declaration entries",
            ));
        }
        if self.node_types.is_empty() {
            return Err(Error::invalid_graph_type(
                "a catalog graph type requires at least one node type",
            ));
        }
        let definition = GraphTypeDefinition {
            node_types: self.node_types,
            edge_types: self.edge_types,
        };
        let runtime = definition.clone().into_runtime(
            &PathSegment::regular("validation")
                .expect("static validation graph-type name is valid"),
        )?;
        let logical = selene_graph::logical_transaction::definition(&runtime)
            .map_err(Error::invalid_graph_type_source)?;
        selene_core::logical::Encoder::new(Default::default())
            .and_then(|mut e| e.graph_definition(&logical))
            .map_err(Error::invalid_graph_type_source)?;
        Ok(definition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn il003_requires_exactly_one_node_key_label() {
        let name = PathSegment::regular("PersonType").unwrap();
        assert!(NodeTypeDefinition::new(name.clone(), Vec::new()).is_err());
        assert!(
            NodeTypeDefinition::new(
                name.clone(),
                vec![
                    PathSegment::regular("Person").unwrap(),
                    PathSegment::regular("Employee").unwrap(),
                ],
            )
            .is_err()
        );
        NodeTypeDefinition::new(name, vec![PathSegment::regular("Person").unwrap()]).unwrap();
    }
}
