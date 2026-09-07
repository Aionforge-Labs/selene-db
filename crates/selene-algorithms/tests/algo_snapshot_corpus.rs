//! Algorithm snapshot harness — closes M5f per BRIEF-56 (D21 pattern; spec
//! 16 §E31–§E36).
//!
//! Runs every `AlgoCorpusEntry` from the curated `AlgoCorpus::m5f()` corpus
//! through `algo_summary` and asserts the rendered output matches the
//! committed `.snap` golden file. 6 drift tests in addition to the snapshot
//! loop:
//! 1. `corpus_snapshots_match` — per-entry `insta::assert_snapshot!`.
//! 2. `corpus_slugs_are_unique` — collision protection.
//! 3. `corpus_categories_covered` — every `AlgoCorpusCategory` exercised.
//! 4. `corpus_covers_every_algorithm_surface` — every `AlgoSurface` exercised.
//! 5. `algo_surface_mirror_matches_selene_algorithms_exports` — runtime
//!    drift gate on the mirror enum: asserts `AlgoSurface::ALL.len() == 15`
//!    and that surface names are distinct. Combined with the dispatcher's
//!    wildcard panic, a mirror/exports drift either trips the count or
//!    panics on first execution.
//! 6. `corpus_fixture_graphs_build_successfully` — sanity that every
//!    referenced `AlgoCorpusGraph` variant instantiates without error.

use std::collections::BTreeSet;

use selene_algorithms::{
    AlgoResult, AlgoSnapshotInput, ApspConfig, BetweennessConfig, GraphProjection, GraphSummary,
    PageRankConfig, PageRankOrientation, Parallelism, ProjectionConfig, TriangleCountConfig,
    algo_summary, apsp, articulation_points, betweenness, bridges, dijkstra, label_propagation,
    louvain, pagerank, scc, scc_count, sssp, topological_sort, triangle_count, wcc, wcc_count,
};
use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap};
use selene_graph::SharedGraph;
use selene_testing::{
    ALGORITHM_COVERAGE, AlgoCorpus, AlgoCorpusCategory, AlgoCorpusEntry, AlgoCorpusGraph,
    AlgoCorpusInvocation, AlgoSurface,
};

// ---------------------------------------------------------------------------
// Snapshot test
// ---------------------------------------------------------------------------

#[test]
fn corpus_snapshots_match() {
    let corpus = AlgoCorpus::m5f();
    for entry in corpus.entries() {
        let snapshot = execute_entry(entry);
        insta::with_settings!({ snapshot_suffix => entry.slug }, {
            insta::assert_snapshot!(snapshot.to_string());
        });
    }
}

// ---------------------------------------------------------------------------
// Drift tests
// ---------------------------------------------------------------------------

#[test]
fn corpus_slugs_are_unique() {
    let corpus = AlgoCorpus::m5f();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for entry in corpus.entries() {
        assert!(
            seen.insert(entry.slug),
            "duplicate corpus slug {:?}",
            entry.slug
        );
    }
}

#[test]
fn corpus_categories_covered() {
    let corpus = AlgoCorpus::m5f();
    let mut categories: BTreeSet<AlgoCorpusCategory> = BTreeSet::new();
    for entry in corpus.entries() {
        categories.insert(entry.category);
    }
    let expected = [
        AlgoCorpusCategory::Structural,
        AlgoCorpusCategory::Pathfinding,
        AlgoCorpusCategory::Centrality,
        AlgoCorpusCategory::Community,
    ];
    for cat in expected {
        assert!(
            categories.contains(&cat),
            "category {cat:?} has no corpus entries"
        );
    }
}

#[test]
fn corpus_covers_every_algorithm_surface() {
    let corpus = AlgoCorpus::m5f();
    let mut covered: BTreeSet<AlgoSurface> = BTreeSet::new();
    for entry in corpus.entries() {
        // Use the dispatcher-derived surface from the invocation, not just
        // the declared `covered_algorithms` — guards against an entry that
        // lies about what it exercises.
        covered.insert(entry.invocation.surface());
    }
    for surface in ALGORITHM_COVERAGE {
        assert!(
            covered.contains(surface),
            "algorithm surface {surface:?} has no corpus entry exercising it"
        );
    }
}

/// Compile-time anchor table — every algorithm in `selene-algorithms` MUST
/// have an entry here mapping `AlgoSurface` to the dispatcher's stable name.
/// Removal of any algorithm from `selene-algorithms` breaks the `use` import
/// at the top of this file → fail-to-compile signal. Addition of a new
/// algorithm without extending this table is caught by the
/// `algo_surface_mirror_matches_selene_algorithms_exports` count assertion
/// below combined with the `corpus_covers_every_algorithm_surface` test.
///
/// This is the strongest mirror-drift guard achievable for `#[non_exhaustive]`
/// foreign enums (PR #62 Codex P2 finding).
const ALGORITHM_NAME_ANCHOR: &[(AlgoSurface, &str)] = &[
    (AlgoSurface::Wcc, "wcc"),
    (AlgoSurface::WccCount, "wcc_count"),
    (AlgoSurface::Scc, "scc"),
    (AlgoSurface::SccCount, "scc_count"),
    (AlgoSurface::TopologicalSort, "topological_sort"),
    (AlgoSurface::ArticulationPoints, "articulation_points"),
    (AlgoSurface::Bridges, "bridges"),
    (AlgoSurface::Dijkstra, "dijkstra"),
    (AlgoSurface::Sssp, "sssp"),
    (AlgoSurface::Apsp, "apsp"),
    (AlgoSurface::Pagerank, "pagerank"),
    (AlgoSurface::Betweenness, "betweenness"),
    (AlgoSurface::LabelPropagation, "label_propagation"),
    (AlgoSurface::Louvain, "louvain"),
    (AlgoSurface::TriangleCount, "triangle_count"),
];

#[test]
fn algo_surface_mirror_matches_selene_algorithms_exports() {
    // Three-way pinning per spec 16 §E33:
    //
    // (a) The `use selene_algorithms::{...}` import at the top of this file
    //     references every algorithm function by name. Removing one from
    //     `selene-algorithms` fails the import → compile error.
    //
    // (b) `ALGORITHM_NAME_ANCHOR` maps every `AlgoSurface` variant to the
    //     dispatcher's stable name. Mirror count + name alignment is
    //     asserted below; mismatches trip immediately.
    //
    // (c) `corpus_covers_every_algorithm_surface` (separate test) asserts
    //     every surface appears in at least one corpus entry via the
    //     dispatcher.
    //
    // Adding a new algorithm to `selene-algorithms` without (a) updating
    // the use import is caught at compile time. Without (b) extending the
    // anchor + AlgoSurface, the count check below fails. Without (c) adding
    // a corpus entry, the coverage test fails.
    assert_eq!(
        AlgoSurface::ALL.len(),
        ALGORITHM_NAME_ANCHOR.len(),
        "AlgoSurface mirror count drift: {} variants in AlgoSurface::ALL vs \
         {} in ALGORITHM_NAME_ANCHOR. Update both together — see this \
         test's comment for the full three-way pinning contract.",
        AlgoSurface::ALL.len(),
        ALGORITHM_NAME_ANCHOR.len()
    );
    for (i, (surface, name)) in ALGORITHM_NAME_ANCHOR.iter().enumerate() {
        assert_eq!(
            AlgoSurface::ALL[i],
            *surface,
            "AlgoSurface::ALL[{i}] = {:?} but anchor expects {:?}",
            AlgoSurface::ALL[i],
            surface
        );
        assert_eq!(
            surface.name(),
            *name,
            "AlgoSurface::{:?}.name() = {:?} but anchor expects {:?}",
            surface,
            surface.name(),
            name
        );
    }
    // Sanity: every name is distinct.
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for (_, name) in ALGORITHM_NAME_ANCHOR {
        assert!(
            names.insert(*name),
            "duplicate algorithm name {name:?} in ALGORITHM_NAME_ANCHOR"
        );
    }
}

#[test]
fn corpus_fixture_graphs_build_successfully() {
    let corpus = AlgoCorpus::m5f();
    for entry in corpus.entries() {
        let _ = build_graph_and_projection(entry.graph);
    }
}

// ---------------------------------------------------------------------------
// Fixture instantiation
// ---------------------------------------------------------------------------

fn db_string(name: &str) -> DbString {
    selene_core::db_string(name).unwrap()
}

/// Build a `SharedGraph` from an `AlgoCorpusGraph` plus the projection that
/// each algorithm runs against. Returns the projection and the fixture
/// label used in the snapshot header.
fn build_graph_and_projection(graph: AlgoCorpusGraph) -> (GraphProjection, String) {
    let shared = SharedGraph::new(GraphId::new(1));
    let label = db_string("N");
    let rel = db_string("R");
    let mut txn = shared.begin_write();
    let (node_count, edges, fixture_label, scope_opt) = match graph {
        AlgoCorpusGraph::Empty => (0_usize, vec![], "empty".to_string(), None),
        AlgoCorpusGraph::Single => (1, vec![], "single".to_string(), None),
        AlgoCorpusGraph::Chain { len } => {
            let mut edges = Vec::with_capacity(len.saturating_sub(1));
            for i in 0..len.saturating_sub(1) {
                edges.push((i, i + 1));
            }
            (len, edges, format!("chain-{}", len), None)
        }
        AlgoCorpusGraph::K3DirectedCycle => (
            3,
            vec![(0, 1), (1, 2), (2, 0)],
            "k3-directed-cycle".to_string(),
            None,
        ),
        AlgoCorpusGraph::K4Bidir => {
            let pairs = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
            let mut edges = Vec::with_capacity(pairs.len() * 2);
            for (a, b) in pairs {
                edges.push((a, b));
                edges.push((b, a));
            }
            (4, edges, "k4-bidir".to_string(), None)
        }
        AlgoCorpusGraph::StarBidir { spokes } => {
            let mut edges = Vec::with_capacity(spokes * 2);
            for i in 1..=spokes {
                edges.push((0, i));
                edges.push((i, 0));
            }
            (spokes + 1, edges, format!("star-bidir-{}", spokes), None)
        }
        AlgoCorpusGraph::TwoTrianglesBridge => {
            // Bidirectional pairs within two triangles + bridge (2,3).
            let mut edges = Vec::new();
            for (a, b) in [(0, 1), (1, 2), (0, 2), (3, 4), (4, 5), (3, 5), (2, 3)] {
                edges.push((a, b));
                edges.push((b, a));
            }
            (6, edges, "two-triangles-bridge".to_string(), None)
        }
        // `AlgoCorpusGraph` is `#[non_exhaustive]`; the wildcard preserves
        // forward-compat. A new variant without an explicit arm here panics
        // at test runtime — equivalent fail-fast to the dispatcher.
        AlgoCorpusGraph::SparseRowTriangle {
            total_nodes,
            in_scope,
        } => {
            // Triangle edges among in_scope rows.
            let mut edges = Vec::new();
            if in_scope.len() >= 3 {
                let a = in_scope[0] as usize;
                let b = in_scope[1] as usize;
                let c = in_scope[2] as usize;
                edges.push((a, b));
                edges.push((b, c));
                edges.push((c, a));
            }
            (
                total_nodes,
                edges,
                format!(
                    "sparse-row-triangle{{total={},scope={}}}",
                    total_nodes,
                    in_scope.len()
                ),
                Some(in_scope),
            )
        }
        _ => {
            panic!("unmapped AlgoCorpusGraph variant {graph:?} — extend build_graph_and_projection")
        }
    };
    let mut nodes = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let id = txn
            .mutator()
            .create_node(LabelSet::single(label.clone()), PropertyMap::new())
            .unwrap();
        nodes.push(id);
    }
    for (s, t) in edges {
        txn.mutator()
            .create_edge(rel.clone(), nodes[s], nodes[t], PropertyMap::new())
            .unwrap();
    }
    txn.commit().unwrap();
    let snapshot = shared.read();
    let scope_candidates = scope_opt.map(|in_scope| {
        let in_scope_ids = in_scope.iter().map(|&row| nodes[row as usize]);
        snapshot.bind_node_candidates(in_scope_ids).unwrap()
    });
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "snap".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        scope_candidates.as_ref(),
    )
    .unwrap();
    (proj, fixture_label)
}

fn node_id_of_row(row: u32) -> NodeId {
    NodeId::new(u64::from(row) + 1)
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

fn execute_entry(entry: &AlgoCorpusEntry) -> selene_algorithms::AlgoSnapshot {
    let (proj, fixture_label) = build_graph_and_projection(entry.graph);
    let graph_summary = GraphSummary {
        fixture_label: &fixture_label,
        node_count: proj.node_count(),
        edge_count: proj.edge_count(),
    };
    let (algorithm_name, params_str, result_owner) = dispatch(entry.invocation, &proj);
    let result = result_owner.as_result();
    let input = AlgoSnapshotInput {
        graph_summary,
        algorithm: algorithm_name,
        params: &params_str,
        result,
    };
    algo_summary(&input)
}

/// Owned dispatcher output so each call can borrow into the renderer.
enum ResultOwner {
    NodePairs(Vec<(NodeId, u64)>),
    NodePairsWithLevel(Vec<(NodeId, u64, u32)>),
    NodePairsFloat(Vec<(NodeId, f64)>),
    NodePairsUsize(Vec<(NodeId, usize)>),
    TopologicalOrder(Vec<(NodeId, usize)>),
    NodeList(Vec<NodeId>),
    EdgeList(Vec<(NodeId, NodeId)>),
    Count(usize),
    DijkstraPath(Option<selene_algorithms::PathResult>),
    ApspMatrix(Vec<(NodeId, NodeId, f64)>),
    Err(String),
}

impl ResultOwner {
    fn as_result(&self) -> AlgoResult<'_> {
        match self {
            ResultOwner::NodePairs(v) => AlgoResult::NodePairs(v),
            ResultOwner::NodePairsWithLevel(v) => AlgoResult::NodePairsWithLevel(v),
            ResultOwner::NodePairsFloat(v) => AlgoResult::NodePairsFloat(v),
            ResultOwner::NodePairsUsize(v) => AlgoResult::NodePairsUsize(v),
            ResultOwner::TopologicalOrder(v) => AlgoResult::TopologicalOrder(v),
            ResultOwner::NodeList(v) => AlgoResult::NodeList(v),
            ResultOwner::EdgeList(v) => AlgoResult::EdgeList(v),
            ResultOwner::Count(n) => AlgoResult::Count(*n),
            ResultOwner::DijkstraPath(opt) => AlgoResult::DijkstraPath(opt.as_ref()),
            ResultOwner::ApspMatrix(v) => AlgoResult::ApspMatrix(v),
            ResultOwner::Err(s) => AlgoResult::Err(s),
        }
    }
}

/// Dispatch every `AlgoCorpusInvocation` variant to its algorithm call.
///
/// `AlgoCorpusInvocation` is `#[non_exhaustive]` from another crate, which
/// forces a wildcard arm. The `algo_surface_mirror_matches_selene_algorithms_exports`
/// runtime drift test + the wildcard's explicit panic together provide the
/// §E33 fail-fast contract: a new mirror variant without a dispatcher arm
/// either trips the count mismatch (if it ships without a corresponding
/// `AlgoSurface::ALL` extension) or panics on first execution (if it ships
/// with both but no dispatcher arm).
fn dispatch(
    invocation: AlgoCorpusInvocation,
    proj: &GraphProjection,
) -> (&'static str, String, ResultOwner) {
    match invocation {
        AlgoCorpusInvocation::Wcc => ("wcc", String::new(), ResultOwner::NodePairs(wcc(proj))),
        AlgoCorpusInvocation::WccCount => (
            "wcc_count",
            String::new(),
            ResultOwner::Count(wcc_count(proj)),
        ),
        AlgoCorpusInvocation::Scc => ("scc", String::new(), ResultOwner::NodePairs(scc(proj))),
        AlgoCorpusInvocation::SccCount => (
            "scc_count",
            String::new(),
            ResultOwner::Count(scc_count(proj)),
        ),
        AlgoCorpusInvocation::TopologicalSort => {
            let owner = match topological_sort(proj) {
                Ok(v) => ResultOwner::TopologicalOrder(v),
                Err(e) => ResultOwner::Err(short_topo_err(&e)),
            };
            ("topological_sort", String::new(), owner)
        }
        AlgoCorpusInvocation::ArticulationPoints => (
            "articulation_points",
            String::new(),
            ResultOwner::NodeList(articulation_points(proj)),
        ),
        AlgoCorpusInvocation::Bridges => (
            "bridges",
            String::new(),
            ResultOwner::EdgeList(bridges(proj)),
        ),
        AlgoCorpusInvocation::Dijkstra { from_row, to_row } => {
            let from = node_id_of_row(from_row);
            let to = node_id_of_row(to_row);
            let owner = match dijkstra(proj, from, to) {
                Ok(opt) => ResultOwner::DijkstraPath(opt),
                Err(e) => ResultOwner::Err(short_path_err(&e)),
            };
            (
                "dijkstra",
                format!("from=n{} to=n{}", from_row, to_row),
                owner,
            )
        }
        AlgoCorpusInvocation::Sssp { source_row } => {
            let src = node_id_of_row(source_row);
            let owner = match sssp(proj, src) {
                Ok(v) => ResultOwner::NodePairsFloat(v),
                Err(e) => ResultOwner::Err(short_path_err(&e)),
            };
            ("sssp", format!("source=n{}", source_row), owner)
        }
        AlgoCorpusInvocation::Apsp { max_nodes } => {
            let owner = match apsp(
                proj,
                ApspConfig {
                    max_nodes,
                    parallelism: Parallelism::Sequential,
                },
            ) {
                Ok(v) => ResultOwner::ApspMatrix(v),
                Err(e) => ResultOwner::Err(short_path_err(&e)),
            };
            ("apsp", format!("max_nodes={}", max_nodes), owner)
        }
        AlgoCorpusInvocation::Pagerank {
            damping,
            max_iter,
            tolerance,
        } => {
            let config = PageRankConfig {
                damping,
                max_iter,
                tolerance,
                parallelism: Parallelism::Sequential,
                orientation: PageRankOrientation::Natural,
                personalization: None,
            };
            (
                "pagerank",
                format!(
                    "damping={:.2} max_iter={} tolerance={:.6}",
                    damping, max_iter, tolerance
                ),
                ResultOwner::NodePairsFloat(pagerank(proj, config)),
            )
        }
        AlgoCorpusInvocation::Betweenness { sample_size } => {
            let label = match sample_size {
                Some(k) => format!("sample_size={}", k),
                None => "sample_size=full".to_string(),
            };
            (
                "betweenness",
                label,
                ResultOwner::NodePairsFloat(betweenness(
                    proj,
                    BetweennessConfig {
                        sample_size,
                        parallelism: Parallelism::Sequential,
                    },
                )),
            )
        }
        AlgoCorpusInvocation::LabelPropagation { max_iter } => (
            "label_propagation",
            format!("max_iter={}", max_iter),
            ResultOwner::NodePairs(label_propagation(proj, max_iter)),
        ),
        AlgoCorpusInvocation::Louvain { max_iter } => (
            "louvain",
            format!("max_iter={}", max_iter),
            ResultOwner::NodePairsWithLevel(louvain(proj, max_iter)),
        ),
        AlgoCorpusInvocation::TriangleCount => (
            "triangle_count",
            String::new(),
            ResultOwner::NodePairsUsize(triangle_count(
                proj,
                TriangleCountConfig {
                    parallelism: Parallelism::Sequential,
                },
            )),
        ),
        // `AlgoCorpusInvocation` is `#[non_exhaustive]` from an external
        // crate, so a wildcard arm is required. The
        // `#[deny(non_exhaustive_omitted_patterns)]` attribute on the fn
        // raises this branch from a silent fallback to a compile error if
        // a new variant is added without an explicit arm — preserving the
        // §E33 fail-fast contract.
        _ => panic!(
            "unmapped AlgoCorpusInvocation variant {invocation:?} — extend the dispatcher and add a corpus entry"
        ),
    }
}

fn short_topo_err(err: &selene_algorithms::TopoSortError) -> String {
    // Stable short label; the full error message includes a NodeId hint that
    // we don't want in snapshots.
    match err {
        selene_algorithms::TopoSortError::NotADag { .. } => "NotADag".to_string(),
        _ => "TopoSortErrorOther".to_string(),
    }
}

fn short_path_err(err: &selene_algorithms::PathfindingError) -> String {
    use selene_algorithms::PathfindingError;
    match err {
        PathfindingError::NegativeWeight { .. } => "NegativeWeight".to_string(),
        PathfindingError::NaNWeight { .. } => "NaNWeight".to_string(),
        PathfindingError::TooLarge { .. } => "TooLarge".to_string(),
        _ => "PathfindingErrorOther".to_string(),
    }
}
