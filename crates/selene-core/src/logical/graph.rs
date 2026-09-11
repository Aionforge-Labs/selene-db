//! Graph logical data operations plus complete logical schema and backing identities.
//! SchemaChange legacy events are not a wire layout; their final logical schema and
//! catalog registrations are encoded by the authoring graph/facade adapters instead.

use super::{CodecError as E, CodecResult, Decoder, Encoder, GraphDefinition};
use crate::{
    Change, DbString, EdgeDirectionality, EdgeId, GraphId, LabelDiff, NodeId, PropertyDiff,
    PropertyMap,
};

/// All logical changes for one touched graph in an atomic transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphDelta {
    /// Stable graph identity, bound to its catalog descriptor by the apply owner.
    pub id: GraphId,
    /// Previous graph generation; absent only for a newly created graph.
    pub previous: Option<u64>,
    /// Resulting graph generation.
    pub generation: u64,
    /// Next node ID; includes deleted/published and burned identities.
    pub next_node_id: u64,
    /// Next edge ID; includes deleted/published and burned identities.
    pub next_edge_id: u64,
    /// Complete resulting logical type definition, absent for open graphs.
    pub definition: Option<GraphDefinition>,
    /// Catalog index IDs for actually retained registrations, sorted and unique.
    /// Merely marking an unrelated descriptor Ready does not add it here.
    pub backing_indexes: Vec<u64>,
    /// Ordered data mutations. Schema events are represented by definition/catalog changes.
    pub changes: Vec<Change>,
}

impl GraphDelta {
    /// Encode one graph payload using the whole transaction's shared budget.
    pub fn encode(&self, e: &mut Encoder) -> CodecResult<()> {
        e.u64(self.id.get())?;
        e.boolean(self.previous.is_some())?;
        if let Some(generation) = self.previous {
            e.u64(generation)?;
        }
        e.u64(self.generation)?;
        e.u64(self.next_node_id)?;
        e.u64(self.next_edge_id)?;
        e.boolean(self.definition.is_some())?;
        if let Some(definition) = &self.definition {
            e.graph_definition(definition)?;
        }
        if self.backing_indexes.windows(2).any(|p| p[0] >= p[1]) {
            return Err(E::Invalid("backing identity order"));
        }
        e.count(self.backing_indexes.len())?;
        for id in &self.backing_indexes {
            if *id == 0 {
                return Err(E::Semantic);
            }
            e.u64(*id)?;
        }
        e.count_for::<Change>(self.changes.len())?;
        for change in &self.changes {
            e.graph_change(change)?;
        }
        Ok(())
    }
    /// Decode an entire graph payload before any isolated graph mutation.
    pub fn decode(d: &mut Decoder<'_, '_>) -> CodecResult<Self> {
        let id = GraphId::new(d.nonzero()?);
        let previous = if d.boolean()? { Some(d.u64()?) } else { None };
        let generation = d.u64()?;
        let next_node_id = d.u64()?;
        let next_edge_id = d.u64()?;
        if next_node_id == 0 || next_edge_id == 0 {
            return Err(E::Invalid("element high water"));
        }
        let definition = if d.boolean()? {
            Some(d.graph_definition()?)
        } else {
            None
        };
        let count = d.count()?;
        let mut backing_indexes = Vec::with_capacity(count);
        for _ in 0..count {
            let id = d.u64()?;
            if id == 0
                || backing_indexes
                    .last()
                    .is_some_and(|previous| *previous >= id)
            {
                return Err(E::Invalid("backing identity order"));
            }
            backing_indexes.push(id);
        }
        let count = d.count_for::<Change>()?;
        let mut changes = Vec::with_capacity(count);
        for _ in 0..count {
            changes.push(d.graph_change()?);
        }
        Ok(Self {
            id,
            previous,
            generation,
            next_node_id,
            next_edge_id,
            definition,
            backing_indexes,
            changes,
        })
    }
}

impl Encoder {
    fn properties(&mut self, properties: &PropertyMap) -> CodecResult<()> {
        self.count(properties.len())?;
        for (name, value) in properties.iter() {
            self.text(name.as_str())?;
            self.value(value, 1)?;
        }
        Ok(())
    }
    fn names(&mut self, names: &[DbString]) -> CodecResult<()> {
        if names.windows(2).any(|p| p[0] >= p[1]) {
            return Err(E::Invalid("name order"));
        }
        self.count(names.len())?;
        for name in names {
            self.text(name.as_str())?;
        }
        Ok(())
    }
    fn property_diff(&mut self, diff: &PropertyDiff) -> CodecResult<()> {
        if diff.set.windows(2).any(|p| p[0].0 >= p[1].0)
            || diff
                .set
                .iter()
                .any(|(key, _)| diff.removed.binary_search(key).is_ok())
        {
            return Err(E::Invalid("property diff order/overlap"));
        }
        self.count(diff.set.len())?;
        for (key, value) in &diff.set {
            self.text(key.as_str())?;
            self.value(value, 1)?;
        }
        self.names(&diff.removed)
    }
    fn graph_change(&mut self, change: &Change) -> CodecResult<()> {
        match change {
            Change::NodeCreated {
                id,
                labels,
                properties,
            } => {
                self.u8(1)?;
                self.u64(id.get())?;
                self.labels(labels)?;
                self.properties(properties)
            }
            Change::NodeUpdated {
                id,
                labels_diff,
                properties_diff,
            } => {
                self.u8(2)?;
                self.u64(id.get())?;
                if labels_diff
                    .added
                    .iter()
                    .any(|key| labels_diff.removed.binary_search(key).is_ok())
                {
                    return Err(E::Invalid("label overlap"));
                }
                self.names(&labels_diff.added)?;
                self.names(&labels_diff.removed)?;
                self.property_diff(properties_diff)
            }
            Change::NodeDeleted { id } => {
                self.u8(3)?;
                self.u64(id.get())
            }
            Change::EdgeCreated {
                id,
                directionality,
                label,
                source,
                target,
                properties,
            } => {
                if directionality.canonical_endpoints(*source, *target) != (*source, *target) {
                    return Err(E::Invalid("undirected endpoints"));
                }
                self.u8(4)?;
                self.u64(id.get())?;
                self.u8(match directionality {
                    EdgeDirectionality::Directed => 1,
                    EdgeDirectionality::Undirected => 2,
                })?;
                self.text(label.as_str())?;
                self.u64(source.get())?;
                self.u64(target.get())?;
                self.properties(properties)
            }
            Change::EdgeUpdated {
                id,
                properties_diff,
            } => {
                self.u8(5)?;
                self.u64(id.get())?;
                self.property_diff(properties_diff)
            }
            Change::EdgeDeleted { id } => {
                self.u8(6)?;
                self.u64(id.get())
            }
            Change::NodePropertyRemoved { id, property } => {
                self.u8(7)?;
                self.u64(id.get())?;
                self.text(property.as_str())
            }
            Change::EdgePropertyRemoved { id, property } => {
                self.u8(8)?;
                self.u64(id.get())?;
                self.text(property.as_str())
            }
            Change::NodeLabelRemoved { id, label } => {
                self.u8(9)?;
                self.u64(id.get())?;
                self.text(label.as_str())
            }
            Change::NodesOfTypeTruncated { label } => {
                self.u8(10)?;
                self.text(label.as_str())
            }
            Change::EdgesOfTypeTruncated { label } => {
                self.u8(11)?;
                self.text(label.as_str())
            }
            Change::GraphReset {} => self.u8(12),
            Change::SchemaChanged { .. } => {
                Err(E::Invalid("schema event requires logical metadata adapter"))
            }
        }
    }
}
impl Decoder<'_, '_> {
    fn nonzero(&mut self) -> CodecResult<u64> {
        let id = self.u64()?;
        if id == 0 { Err(E::Semantic) } else { Ok(id) }
    }
    fn node_id(&mut self) -> CodecResult<NodeId> {
        Ok(NodeId::new(self.nonzero()?))
    }
    fn edge_id(&mut self) -> CodecResult<EdgeId> {
        Ok(EdgeId::new(self.nonzero()?))
    }
    fn names(&mut self) -> CodecResult<Vec<DbString>> {
        let count = self.count()?;
        let mut names = Vec::with_capacity(count);
        for _ in 0..count {
            let name = self.name()?;
            if names.last().is_some_and(|p| p >= &name) {
                return Err(E::Invalid("name order"));
            }
            names.push(name);
        }
        Ok(names)
    }
    fn properties(&mut self) -> CodecResult<PropertyMap> {
        let count = self.count()?;
        let mut map = PropertyMap::new();
        let mut previous = None;
        for _ in 0..count {
            let key = self.name()?;
            if previous.as_ref().is_some_and(|p| p >= &key) {
                return Err(E::Invalid("property order"));
            }
            previous = Some(key.clone());
            map.set(key, self.value(1)?).map_err(|_| E::Semantic)?;
        }
        Ok(map)
    }
    fn property_diff(&mut self) -> CodecResult<PropertyDiff> {
        let map = self.properties()?;
        let removed = self.names()?;
        PropertyDiff::new(
            map.iter().map(|(key, value)| (key.clone(), value.clone())),
            removed,
        )
        .map_err(|_| E::Semantic)
    }
    fn graph_change(&mut self) -> CodecResult<Change> {
        Ok(match self.u8()? {
            1 => Change::NodeCreated {
                id: self.node_id()?,
                labels: self.labels()?,
                properties: self.properties()?,
            },
            2 => Change::NodeUpdated {
                id: self.node_id()?,
                labels_diff: LabelDiff::new(self.names()?, self.names()?)
                    .map_err(|_| E::Semantic)?,
                properties_diff: self.property_diff()?,
            },
            3 => Change::NodeDeleted {
                id: self.node_id()?,
            },
            4 => {
                let id = self.edge_id()?;
                let directionality = match self.u8()? {
                    1 => EdgeDirectionality::Directed,
                    2 => EdgeDirectionality::Undirected,
                    _ => return Err(E::Invalid("directionality")),
                };
                let label = self.name()?;
                let source = self.node_id()?;
                let target = self.node_id()?;
                if directionality.canonical_endpoints(source, target) != (source, target) {
                    return Err(E::Invalid("undirected endpoints"));
                }
                Change::EdgeCreated {
                    id,
                    directionality,
                    label,
                    source,
                    target,
                    properties: self.properties()?,
                }
            }
            5 => Change::EdgeUpdated {
                id: self.edge_id()?,
                properties_diff: self.property_diff()?,
            },
            6 => Change::EdgeDeleted {
                id: self.edge_id()?,
            },
            7 => Change::NodePropertyRemoved {
                id: self.node_id()?,
                property: self.name()?,
            },
            8 => Change::EdgePropertyRemoved {
                id: self.edge_id()?,
                property: self.name()?,
            },
            9 => Change::NodeLabelRemoved {
                id: self.node_id()?,
                label: self.name()?,
            },
            10 => Change::NodesOfTypeTruncated {
                label: self.name()?,
            },
            11 => Change::EdgesOfTypeTruncated {
                label: self.name()?,
            },
            12 => Change::GraphReset {},
            _ => return Err(E::Invalid("graph operation tag")),
        })
    }
}
