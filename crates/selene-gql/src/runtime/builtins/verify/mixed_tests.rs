//! DATA-1: distinguish every expected incidence from balanced corruption.

use selene_core::{
    EdgeDirectionality::{Directed, Undirected},
    EdgeId, GraphId, LabelSet, NodeId, PropertyMap, Value, db_string,
};
use selene_graph::{AdjacencyEdge, SeleneGraph, SharedGraph};

use super::verify_snapshot;

fn fixture() -> (SeleneGraph, [NodeId; 3], [EdgeId; 7]) {
    let shared = SharedGraph::new(GraphId::new(121_304));
    let mut tx = shared.begin_write();
    let (nodes, edges) = {
        let mut m = tx.mutator();
        let nodes =
            std::array::from_fn(|_| m.create_node(LabelSet::new(), PropertyMap::new()).unwrap());
        let [a, b, _] = nodes;
        let edges = [
            (a, b, Directed),
            (b, a, Directed),
            (a, b, Directed),
            (a, b, Undirected),
            (b, a, Undirected),
            (a, a, Directed),
            (a, a, Undirected),
        ]
        .map(|(first, second, kind)| {
            m.create_mixed_edge(
                db_string("E").unwrap(),
                first,
                second,
                kind,
                PropertyMap::new(),
            )
            .unwrap()
        });
        (nodes, edges)
    };
    tx.commit().unwrap();
    (shared.read().as_ref().clone(), nodes, edges)
}

fn incidence(id: EdgeId, neighbor: NodeId) -> AdjacencyEdge {
    AdjacencyEdge {
        edge_id: id,
        neighbor,
        label: db_string("E").unwrap(),
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    UndirectedInDirectedMaps,
    DirectedInUndirectedMap,
    MissingUndirectedEndpoint,
    MissingBothUndirectedEndpoints,
    MissingDirectedLoopIncoming,
    MissingUndirectedLoop,
    DuplicateUndirectedLoop,
    DuplicateReplacingUndirectedSibling,
    DuplicateReplacingDirectedSibling,
    WrongLabel,
    WrongNeighbor,
    WrongMapKey,
    SpuriousIdentity,
    EmptyEntry,
}

#[rstest::rstest]
#[case::undirected_in_directed_maps(Fault::UndirectedInDirectedMaps)]
#[case::directed_in_undirected_map(Fault::DirectedInUndirectedMap)]
#[case::missing_undirected_endpoint(Fault::MissingUndirectedEndpoint)]
#[case::missing_both_undirected_endpoints(Fault::MissingBothUndirectedEndpoints)]
#[case::missing_directed_loop_incoming(Fault::MissingDirectedLoopIncoming)]
#[case::missing_undirected_loop(Fault::MissingUndirectedLoop)]
#[case::duplicate_undirected_loop(Fault::DuplicateUndirectedLoop)]
#[case::duplicate_replacing_undirected_sibling(Fault::DuplicateReplacingUndirectedSibling)]
#[case::duplicate_replacing_directed_sibling(Fault::DuplicateReplacingDirectedSibling)]
#[case::wrong_label(Fault::WrongLabel)]
#[case::wrong_neighbor(Fault::WrongNeighbor)]
#[case::wrong_map_key(Fault::WrongMapKey)]
#[case::spurious_identity(Fault::SpuriousIdentity)]
#[case::empty_entry(Fault::EmptyEntry)]
fn mixed_adjacency_corruption_is_reported_without_repair(#[case] fault: Fault) {
    let (
        mut graph,
        [a, b, c],
        [
            directed,
            _,
            directed_sibling,
            undirected,
            sibling,
            directed_loop,
            undirected_loop,
        ],
    ) = fixture();
    match fault {
        Fault::UndirectedInDirectedMaps => {
            graph
                .adjacency_undirected
                .get_mut_cow(&a)
                .unwrap()
                .remove(undirected);
            graph
                .adjacency_undirected
                .get_mut_cow(&b)
                .unwrap()
                .remove(undirected);
            graph
                .adjacency_out
                .get_mut_cow(&a)
                .unwrap()
                .edges
                .push(incidence(undirected, b));
            graph
                .adjacency_in
                .get_mut_cow(&b)
                .unwrap()
                .edges
                .push(incidence(undirected, a));
        }
        Fault::DirectedInUndirectedMap => {
            graph
                .adjacency_out
                .get_mut_cow(&a)
                .unwrap()
                .remove(directed);
            graph.adjacency_in.get_mut_cow(&b).unwrap().remove(directed);
            graph
                .adjacency_undirected
                .get_mut_cow(&a)
                .unwrap()
                .edges
                .push(incidence(directed, b));
            graph
                .adjacency_undirected
                .get_mut_cow(&b)
                .unwrap()
                .edges
                .push(incidence(directed, a));
        }
        Fault::MissingUndirectedEndpoint | Fault::MissingBothUndirectedEndpoints => {
            graph
                .adjacency_undirected
                .get_mut_cow(&a)
                .unwrap()
                .remove(undirected);
            if matches!(fault, Fault::MissingBothUndirectedEndpoints) {
                graph
                    .adjacency_undirected
                    .get_mut_cow(&b)
                    .unwrap()
                    .remove(undirected);
            }
        }
        Fault::MissingDirectedLoopIncoming => {
            graph
                .adjacency_in
                .get_mut_cow(&a)
                .unwrap()
                .remove(directed_loop);
        }
        Fault::MissingUndirectedLoop => {
            graph
                .adjacency_undirected
                .get_mut_cow(&a)
                .unwrap()
                .remove(undirected_loop);
        }
        Fault::DuplicateUndirectedLoop => {
            graph
                .adjacency_undirected
                .get_mut_cow(&a)
                .unwrap()
                .edges
                .push(incidence(undirected_loop, a));
        }
        Fault::DuplicateReplacingUndirectedSibling => {
            for node in [a, b] {
                let entry = graph.adjacency_undirected.get_mut_cow(&node).unwrap();
                entry
                    .edges
                    .iter_mut()
                    .find(|edge| edge.edge_id == sibling)
                    .unwrap()
                    .edge_id = undirected;
            }
        }
        Fault::DuplicateReplacingDirectedSibling => {
            for (map, node) in [(&mut graph.adjacency_out, a), (&mut graph.adjacency_in, b)] {
                map.get_mut_cow(&node)
                    .unwrap()
                    .edges
                    .iter_mut()
                    .find(|edge| edge.edge_id == directed_sibling)
                    .unwrap()
                    .edge_id = directed;
            }
        }
        Fault::WrongLabel | Fault::WrongNeighbor => {
            let edge = graph
                .adjacency_undirected
                .get_mut_cow(&a)
                .unwrap()
                .edges
                .iter_mut()
                .find(|edge| edge.edge_id == undirected)
                .unwrap();
            if matches!(fault, Fault::WrongLabel) {
                edge.label = db_string("wrong").unwrap();
            } else {
                edge.neighbor = c;
            }
        }
        Fault::WrongMapKey => {
            let entry = graph.adjacency_undirected.get(&a).unwrap().clone();
            graph.adjacency_undirected.remove_cow(&a);
            graph.adjacency_undirected.insert_cow(c, entry);
        }
        Fault::SpuriousIdentity => {
            graph
                .adjacency_undirected
                .get_mut_cow(&a)
                .unwrap()
                .edges
                .push(incidence(EdgeId::new(999), b));
        }
        Fault::EmptyEntry => {
            graph.adjacency_undirected.insert_cow(c, Default::default());
        }
    }
    let before = graph.clone();
    let result = verify_snapshot(&graph, false).unwrap();
    let row = result
        .rows
        .iter()
        .find(|row| matches!(&row[0], Value::String(name) if name.as_str() == "adjacency_symmetry"))
        .unwrap();
    assert!(
        matches!(&row[1], Value::String(status) if status.as_str() == "inconsistent"),
        "{fault:?}: {row:?}"
    );
    assert_eq!(graph.adjacency_out, before.adjacency_out);
    assert_eq!(graph.adjacency_in, before.adjacency_in);
    assert_eq!(graph.adjacency_undirected, before.adjacency_undirected);
}
