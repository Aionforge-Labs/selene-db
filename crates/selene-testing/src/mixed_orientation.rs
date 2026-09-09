//! Independent ISO §16.7 / §22.3 orientation vectors for current and future engines.
//!
//! The constants are hand-worked from the three intrinsic traversal cases, not
//! produced by a matcher. See `docs/gql/mixed-edge-orientation.md` for provenance.

use selene_core::{
    EdgeDirectionality, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, Value, db_string,
};
use selene_graph::SharedGraph;

/// One standard spelling pair and eligible fixture edge offsets from each node.
pub struct OrientationCase {
    /// Full spelling, with `e` available for property predicates.
    pub full: &'static str,
    /// Bracket-free spelling.
    pub abbreviated: &'static str,
    /// Eligible zero-based edge offsets from A.
    pub from_a: &'static [usize],
    /// Eligible zero-based edge offsets from B.
    pub from_b: &'static [usize],
}

/// Seven independently enumerated orientation unions.
pub const ORIENTATIONS: [OrientationCase; 7] = [
    OrientationCase {
        full: "-[e]->",
        abbreviated: "->",
        from_a: &[0, 4, 6],
        from_b: &[1],
    },
    OrientationCase {
        full: "<-[e]-",
        abbreviated: "<-",
        from_a: &[1, 4],
        from_b: &[0, 6],
    },
    OrientationCase {
        full: "~[e]~",
        abbreviated: "~",
        from_a: &[2, 3, 5],
        from_b: &[2, 3],
    },
    OrientationCase {
        full: "<~[e]~",
        abbreviated: "<~",
        from_a: &[1, 2, 3, 4, 5],
        from_b: &[0, 2, 3, 6],
    },
    OrientationCase {
        full: "~[e]~>",
        abbreviated: "~>",
        from_a: &[0, 2, 3, 4, 5, 6],
        from_b: &[1, 2, 3],
    },
    OrientationCase {
        full: "<-[e]->",
        abbreviated: "<->",
        from_a: &[0, 1, 4, 6],
        from_b: &[0, 1, 6],
    },
    OrientationCase {
        full: "-[e]-",
        abbreviated: "-",
        from_a: &[0, 1, 2, 3, 4, 5, 6],
        from_b: &[0, 1, 2, 3, 6],
    },
];

/// Reverse creation, parallel identities, both loop kinds, and an isolated node.
pub struct MixedOrientationFixture {
    /// Graph containing the vectors.
    pub graph: SharedGraph,
    /// A, B, and isolated C (labels A/B/C as well as common N).
    pub nodes: [NodeId; 3],
    /// R, L, U, reverse-created U, directed loop, U loop, parallel R.
    pub edges: Vec<EdgeId>,
}

impl MixedOrientationFixture {
    /// Build through the normal mutator. Each edge has `key = offset` and label E.
    pub fn build() -> Self {
        let graph = SharedGraph::new(GraphId::new(904));
        let mut tx = graph.begin_write();
        let mut m = tx.mutator();
        let nodes = ["A", "B", "C"].map(|label| {
            m.create_node(
                LabelSet::from_iter([db_string(label).unwrap(), db_string("N").unwrap()]),
                PropertyMap::new(),
            )
            .unwrap()
        });
        use EdgeDirectionality::{Directed as D, Undirected as U};
        let edges = [
            (0, 1, D),
            (1, 0, D),
            (0, 1, U),
            (1, 0, U),
            (0, 0, D),
            (0, 0, U),
            (0, 1, D),
        ]
        .into_iter()
        .enumerate()
        .map(|(key, (a, b, direction))| {
            m.create_mixed_edge(
                db_string("E").unwrap(),
                nodes[a],
                nodes[b],
                direction,
                PropertyMap::from_pairs([(db_string("key").unwrap(), Value::Int(key as i64))])
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
        tx.commit().unwrap();
        Self {
            graph,
            nodes,
            edges,
        }
    }
}
