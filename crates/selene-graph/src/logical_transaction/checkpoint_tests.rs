use super::*;

#[test]
fn checkpoint_reconstructs_two_complete_graphs_without_a_catalog_seed() {
    let seed = seed();
    let mut tx = transaction(&seed);
    tx.graphs[0].next_node_id = 91;
    tx.graphs[0].next_edge_id = 83;
    let live = apply(&seed, &tx).unwrap();
    let graphs: Vec<_> = live.graphs.values().map(AsRef::as_ref).collect();
    let bytes =
        encode_checkpoint(&live.catalog, &BTreeMap::new(), &graphs, Limits::default()).unwrap();
    let reopened = ReplayState::from_checkpoint(&bytes, Limits::default()).unwrap();
    assert_eq!(reopened.catalog.descriptors(), live.catalog.descriptors());
    assert_eq!(
        reopened.graph_summary(GraphId::new(1)),
        Some((2, 1, 91, 83))
    );
    assert_eq!(reopened.graph_summary(GraphId::new(2)), Some((2, 1, 3, 2)));
    for id in [1, 2] {
        let graph = &reopened.graphs[&GraphId::new(id)];
        assert_eq!(
            graph
                .node_properties(NodeId::new(1))
                .unwrap()
                .get(&db_string("v").unwrap()),
            Some(&Value::Int(1))
        );
        let edge = graph.edge_record(EdgeId::new(1)).unwrap();
        assert_eq!(edge.directionality, EdgeDirectionality::Undirected);
        assert_eq!((edge.first, edge.second), (NodeId::new(1), NodeId::new(2)));
    }
    for end in 0..bytes.len() {
        assert!(
            ReplayState::from_checkpoint(&bytes[..end], Limits::default()).is_err(),
            "cut {end}"
        );
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(ReplayState::from_checkpoint(&extra, Limits::default()).is_err());
    assert!(matches!(
        ReplayState::from_checkpoint(
            &bytes,
            Limits {
                allocation: 1024,
                ..Limits::default()
            }
        ),
        Err(E::Limit)
    ));
}
