//! Intrinsic edge state, independent of traversal orientation and physical rows.

use serde::{Deserialize, Serialize};

use crate::{DbString, EdgeId, NodeId, PropertyMap};

/// Intrinsic directionality of one edge identity.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Hash,
    Deserialize,
    Serialize,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
)]
pub enum EdgeDirectionality {
    /// An ordered source and destination.
    Directed,
    /// An unordered pair of endpoints, not two directed edges.
    Undirected,
}

impl EdgeDirectionality {
    /// Canonicalize endpoint storage without assigning traversal orientation.
    #[must_use]
    pub fn canonical_endpoints(self, first: NodeId, second: NodeId) -> (NodeId, NodeId) {
        if self == Self::Undirected && second < first {
            (second, first)
        } else {
            (first, second)
        }
    }
}

/// Version-one logical edge reconstruction record.
///
/// This is a semantic input to storage, not a commitment to an archive or WAL
/// byte layout. F02 owns the persistence encoding. Undirected endpoints are
/// canonically ordered by ID; neither is a semantic source or destination.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeRecordV1 {
    /// Stable identity, shared by every incidence of this edge.
    pub id: EdgeId,
    /// Edge label.
    pub label: DbString,
    /// Intrinsic edge directionality.
    pub directionality: EdgeDirectionality,
    /// Source for a directed edge; first canonical endpoint otherwise.
    pub first: NodeId,
    /// Destination for a directed edge; second canonical endpoint otherwise.
    pub second: NodeId,
    /// Native edge properties.
    pub properties: PropertyMap,
}

impl From<EdgeRecordV1> for crate::Change {
    fn from(record: EdgeRecordV1) -> Self {
        Self::EdgeCreated {
            id: record.id,
            label: record.label,
            directionality: record.directionality,
            source: record.first,
            target: record.second,
            properties: record.properties,
        }
    }
}
