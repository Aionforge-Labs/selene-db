//! Mixed projection identity and incidence contract.

use selene_algorithms::{GraphProjection, ProjectionConfig};
use selene_core::{EdgeDirectionality, GraphId, LabelSet, PropertyMap, db_string};
use selene_graph::SharedGraph;

#[test]
fn undirected_projection_has_two_way_incidence_but_one_logical_edge() {
    let graph = SharedGraph::new(GraphId::new(901));
    let mut tx = graph.begin_write();
    let a = tx
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let b = tx
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let e = tx
        .mutator()
        .create_mixed_edge(
            db_string("E").unwrap(),
            b,
            a,
            EdgeDirectionality::Undirected,
            PropertyMap::new(),
        )
        .unwrap();
    let loop_edge = tx
        .mutator()
        .create_mixed_edge(
            db_string("E").unwrap(),
            a,
            a,
            EdgeDirectionality::Undirected,
            PropertyMap::new(),
        )
        .unwrap();
    tx.commit().unwrap();
    let p = GraphProjection::build(
        &graph.read(),
        &ProjectionConfig {
            name: "mixed".into(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(p.edge_count(), 2);
    for neighbors in [p.out_neighbors(a), p.in_neighbors(a)] {
        assert_eq!(
            neighbors.iter().map(|n| n.edge_id).collect::<Vec<_>>(),
            vec![loop_edge, e]
        );
    }
    for neighbors in [p.out_neighbors(b), p.in_neighbors(b)] {
        assert_eq!(neighbors.len(), 1);
        assert_eq!((neighbors[0].edge_id, neighbors[0].node_id), (e, a));
    }
}

#[test]
fn native_pathfinding_components_filters_and_weights_use_mixed_projection() {
    let f = selene_testing::mixed_orientation::MixedOrientationFixture::build();
    let snapshot = f.graph.read();
    let config = ProjectionConfig {
        name: "mixed".into(),
        node_labels: vec![db_string("N").unwrap()],
        edge_labels: vec![db_string("E").unwrap()],
        weight_property: Some(db_string("key").unwrap()),
    };
    let p = GraphProjection::build(&snapshot, &config, None).unwrap();
    assert_eq!(p.edge_count(), 7);
    assert_eq!(selene_algorithms::structural::wcc_count(&p), 2);
    assert_eq!(selene_algorithms::structural::scc_count(&p), 2);
    let only_a = snapshot.bind_node_candidates([f.nodes[0]]).unwrap();
    let filtered = GraphProjection::build(&snapshot, &config, Some(&only_a)).unwrap();
    assert_eq!(filtered.edge_count(), 2, "both loop kinds count once");
    assert_eq!(filtered.out_degree(f.nodes[0]), 2);
    assert_eq!(filtered.in_degree(f.nodes[0]), 2);
    assert_eq!(
        filtered
            .out_neighbors(f.nodes[0])
            .iter()
            .map(|n| n.weight)
            .collect::<Vec<_>>(),
        vec![4.0, 5.0]
    );
    // Remove directed connections: pathfinding in each direction now depends
    // on real undirected edges, not on a reciprocal-directed fixture.
    let mut tx = f.graph.begin_write();
    for i in [0, 1, 4, 6] {
        tx.mutator().delete_edge(f.edges[i]).unwrap();
    }
    tx.commit().unwrap();
    let rebuilt = GraphProjection::build(&f.graph.read(), &config, None).unwrap();
    assert!(rebuilt.generation() > p.generation());
    assert_eq!(rebuilt.edge_count(), 3);
    for (a, b) in [(f.nodes[0], f.nodes[1]), (f.nodes[1], f.nodes[0])] {
        let result = selene_algorithms::pathfinding::dijkstra(&rebuilt, a, b)
            .unwrap()
            .unwrap();
        assert_eq!(result.nodes, vec![a, b]);
        assert_eq!(result.cost, 2.0);
    }
    assert_eq!(selene_algorithms::structural::wcc_count(&rebuilt), 2);
    assert_eq!(selene_algorithms::structural::scc_count(&rebuilt), 2);
}

#[test]
fn community_incidence_is_independent_of_intrinsic_edge_kind() {
    fn projection(undirected: bool) -> GraphProjection {
        let graph = SharedGraph::new(GraphId::new(911));
        let mut tx = graph.begin_write();
        let nodes = std::array::from_fn::<_, 4, _>(|_| {
            tx.mutator()
                .create_node(LabelSet::new(), PropertyMap::new())
                .unwrap()
        });
        for (a, b) in [(0, 1), (0, 1), (1, 1), (2, 3)] {
            tx.mutator()
                .create_mixed_edge(
                    db_string("E").unwrap(),
                    nodes[a],
                    nodes[b],
                    if undirected {
                        EdgeDirectionality::Undirected
                    } else {
                        EdgeDirectionality::Directed
                    },
                    PropertyMap::new(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
        GraphProjection::build(
            &graph.read(),
            &ProjectionConfig {
                name: "p".into(),
                node_labels: vec![],
                edge_labels: vec![],
                weight_property: None,
            },
            None,
        )
        .unwrap()
    }
    let directed = projection(false);
    let undirected = projection(true);
    assert_eq!(
        selene_algorithms::community::label_propagation(&directed, 20),
        selene_algorithms::community::label_propagation(&undirected, 20)
    );
    let expected = selene_algorithms::community::louvain(&directed, 20);
    assert_eq!(
        selene_algorithms::community::louvain(&undirected, 20),
        expected
    );
    assert_eq!(expected.len(), 4);
    assert_eq!(expected[0].1, expected[1].1);
    assert_eq!(expected[2].1, expected[3].1);
    assert_ne!(expected[0].1, expected[2].1);
}
