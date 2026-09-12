//! Independent bounded small-multigraph oracle. Enumerate complete walks first,
//! then split lengths and apply identity restrictions. No production adjacency,
//! automaton transitions, visited sets, or binding helpers are used here.

use super::{PathExecutionLimits, tests::run};
use proptest::prelude::*;
use selene_core::{
    EdgeDirectionality, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, Value, db_string,
};
use selene_graph::SharedGraph;
use std::collections::HashSet;

pub(super) struct Fixture {
    pub(super) graph: SharedGraph,
    pub(super) nodes: Vec<NodeId>,
    pub(super) edges: Vec<(EdgeId, usize, usize, bool, bool)>,
}

impl Fixture {
    pub(super) fn new(nodes: usize, edges: &[(usize, usize, bool, bool)]) -> Self {
        let graph = SharedGraph::new(GraphId::new(5002));
        let mut tx = graph.begin_write();
        let mut m = tx.mutator();
        let ids: Vec<_> = (0..nodes)
            .map(|i| {
                m.create_node(
                    LabelSet::single(db_string(if i == 0 { "Root" } else { "N" }).unwrap()),
                    PropertyMap::new(),
                )
                .unwrap()
            })
            .collect();
        let edges = edges
            .iter()
            .map(|&(a, b, d, k)| {
                let id = m
                    .create_mixed_edge(
                        db_string(if k { "K" } else { "L" }).unwrap(),
                        ids[a],
                        ids[b],
                        if d {
                            EdgeDirectionality::Directed
                        } else {
                            EdgeDirectionality::Undirected
                        },
                        PropertyMap::new(),
                    )
                    .unwrap();
                (id, a, b, d, k)
            })
            .collect();
        tx.commit().unwrap();
        Self {
            graph,
            nodes: ids,
            edges,
        }
    }
}

#[derive(Clone, Copy)]
struct Segment {
    min: usize,
    max: usize,
    orientation: usize,
    label: Option<bool>,
}

// Hand-written acceptance table, independent of EdgeDirection helpers.
const MASKS: [(bool, bool, bool); 7] = [
    (false, false, true),
    (true, false, false),
    (false, true, false),
    (true, true, false),
    (false, true, true),
    (true, false, true),
    (true, true, true),
];
const SPELLINGS: [(&str, &str); 7] = [
    ("-", "->"),
    ("<-", "-"),
    ("~", "~"),
    ("<~", "~"),
    ("~", "~>"),
    ("<-", "->"),
    ("-", "-"),
];

#[derive(Clone)]
struct Walk {
    nodes: Vec<usize>,
    edges: Vec<EdgeId>,
    labels: Vec<bool>,
}

fn enumerate(f: &Fixture, source: usize, max: usize, orientation: usize) -> Vec<Walk> {
    let mut level = vec![Walk {
        nodes: vec![source],
        edges: Vec::new(),
        labels: Vec::new(),
    }];
    let mut all = level.clone();
    let (left, undirected, right) = MASKS[orientation];
    for _ in 0..max {
        let mut next = Vec::new();
        for walk in &level {
            let current = *walk.nodes.last().unwrap();
            for &(id, a, b, d, k) in &f.edges {
                let target = if d {
                    if right && a == current {
                        Some(b)
                    } else if left && b == current {
                        Some(a)
                    } else {
                        None
                    }
                } else if undirected {
                    if a == current {
                        Some(b)
                    } else if b == current {
                        Some(a)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(target) = target {
                    let mut candidate = walk.clone();
                    candidate.nodes.push(target);
                    candidate.edges.push(id);
                    candidate.labels.push(k);
                    next.push(candidate);
                }
            }
        }
        all.extend(next.iter().cloned());
        level = next;
    }
    all
}

fn allowed(walk: &Walk, mode: &str, different: bool) -> bool {
    let edges_unique = walk.edges.iter().collect::<HashSet<_>>().len() == walk.edges.len();
    let nodes_unique = walk.nodes.iter().collect::<HashSet<_>>().len() == walk.nodes.len();
    let interior_unique = walk.nodes[..walk.nodes.len() - 1]
        .iter()
        .collect::<HashSet<_>>()
        .len()
        == walk.nodes.len() - 1;
    (!different || edges_unique)
        && match mode {
            "WALK" => true,
            "TRAIL" => edges_unique,
            "ACYCLIC" => nodes_unique,
            "SIMPLE" => {
                nodes_unique || (walk.nodes.first() == walk.nodes.last() && interior_unique)
            }
            _ => panic!("unknown oracle mode"),
        }
}

fn reference(f: &Fixture, segments: &[Segment], mode: &str, different: bool) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for start in 0..f.nodes.len() {
        // Independently enumerate segment walks then concatenate complete
        // candidates. Production instead walks the product state with pruning.
        let first = segments[0];
        for a in enumerate(f, start, first.max, first.orientation) {
            if a.edges.len() < first.min
                || a.labels
                    .iter()
                    .any(|l| first.label.is_some_and(|k| *l != k))
            {
                continue;
            }
            if segments.len() == 1 {
                if allowed(&a, mode, different) {
                    rows.push(vec![
                        Value::NodeRef(f.nodes[start]),
                        list(&a.edges),
                        Value::NodeRef(f.nodes[*a.nodes.last().unwrap()]),
                    ]);
                }
                continue;
            }
            let second = segments[1];
            let middle = *a.nodes.last().unwrap();
            for b in enumerate(f, middle, second.max, second.orientation) {
                if b.edges.len() < second.min
                    || b.labels
                        .iter()
                        .any(|l| second.label.is_some_and(|k| *l != k))
                {
                    continue;
                }
                let mut joined = a.clone();
                joined.nodes.extend_from_slice(&b.nodes[1..]);
                joined.edges.extend_from_slice(&b.edges);
                if allowed(&joined, mode, different) {
                    rows.push(vec![
                        Value::NodeRef(f.nodes[start]),
                        list(&a.edges),
                        Value::NodeRef(f.nodes[middle]),
                        list(&b.edges),
                        Value::NodeRef(f.nodes[*b.nodes.last().unwrap()]),
                    ]);
                }
            }
        }
    }
    rows
}

pub(super) fn list(edges: &[EdgeId]) -> Value {
    Value::List(edges.iter().copied().map(Value::EdgeRef).collect())
}

// Deterministic presentation carrier only, not a traversal-order assertion.
pub(super) fn canonical(rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut rows: Vec<_> = rows.iter().map(|row| format!("{row:?}")).collect();
    rows.sort();
    rows
}

fn query(segments: &[Segment], mode: &str, match_mode: &str) -> String {
    let mut source = format!("MATCH {match_mode} {mode} (a)");
    for (i, s) in segments.iter().enumerate() {
        let (left, right) = SPELLINGS[s.orientation];
        let name = if i == 0 { "r" } else { "s" };
        let target = if i == 0 && segments.len() == 2 {
            "m"
        } else {
            "b"
        };
        let label = s.label.map_or("", |k| if k { ":K" } else { ":L" });
        source.push_str(&format!(
            "{left}[{name}{label}{{{},{}}}]{right}({target})",
            s.min, s.max
        ));
    }
    source.push_str(" RETURN a");
    source
}

fn compare(f: &Fixture, segments: &[Segment]) {
    for mode in ["WALK", "TRAIL", "SIMPLE", "ACYCLIC"] {
        for match_mode in ["", "REPEATABLE ELEMENTS", "DIFFERENT EDGES"] {
            let source = query(segments, mode, match_mode);
            let actual = run(&f.graph, &source, PathExecutionLimits::default()).unwrap();
            let expected = reference(f, segments, mode, match_mode == "DIFFERENT EDGES");
            assert_eq!(
                canonical(
                    actual
                        .table
                        .rows()
                        .iter()
                        .map(|r| r.values().to_vec())
                        .collect()
                ),
                canonical(expected),
                "{source}"
            );
        }
    }
}

#[test]
fn every_orientation_and_primitive_agrees_on_mixed_parallel_loops() {
    let f = Fixture::new(
        3,
        &[
            (0, 0, true, true),
            (0, 1, false, true),
            (0, 1, false, true),
            (1, 2, true, false),
            (2, 0, true, true),
        ],
    );
    for orientation in 0..7 {
        compare(
            &f,
            &[Segment {
                min: 0,
                max: 3,
                orientation,
                label: None,
            }],
        );
        compare(
            &f,
            &[
                Segment {
                    min: 1,
                    max: 2,
                    orientation,
                    label: Some(true),
                },
                Segment {
                    min: 0,
                    max: 1,
                    orientation: 6,
                    label: None,
                },
            ],
        );
    }
}

#[test]
fn singleton_and_questioned_exposures_match_independent_walks() {
    let f = Fixture::new(
        2,
        &[
            (0, 0, true, true),
            (0, 1, false, true),
            (0, 1, false, true),
            (1, 0, true, false),
        ],
    );
    for (orientation, (left, right)) in SPELLINGS.iter().enumerate() {
        for mode in ["WALK", "TRAIL", "SIMPLE", "ACYCLIC"] {
            for questioned in [false, true] {
                let q = if questioned { "?" } else { "" };
                let source = format!("MATCH {mode} (a){left}[r{q}]{right}(b) RETURN a");
                let actual = run(&f.graph, &source, PathExecutionLimits::default()).unwrap();
                let mut expected = reference(
                    &f,
                    &[Segment {
                        min: usize::from(!questioned),
                        max: 1,
                        orientation,
                        label: None,
                    }],
                    mode,
                    false,
                );
                for row in &mut expected {
                    let Value::List(edges) = &row[1] else {
                        unreachable!()
                    };
                    row[1] = edges.first().cloned().unwrap_or(Value::Null);
                }
                assert_eq!(
                    canonical(
                        actual
                            .table
                            .rows()
                            .iter()
                            .map(|r| r.values().to_vec())
                            .collect()
                    ),
                    canonical(expected),
                    "{source}"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn shrinking_small_multigraph_oracle(
        edges in prop::collection::vec((0usize..3, 0usize..3, any::<bool>(), any::<bool>()), 0..6),
        orientation in 0usize..7,
        first_min in 0usize..2,
        second_min in 0usize..2,
    ) {
        let f = Fixture::new(3, &edges);
        compare(&f, &[Segment { min: first_min, max: 2, orientation, label: Some(true) }, Segment { min: second_min, max: 2, orientation: 6, label: None }]);
    }
}

#[test]
fn walk_adjacent_quantifiers_equal_every_fixed_decomposition() {
    let f = Fixture::new(
        3,
        &[
            (0, 1, true, true),
            (1, 0, true, true),
            (1, 2, true, false),
            (2, 2, true, false),
        ],
    );
    let bounded = run(
        &f.graph,
        "MATCH WALK (a)-[r:K{0,2}]->(m)-[s:L{1,2}]->(b) RETURN a",
        PathExecutionLimits::default(),
    )
    .unwrap();
    let mut decomposed = Vec::new();
    // For every pair of admissible lengths build its unquantified adjacent
    // chain. Extract edge lists independently from singleton columns.
    for k in 0..=2 {
        for l in 1..=2 {
            let mut source = String::from("MATCH WALK (a)");
            for i in 0..k {
                source.push_str(&format!("-[k{i}:K]->(kn{i})"));
            }
            for i in 0..l {
                source.push_str(&format!("-[l{i}:L]->(ln{i})"));
            }
            source.push_str(" RETURN a");
            let result = run(&f.graph, &source, PathExecutionLimits::default()).unwrap();
            for row in result.table.rows() {
                let values = row.values();
                let r = Value::List((0..k).map(|i| values[1 + i * 2].clone()).collect());
                let s = Value::List((0..l).map(|i| values[1 + (k + i) * 2].clone()).collect());
                decomposed.push(vec![
                    values[0].clone(),
                    r,
                    values[k * 2].clone(),
                    s,
                    values[(k + l) * 2].clone(),
                ]);
            }
        }
    }
    assert_eq!(
        canonical(
            bounded
                .table
                .rows()
                .iter()
                .map(|r| r.values().to_vec())
                .collect()
        ),
        canonical(decomposed)
    );
}
