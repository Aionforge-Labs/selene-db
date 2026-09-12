//! Projection reuse must validate detached workspace identity, not just generation.

use selene_algorithms::{GraphProjection, ProjectionCatalog, ProjectionConfig, wcc};
use selene_core::{GraphId, LabelSet, PropertyMap, db_string};
use selene_graph::SharedGraph;

fn config() -> ProjectionConfig {
    ProjectionConfig {
        name: "p".into(),
        node_labels: vec![],
        edge_labels: vec![],
        weight_property: None,
    }
}

fn graph(nodes: usize) -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(1));
    let mut tx = graph.begin_write();
    for _ in 0..nodes {
        tx.mutator()
            .create_node(
                LabelSet::single(db_string("N").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
    }
    tx.commit().unwrap();
    graph
}

#[test]
fn same_numeric_generation_never_reuses_other_snapshot_and_pins_are_independent() {
    let first = graph(2);
    let second = graph(5);
    let a = first.read();
    let b = second.read();
    assert_eq!(a.meta.generation, b.meta.generation);
    assert_eq!(a.graph_id(), b.graph_id());
    let catalog = ProjectionCatalog::new();
    catalog.project(&a, &config()).unwrap();
    let retained = catalog.resolve(&a, "p").unwrap();
    let other = catalog.resolve(&b, "p").unwrap();
    assert_eq!(other.node_count(), 5);
    assert_eq!(retained.node_count(), 2);
    assert_eq!(catalog.resolve(&a, "p").unwrap().node_count(), 2);
    // Direct construction shares the algorithm kernel, but independently
    // selects graph data and bypasses catalog lookup/cache state.
    let direct = GraphProjection::build(&b, &config(), None).unwrap();
    assert_eq!(wcc(&other), wcc(&direct));
}

#[test]
fn concurrent_resolve_always_returns_the_requested_snapshot() {
    let first = graph(2);
    let second = graph(5);
    let catalog = ProjectionCatalog::new();
    catalog.project(&first.read(), &config()).unwrap();
    std::thread::scope(|scope| {
        for graph in [&first, &second] {
            let catalog = &catalog;
            scope.spawn(move || {
                let snapshot = graph.read();
                for _ in 0..100 {
                    assert_eq!(
                        catalog.resolve(&snapshot, "p").unwrap().node_count(),
                        snapshot.node_count()
                    );
                }
            });
        }
    });
}
