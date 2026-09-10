//! Supported facade values with opaque ownership-bearing references.
//!
//! This is a query value contract, not a serialization format. References and
//! paths are query-only; storage admission is enforced recursively by the engine.

use crate::{DatabaseId, EdgeRef, Error, GraphId, GraphRef, NodeRef, Result};
use std::sync::Arc;

/// One owned query parameter or result value.
///
/// Reference variants carry database and graph identity. There is deliberately
/// no conversion from arbitrary lower runtime values and no serialization impl.
/// Rust equality preserves scalar representations, treats NaNs of the same
/// representation as equal, and matches record fields by name. Query predicate,
/// grouping and ordering semantics are separate operations in the engine.
///
/// ```compile_fail
/// let value = selene_db::Value::NodeRef(selene_db::NodeId::new(1));
/// ```
///
/// ```compile_fail
/// let value: selene_db::Value = selene_core::Value::Int(1);
/// ```
///
/// ```compile_fail
/// let value = selene_db::Value::TableRef(1);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Value {
    /// Boolean value.
    Bool(bool),
    /// Signed 64-bit integer.
    Int(i64),
    /// Unsigned 64-bit integer.
    Uint(u64),
    /// Signed 128-bit integer.
    Int128(i128),
    /// Unsigned 128-bit integer.
    Uint128(u128),
    /// IEEE binary64 number.
    Float(f64),
    /// IEEE binary32 number.
    Float32(f32),
    /// Exact fixed-precision decimal.
    Decimal(selene_core::Decimal),
    /// Validated string.
    String(selene_core::DbString),
    /// Byte string.
    Bytes(Arc<[u8]>),
    /// Zoned datetime.
    ZonedDateTime(Box<jiff::Zoned>),
    /// Local datetime.
    LocalDateTime(jiff::civil::DateTime),
    /// Date.
    Date(jiff::civil::Date),
    /// Zoned time.
    ZonedTime(Box<jiff::Zoned>),
    /// Local time.
    LocalTime(jiff::civil::Time),
    /// Temporal duration.
    Duration(Box<jiff::Span>),
    /// UUID.
    Uuid(selene_core::Uuid),
    /// Finite native dense vector.
    Vector(selene_core::VectorValue),
    /// Validated canonical JSON.
    Json(selene_core::JsonValue),
    /// Null value, distinct from an omitted result or an empty table.
    Null,
    /// Recursive list of query values.
    List(Vec<Value>),
    /// Named record; field names carry meaning independently of field order.
    Record(Box<Record>),
    /// Opaque database- and graph-owned node reference.
    NodeRef(NodeRef),
    /// Opaque database- and graph-owned edge reference.
    EdgeRef(EdgeRef),
    /// Opaque database-owned graph reference.
    GraphRef(GraphRef),
    /// Opaque graph-owned path with reference identity retained after deletion.
    Path(Box<Path>),
}

/// Named record value. No positional descriptor-arena identity is exposed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Record {
    /// Named fields in presentation order.
    Open(Vec<(selene_core::DbString, Value)>),
}

impl PartialEq for Record {
    fn eq(&self, rhs: &Self) -> bool {
        let (Self::Open(lhs), Self::Open(rhs)) = (self, rhs);
        if lhs.len() != rhs.len() {
            return false;
        }
        let mut lhs: Vec<_> = lhs.iter().collect();
        let mut rhs: Vec<_> = rhs.iter().collect();
        lhs.sort_by(|a, b| a.0.cmp(&b.0));
        rhs.sort_by(|a, b| a.0.cmp(&b.0));
        lhs == rhs
    }
}

/// An engine-issued query path. Cloning does not access its referents.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    graph: GraphRef,
    start: NodeRef,
    segments: Vec<PathSegment>,
}

/// One engine-issued step in a query path.
#[derive(Clone, Debug, PartialEq)]
pub struct PathSegment {
    edge: EdgeRef,
    direction: selene_core::EdgeDirection,
    node: NodeRef,
}

impl Path {
    /// Owning database and graph.
    #[must_use]
    pub const fn graph(&self) -> GraphRef {
        self.graph
    }
    /// Starting node reference.
    #[must_use]
    pub const fn start(&self) -> NodeRef {
        self.start
    }
    /// Ordered traversal steps.
    #[must_use]
    pub fn segments(&self) -> &[PathSegment] {
        &self.segments
    }
}

impl PathSegment {
    /// Describe a step for validation by [`crate::Session::path_reference`].
    #[must_use]
    pub const fn new(edge: EdgeRef, direction: selene_core::EdgeDirection, node: NodeRef) -> Self {
        Self {
            edge,
            direction,
            node,
        }
    }
    /// Traversed edge reference.
    #[must_use]
    pub const fn edge(&self) -> EdgeRef {
        self.edge
    }
    /// Traversal direction.
    #[must_use]
    pub const fn direction(&self) -> selene_core::EdgeDirection {
        self.direction
    }
    /// Reached node reference.
    #[must_use]
    pub const fn node(&self) -> NodeRef {
        self.node
    }
}

impl crate::Session {
    /// Validate a path's ownership, live elements, direction, and connectivity.
    /// The resulting immutable path remains copyable after an element is deleted.
    ///
    /// # Errors
    /// Returns `42002` for foreign ownership or malformed topology, and `22G11`
    /// when construction would access a deleted referent.
    pub fn path_reference(&self, start: NodeRef, segments: Vec<PathSegment>) -> Result<Path> {
        self.ensure_selected(start.database_id(), start.graph_id())?;
        for step in &segments {
            self.ensure_selected(step.edge.database_id(), step.edge.graph_id())?;
            self.ensure_selected(step.node.database_id(), step.node.graph_id())?;
        }
        self.inner
            .with_reference_graph(start.graph_id(), |runtime| {
                let snapshot = runtime.read();
                let mut current = start.node_id();
                if !snapshot.is_node_alive(current) {
                    return Err(deleted_reference());
                }
                for step in &segments {
                    if !snapshot.is_node_alive(step.node.node_id())
                        || !snapshot.is_edge_alive(step.edge.edge_id())
                    {
                        return Err(deleted_reference());
                    }
                    let (source, target) = snapshot
                        .edge_endpoints(step.edge.edge_id())
                        .ok_or_else(deleted_reference)?;
                    let connected = match (
                        snapshot.edge_directionality(step.edge.edge_id()),
                        step.direction,
                    ) {
                        (
                            Some(selene_core::EdgeDirectionality::Directed),
                            selene_core::EdgeDirection::Outgoing,
                        ) => source == current && target == step.node.node_id(),
                        (
                            Some(selene_core::EdgeDirectionality::Directed),
                            selene_core::EdgeDirection::Incoming,
                        ) => target == current && source == step.node.node_id(),
                        (
                            Some(selene_core::EdgeDirectionality::Undirected),
                            selene_core::EdgeDirection::Undirected,
                        ) => {
                            (source == current && target == step.node.node_id())
                                || (target == current && source == step.node.node_id())
                        }
                        _ => false,
                    };
                    if !connected {
                        return Err(Error::invalid_runtime_reference(
                            "path traversal is not connected in the requested direction",
                        ));
                    }
                    current = step.node.node_id();
                }
                Ok(())
            })?;
        Ok(Path {
            graph: GraphRef::new(start.database_id(), start.graph_id()),
            start,
            segments,
        })
    }
}

pub(crate) fn deleted_reference() -> Error {
    Error::from_engine(selene_gql::ExecutorError::DataException {
        subclass: selene_gql::DataExceptionSubclass::InvalidReferenceValue,
        message: "referenced graph element is absent or no longer alive".to_owned(),
        span: selene_gql::SourceSpan::default(),
    })
}

impl Value {
    // Checked before recursive conversion or cloning at the public boundary.
    pub(crate) fn validate_shape(&self) -> Result<()> {
        let mut pending = vec![(self, 1)];
        while let Some((value, depth)) = pending.pop() {
            if depth > selene_core::MAX_STORED_VALUE_DEPTH {
                return Err(value_error("query value nesting limit exceeded"));
            }
            match value {
                Self::List(values) => pending.extend(values.iter().map(|v| (v, depth + 1))),
                Self::Record(record) => {
                    let Record::Open(fields) = record.as_ref();
                    let mut names = std::collections::BTreeSet::new();
                    for (name, value) in fields {
                        if !names.insert(name) {
                            return Err(value_error("duplicate query record field"));
                        }
                        pending.push((value, depth + 1));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn reference_graph(&self, database: DatabaseId) -> Result<Option<GraphId>> {
        let mut domain = None;
        let mut pending = vec![self];
        while let Some(value) = pending.pop() {
            let reference = match value {
                Self::NodeRef(r) => Some((r.database_id(), r.graph_id())),
                Self::EdgeRef(r) => Some((r.database_id(), r.graph_id())),
                Self::GraphRef(r) => Some((r.database_id(), r.graph_id())),
                Self::Path(p) => Some((p.graph.database_id(), p.graph.graph_id())),
                Self::List(values) => {
                    pending.extend(values);
                    None
                }
                Self::Record(record) => {
                    let Record::Open(fields) = record.as_ref();
                    pending.extend(fields.iter().map(|(_, value)| value));
                    None
                }
                _ => None,
            };
            if let Some((owner, graph)) = reference {
                if owner != database {
                    return Err(Error::invalid_runtime_reference(
                        "reference belongs to another database instance",
                    ));
                }
                if domain.is_some_and(|previous| previous != graph) {
                    return Err(Error::invalid_runtime_reference(
                        "reference belongs to another graph",
                    ));
                }
                domain = Some(graph);
            }
        }
        Ok(domain)
    }

    pub(crate) fn to_lower(&self) -> selene_core::Value {
        use selene_core::Value as Lower;
        match self {
            Self::Bool(v) => Lower::Bool(*v),
            Self::Int(v) => Lower::Int(*v),
            Self::Uint(v) => Lower::Uint(*v),
            Self::Int128(v) => Lower::Int128(*v),
            Self::Uint128(v) => Lower::Uint128(*v),
            Self::Float(v) => Lower::Float(*v),
            Self::Float32(v) => Lower::Float32(*v),
            Self::Decimal(v) => Lower::Decimal(*v),
            Self::String(v) => Lower::String(v.clone()),
            Self::Bytes(v) => Lower::Bytes(v.clone()),
            Self::ZonedDateTime(v) => Lower::ZonedDateTime(v.clone()),
            Self::LocalDateTime(v) => Lower::LocalDateTime(*v),
            Self::Date(v) => Lower::Date(*v),
            Self::ZonedTime(v) => Lower::ZonedTime(v.clone()),
            Self::LocalTime(v) => Lower::LocalTime(*v),
            Self::Duration(v) => Lower::Duration(v.clone()),
            Self::Uuid(v) => Lower::Uuid(*v),
            Self::Vector(v) => Lower::Vector(v.clone()),
            Self::Json(v) => Lower::Json(v.clone()),
            Self::Null => Lower::Null,
            Self::List(values) => Lower::List(values.iter().map(Self::to_lower).collect()),
            Self::Record(record) => {
                let Record::Open(fields) = record.as_ref();
                Lower::Record(Box::new(selene_core::Record::Open(
                    fields
                        .iter()
                        .map(|(k, v)| (k.clone(), v.to_lower()))
                        .collect(),
                )))
            }
            Self::NodeRef(r) => Lower::NodeRef(r.node_id()),
            Self::EdgeRef(r) => Lower::EdgeRef(r.edge_id()),
            Self::GraphRef(r) => Lower::GraphRef(selene_core::GraphId::new(r.graph_id().get())),
            Self::Path(p) => Lower::Path(Box::new(selene_core::Path {
                graph: selene_core::GraphId::new(p.graph.graph_id().get()),
                start: p.start.node_id(),
                segments: p
                    .segments
                    .iter()
                    .map(|s| selene_core::PathSegment {
                        edge: s.edge.edge_id(),
                        direction: s.direction,
                        node: s.node.node_id(),
                    })
                    .collect(),
            })),
        }
    }

    pub(crate) fn from_lower(value: &selene_core::Value, graph: GraphRef) -> Result<Self> {
        if !selene_core::StructuralType::DYNAMIC.matches(value) {
            return Err(value_error("unsupported query result value shape"));
        }
        Self::from_lower_at(value, graph, 1)
    }

    fn from_lower_at(value: &selene_core::Value, graph: GraphRef, depth: usize) -> Result<Self> {
        use selene_core::Value as Lower;
        if depth > selene_core::MAX_STORED_VALUE_DEPTH {
            return Err(value_error("query result nesting limit exceeded"));
        }
        Ok(match value {
            Lower::Bool(v) => Self::Bool(*v),
            Lower::Int(v) => Self::Int(*v),
            Lower::Uint(v) => Self::Uint(*v),
            Lower::Int128(v) => Self::Int128(*v),
            Lower::Uint128(v) => Self::Uint128(*v),
            Lower::Float(v) => Self::Float(*v),
            Lower::Float32(v) => Self::Float32(*v),
            Lower::Decimal(v) => Self::Decimal(*v),
            Lower::String(v) => Self::String(v.clone()),
            Lower::Bytes(v) => Self::Bytes(v.clone()),
            Lower::ZonedDateTime(v) => Self::ZonedDateTime(v.clone()),
            Lower::LocalDateTime(v) => Self::LocalDateTime(*v),
            Lower::Date(v) => Self::Date(*v),
            Lower::ZonedTime(v) => Self::ZonedTime(v.clone()),
            Lower::LocalTime(v) => Self::LocalTime(*v),
            Lower::Duration(v) => Self::Duration(v.clone()),
            Lower::Uuid(v) => Self::Uuid(*v),
            Lower::Vector(v) => Self::Vector(v.clone()),
            Lower::Json(v) => Self::Json(v.clone()),
            Lower::Null => Self::Null,
            Lower::List(values) => Self::List(
                values
                    .iter()
                    .map(|v| Self::from_lower_at(v, graph, depth + 1))
                    .collect::<Result<_>>()?,
            ),
            Lower::Record(record) => {
                let selene_core::Record::Open(fields) = record.as_ref() else {
                    return Err(value_error("unsupported result record shape"));
                };
                Self::Record(Box::new(Record::Open(
                    fields
                        .iter()
                        .map(|(k, v)| Ok((k.clone(), Self::from_lower_at(v, graph, depth + 1)?)))
                        .collect::<Result<_>>()?,
                )))
            }
            Lower::NodeRef(id) => {
                Self::NodeRef(NodeRef::new(graph.database_id(), graph.graph_id(), *id))
            }
            Lower::EdgeRef(id) => {
                Self::EdgeRef(EdgeRef::new(graph.database_id(), graph.graph_id(), *id))
            }
            Lower::GraphRef(id) if id.get() == graph.graph_id().get() => Self::GraphRef(graph),
            Lower::Path(p) if p.graph.get() == graph.graph_id().get() => {
                Self::Path(Box::new(Path {
                    graph,
                    start: NodeRef::new(graph.database_id(), graph.graph_id(), p.start),
                    segments: p
                        .segments
                        .iter()
                        .map(|s| PathSegment {
                            edge: EdgeRef::new(graph.database_id(), graph.graph_id(), s.edge),
                            direction: s.direction,
                            node: NodeRef::new(graph.database_id(), graph.graph_id(), s.node),
                        })
                        .collect(),
                }))
            }
            _ => return Err(value_error("unsupported or foreign runtime result value")),
        })
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Uint(a), Self::Uint(b)) => a == b,
            (Self::Int128(a), Self::Int128(b)) => a == b,
            (Self::Uint128(a), Self::Uint128(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Self::Float32(a), Self::Float32(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Self::Decimal(a), Self::Decimal(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::ZonedDateTime(a), Self::ZonedDateTime(b)) => a == b,
            (Self::LocalDateTime(a), Self::LocalDateTime(b)) => a == b,
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::ZonedTime(a), Self::ZonedTime(b)) => a == b,
            (Self::LocalTime(a), Self::LocalTime(b)) => a == b,
            (Self::Duration(a), Self::Duration(b)) => a.fieldwise() == b.fieldwise(),
            (Self::Uuid(a), Self::Uuid(b)) => a == b,
            (Self::Vector(a), Self::Vector(b)) => a == b,
            (Self::Json(a), Self::Json(b)) => a == b,
            (Self::Null, Self::Null) => true,
            (Self::List(a), Self::List(b)) => a == b,
            (Self::Record(a), Self::Record(b)) => a == b,
            (Self::Path(a), Self::Path(b)) => a == b,
            (Self::NodeRef(a), Self::NodeRef(b)) => a == b,
            (Self::EdgeRef(a), Self::EdgeRef(b)) => a == b,
            (Self::GraphRef(a), Self::GraphRef(b)) => a == b,
            _ => false,
        }
    }
}

fn value_error(message: &str) -> Error {
    Error::from_engine(selene_gql::ExecutorError::DataException {
        subclass: selene_gql::DataExceptionSubclass::InvalidValueType,
        message: message.to_owned(),
        span: selene_gql::SourceSpan::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facade_conversion_rejects_legacy_query_families_recursively() {
        use selene_core::Value as Lower;
        let graph = GraphRef::new(DatabaseId::from_raw(1), GraphId(1));
        for value in [
            Lower::RecordTyped(Box::new(selene_core::RecordTyped {
                type_id: selene_core::RecordTypeId::new(1),
                values: [Some(Lower::Int(1))].into_iter().collect(),
            })),
            Lower::Extended {
                type_id: selene_core::ExtensionTypeId::FIRST_PARTY_MIN,
                payload: Arc::from([1_u8]),
            },
        ] {
            for value in [value.clone(), Lower::List(vec![value])] {
                assert_eq!(
                    Value::from_lower(&value, graph)
                        .unwrap_err()
                        .gqlstatus()
                        .unwrap()
                        .as_str(),
                    "22G03"
                );
            }
        }
    }
}
