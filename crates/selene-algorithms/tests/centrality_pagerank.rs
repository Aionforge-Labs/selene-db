//! Integration tests for `pagerank` per spec 16 §E19–§E23.

use selene_algorithms::{
    GraphProjection, PageRankConfig, PageRankOrientation, Parallelism, ProjectionConfig, pagerank,
};
use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap};
use selene_graph::SharedGraph;

fn db_string(name: &str) -> DbString {
    selene_core::db_string(name).unwrap()
}

fn build_proj(shared: &SharedGraph) -> GraphProjection {
    let snapshot = shared.read();
    GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "test".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        None,
    )
    .unwrap()
}

fn build_graph(count: usize, edges: &[(usize, usize)]) -> (SharedGraph, Vec<NodeId>) {
    let shared = SharedGraph::new(GraphId::new(1));
    let label = db_string("N");
    let rel = db_string("R");
    let mut txn = shared.begin_write();
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        let id = txn
            .mutator()
            .create_node(LabelSet::single(label.clone()), PropertyMap::new())
            .unwrap();
        nodes.push(id);
    }
    for &(s, t) in edges {
        txn.mutator()
            .create_edge(rel.clone(), nodes[s], nodes[t], PropertyMap::new())
            .unwrap();
    }
    txn.commit().unwrap();
    (shared, nodes)
}

fn default_config() -> PageRankConfig {
    PageRankConfig {
        damping: 0.85,
        max_iter: 100,
        tolerance: 1e-6,
        parallelism: Parallelism::Sequential,
        orientation: PageRankOrientation::Natural,
        personalization: None,
    }
}

#[test]
fn pagerank_empty_projection_returns_empty() {
    let shared = SharedGraph::new(GraphId::new(1));
    let proj = build_proj(&shared);
    assert!(pagerank(&proj, default_config()).is_empty());
}

#[test]
fn pagerank_single_isolated_node_score_one() {
    let (shared, nodes) = build_graph(1, &[]);
    let proj = build_proj(&shared);
    let result = pagerank(&proj, default_config());
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, nodes[0]);
    assert!(
        (result[0].1 - 1.0).abs() < 1e-9,
        "single node converges to score 1.0"
    );
}

#[test]
fn pagerank_max_iter_zero_returns_initial_uniform_scores() {
    // §O.11 — max_iter == 0 means zero iterations; scores stay at 1/N initial.
    let (shared, _) = build_graph(3, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let cfg = PageRankConfig {
        damping: 0.85,
        max_iter: 0,
        tolerance: 1e-9,
        parallelism: Parallelism::Sequential,
        orientation: PageRankOrientation::Natural,
        personalization: None,
    };
    let result = pagerank(&proj, cfg);
    assert_eq!(result.len(), 3);
    let expected_uniform = 1.0 / 3.0;
    for &(nid, score) in &result {
        assert!(
            (score - expected_uniform).abs() < 1e-12,
            "node {nid:?} should have initial 1/N score with max_iter=0"
        );
    }
}

#[test]
fn pagerank_three_cycle_all_equal_scores() {
    // 3-cycle: n0 → n1 → n2 → n0. By symmetry all three converge to 1/3.
    // Tie-break (§E21) MUST sort ASC by NodeId on equal scores.
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let result = pagerank(&proj, default_config());

    let expected = 1.0 / 3.0;
    for &(_, score) in &result {
        assert!(
            (score - expected).abs() < 1e-5,
            "3-cycle nodes should converge to 1/3 by symmetry; got {score}"
        );
    }
    // Tie-break: NodeId ASC on equal scores.
    assert_eq!(
        result.iter().map(|&(n, _)| n).collect::<Vec<_>>(),
        nodes,
        "equal-score result must be ASC by NodeId per §E21 tie-break"
    );
}

#[test]
fn pagerank_chain_convergence_is_not_initial_uniform() {
    let (shared, nodes) = build_graph(4, &[(0, 1), (1, 2), (2, 3)]);
    let proj = build_proj(&shared);
    let initial = pagerank(
        &proj,
        PageRankConfig {
            max_iter: 0,
            ..default_config()
        },
    );
    let converged = pagerank(
        &proj,
        PageRankConfig {
            max_iter: 100,
            tolerance: 1e-9,
            ..default_config()
        },
    );

    assert!(initial.iter().all(|&(_, score)| {
        let expected = 1.0 / 4.0;
        (score - expected).abs() <= 1e-12
    }));
    let sink_score = converged
        .iter()
        .find(|&&(node, _)| node == nodes[3])
        .unwrap()
        .1;
    let source_score = converged
        .iter()
        .find(|&&(node, _)| node == nodes[0])
        .unwrap()
        .1;
    assert!(
        sink_score > source_score,
        "multi-iteration PageRank should move mass along the chain"
    );
    assert_ne!(
        converged
            .iter()
            .map(|&(node, score)| (node, score.to_bits()))
            .collect::<Vec<_>>(),
        initial
            .iter()
            .map(|&(node, score)| (node, score.to_bits()))
            .collect::<Vec<_>>(),
        "converged chain result must differ from the 1/N initial vector"
    );
}

#[test]
fn pagerank_mass_conservation() {
    // §O.W.2 — Σ score should converge to 1.0 (probability distribution).
    let (shared, _) = build_graph(5, &[(0, 1), (1, 2), (2, 0), (3, 4), (4, 3)]);
    let proj = build_proj(&shared);
    let result = pagerank(&proj, default_config());
    let total: f64 = result.iter().map(|&(_, s)| s).sum();
    assert!(
        (total - 1.0).abs() < 1e-6,
        "Σ score should converge to 1.0; got {total}"
    );
}

#[test]
fn pagerank_hub_dominates_in_star_graph() {
    // Star: n0 is center; n1..n4 each have one out-edge to n0.
    // n0 should have highest score (receives from all spokes).
    let (shared, nodes) = build_graph(5, &[(1, 0), (2, 0), (3, 0), (4, 0)]);
    let proj = build_proj(&shared);
    let result = pagerank(&proj, default_config());
    assert_eq!(
        result[0].0, nodes[0],
        "hub n0 must rank first under DESC-by-score"
    );
    assert!(
        result[0].1 > result[1].1,
        "hub score must strictly exceed spoke scores"
    );
}

#[test]
fn pagerank_dangling_node_preserves_mass() {
    // n0 → n1; n2 is dangling (no out-edges) with initial mass.
    // §E23: dangling redistribution preserves Σ score = 1.0.
    let (shared, _) = build_graph(3, &[(0, 1)]);
    let proj = build_proj(&shared);
    let result = pagerank(&proj, default_config());
    let total: f64 = result.iter().map(|&(_, s)| s).sum();
    assert!(
        (total - 1.0).abs() < 1e-6,
        "mass not preserved with dangling node; got {total}"
    );
}

#[test]
fn pagerank_convergence_threshold_terminates_early() {
    // Tight tolerance + low max_iter: must complete via tolerance branch on
    // a small graph that converges quickly.
    let (shared, _) = build_graph(2, &[(0, 1), (1, 0)]);
    let proj = build_proj(&shared);
    let cfg = PageRankConfig {
        damping: 0.85,
        max_iter: 1000,
        tolerance: 1e-3,
        parallelism: Parallelism::Sequential,
        orientation: PageRankOrientation::Natural,
        personalization: None,
    };
    let result = pagerank(&proj, cfg);
    let total: f64 = result.iter().map(|&(_, s)| s).sum();
    assert!((total - 1.0).abs() < 1e-2);
}

#[test]
fn pagerank_directed_edge_propagates_score_one_way() {
    // n0 → n1 only. n1 receives more probability mass than n0 (which only
    // has the teleport contribution + its own initial via dangling spread).
    let (shared, nodes) = build_graph(2, &[(0, 1)]);
    let proj = build_proj(&shared);
    let result = pagerank(&proj, default_config());
    let score_n0 = result.iter().find(|&&(n, _)| n == nodes[0]).unwrap().1;
    let score_n1 = result.iter().find(|&&(n, _)| n == nodes[1]).unwrap().1;
    assert!(
        score_n1 > score_n0,
        "n1 receives from n0; should score higher. n0={score_n0}, n1={score_n1}"
    );
}

#[test]
fn pagerank_handles_sparse_row_projection() {
    // §E20 — state arrays sized by RowIndex, not max_row + 1.
    let shared = SharedGraph::new(GraphId::new(1));
    let label = db_string("N");
    let rel = db_string("R");
    let mut txn = shared.begin_write();
    let mut nodes = Vec::with_capacity(100);
    for _ in 0..100 {
        nodes.push(
            txn.mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap(),
        );
    }
    txn.mutator()
        .create_edge(rel.clone(), nodes[0], nodes[50], PropertyMap::new())
        .unwrap();
    txn.mutator()
        .create_edge(rel.clone(), nodes[50], nodes[99], PropertyMap::new())
        .unwrap();
    txn.mutator()
        .create_edge(rel, nodes[99], nodes[0], PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();

    let snapshot = shared.read();
    let scope = snapshot
        .bind_node_candidates([nodes[0], nodes[50], nodes[99]])
        .unwrap();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "sparse-pr".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        Some(&scope),
    )
    .unwrap();

    assert_eq!(proj.node_count(), 3);
    let result = pagerank(&proj, default_config());
    let total: f64 = result.iter().map(|&(_, s)| s).sum();
    assert!((total - 1.0).abs() < 1e-6);
    // 3-cycle within the scope → all three converge to 1/3.
    for &(_, score) in &result {
        assert!((score - 1.0 / 3.0).abs() < 1e-5);
    }
}

#[test]
fn pagerank_dangling_heavy_graph_mass_preserved() {
    // PR #60 Codex P1 regression: graph with many dangling nodes used to be
    // O(N²) per iteration. Verify correctness on a fixture with all-but-one
    // nodes dangling: only n0 has an out-edge to n1; n2/n3/n4 are dangling.
    // Σ score must still converge to 1.0 under the bulk-apply formulation.
    let (shared, _) = build_graph(5, &[(0, 1)]);
    let proj = build_proj(&shared);
    let result = pagerank(&proj, default_config());
    let total: f64 = result.iter().map(|&(_, s)| s).sum();
    assert!(
        (total - 1.0).abs() < 1e-6,
        "dangling-heavy graph must preserve mass under bulk-apply; got {total}"
    );
    // Sanity: every score is finite and positive.
    for &(nid, score) in &result {
        assert!(
            score.is_finite() && score > 0.0,
            "node {nid:?} score must be finite + positive; got {score}"
        );
    }
}

#[test]
fn pagerank_zero_damping_pure_teleport() {
    // damping = 0 → all probability goes to teleport (1/N for each).
    let (shared, _) = build_graph(4, &[(0, 1), (1, 2), (2, 3), (3, 0)]);
    let proj = build_proj(&shared);
    let cfg = PageRankConfig {
        damping: 0.0,
        max_iter: 50,
        tolerance: 1e-9,
        parallelism: Parallelism::Sequential,
        orientation: PageRankOrientation::Natural,
        personalization: None,
    };
    let result = pagerank(&proj, cfg);
    let expected = 1.0 / 4.0;
    for &(_, score) in &result {
        assert!(
            (score - expected).abs() < 1e-9,
            "damping=0 → pure teleport 1/N"
        );
    }
}

#[test]
fn pagerank_zero_damping_uses_personalized_teleport() {
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let cfg = PageRankConfig {
        damping: 0.0,
        max_iter: 50,
        tolerance: 1e-12,
        parallelism: Parallelism::Sequential,
        orientation: PageRankOrientation::Natural,
        personalization: Some(vec![(nodes[0], 3.0), (nodes[2], 1.0)]),
    };

    let result = pagerank(&proj, cfg);
    let score_n0 = result.iter().find(|&&(n, _)| n == nodes[0]).unwrap().1;
    let score_n1 = result.iter().find(|&&(n, _)| n == nodes[1]).unwrap().1;
    let score_n2 = result.iter().find(|&&(n, _)| n == nodes[2]).unwrap().1;

    assert!((score_n0 - 0.75).abs() < 1e-12);
    assert!((score_n1 - 0.0).abs() < 1e-12);
    assert!((score_n2 - 0.25).abs() < 1e-12);
    assert_eq!(
        result.iter().map(|&(node, _)| node).collect::<Vec<_>>(),
        vec![nodes[0], nodes[2], nodes[1]]
    );
}

#[test]
fn pagerank_personalized_dangling_mass_returns_to_seed_distribution() {
    let (shared, nodes) = build_graph(3, &[]);
    let proj = build_proj(&shared);
    let cfg = PageRankConfig {
        damping: 0.85,
        max_iter: 10,
        tolerance: 0.0,
        parallelism: Parallelism::Sequential,
        orientation: PageRankOrientation::Natural,
        personalization: Some(vec![(nodes[2], 1.0)]),
    };

    let result = pagerank(&proj, cfg);
    let score_n0 = result.iter().find(|&&(n, _)| n == nodes[0]).unwrap().1;
    let score_n1 = result.iter().find(|&&(n, _)| n == nodes[1]).unwrap().1;
    let score_n2 = result.iter().find(|&&(n, _)| n == nodes[2]).unwrap().1;

    assert!((score_n0 - 0.0).abs() < 1e-12);
    assert!((score_n1 - 0.0).abs() < 1e-12);
    assert!((score_n2 - 1.0).abs() < 1e-12);
    let total: f64 = result.iter().map(|&(_, score)| score).sum();
    assert!((total - 1.0).abs() < 1e-12);
}

#[test]
fn pagerank_personalized_sink_spreads_under_undirected_orientation() {
    let (shared, nodes) = build_graph(3, &[(0, 1), (2, 1)]);
    let proj = build_proj(&shared);
    let seed = Some(vec![(nodes[1], 1.0)]);
    let natural = pagerank(
        &proj,
        PageRankConfig {
            max_iter: 1,
            tolerance: 0.0,
            personalization: seed.clone(),
            ..default_config()
        },
    );
    let undirected = pagerank(
        &proj,
        PageRankConfig {
            max_iter: 1,
            tolerance: 0.0,
            orientation: PageRankOrientation::Undirected,
            personalization: seed,
            ..default_config()
        },
    );

    let score = |rows: &[(NodeId, f64)], node| rows.iter().find(|&&(n, _)| n == node).unwrap().1;
    assert!((score(&natural, nodes[1]) - 1.0).abs() < 1e-12);
    assert!((score(&natural, nodes[0]) - 0.0).abs() < 1e-12);
    assert!((score(&natural, nodes[2]) - 0.0).abs() < 1e-12);

    assert!((score(&undirected, nodes[1]) - 0.15).abs() < 1e-12);
    assert!((score(&undirected, nodes[0]) - 0.425).abs() < 1e-12);
    assert!((score(&undirected, nodes[2]) - 0.425).abs() < 1e-12);
}
