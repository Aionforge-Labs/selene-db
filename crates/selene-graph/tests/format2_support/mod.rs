//! Test-only public format-2 codec/apply fixture, not filesystem recovery.
#![allow(dead_code)] // Shared by test binaries using different fixture operations.
use selene_catalog::CatalogLogicalRecords;
use selene_catalog::codec::CatalogDelta;
use selene_catalog::{
    CatalogDescriptor as D, CatalogGeneration as G, CatalogId, CatalogName as N,
    CatalogObjectKind as K, CreationMetadata as C, DirectoryId, SchemaId,
};
use selene_core::{
    Change, GraphId,
    logical::{CodecResult, GraphDelta, Limits},
};
use selene_graph::{
    SeleneGraph, SharedGraph,
    logical_transaction::{LogicalTransaction, ReplayState, encode_checkpoint},
};
use std::collections::BTreeMap;

fn records(graph: Option<GraphId>) -> CatalogLogicalRecords {
    let generation = G::new(if graph.is_some() { 2 } else { 1 }).unwrap();
    let catalog = CatalogId::new(1).unwrap();
    let root = DirectoryId::new(1).unwrap();
    let first = G::new(1).unwrap();
    let mut descriptors = vec![
        D::catalog(
            catalog,
            N::regular("selene").unwrap(),
            first,
            C::new(first, None),
        )
        .unwrap(),
        D::root_directory(root, catalog, first, C::new(first, None)).unwrap(),
    ];
    let mut high = BTreeMap::from([(K::Catalog, 1), (K::Directory, 1)]);
    if let Some(graph) = graph {
        let schema = SchemaId::new(1).unwrap();
        descriptors.push(
            D::schema(
                schema,
                N::regular("data").unwrap(),
                root,
                generation,
                C::new(generation, None),
            )
            .unwrap(),
        );
        descriptors.push(
            D::graph(
                selene_catalog::GraphId::new(graph.get()).unwrap(),
                N::regular("graph").unwrap(),
                schema,
                generation,
                C::new(generation, None),
                None,
            )
            .unwrap(),
        );
        high.insert(K::Schema, 1);
        high.insert(K::Graph, graph.get());
    }
    CatalogLogicalRecords::new(generation, high, descriptors).unwrap()
}

pub fn replay(id: GraphId, changes: Vec<Change>) -> CodecResult<SharedGraph> {
    let before = records(None);
    let after = records(Some(id));
    let mut next_node_id = 1;
    let mut next_edge_id = 1;
    for change in &changes {
        match change {
            Change::NodeCreated { id, .. } => {
                next_node_id = next_node_id.max(id.get().checked_add(1).unwrap())
            }
            Change::EdgeCreated { id, .. } => {
                next_edge_id = next_edge_id.max(id.get().checked_add(1).unwrap())
            }
            _ => {}
        }
    }
    let tx = LogicalTransaction {
        catalog: CatalogDelta::between(&before.reconstruct().unwrap(), &after).unwrap(),
        graph_types: vec![],
        graphs: vec![GraphDelta {
            id,
            previous: None,
            generation: 1,
            next_node_id,
            next_edge_id,
            definition: None,
            backing_indexes: vec![],
            changes,
        }],
    };
    let state =
        ReplayState::seed(before)?.apply_body(&tx.encode(Limits::default())?, Limits::default())?;
    Ok(state
        .materialize(Limits::default())?
        .graphs
        .remove(&id)
        .unwrap())
}

pub fn snapshot(graph: &SeleneGraph) -> CodecResult<SharedGraph> {
    let bytes = encode_checkpoint(
        &records(Some(graph.graph_id())),
        &BTreeMap::new(),
        &[graph],
        Limits::default(),
    )?;
    Ok(ReplayState::from_checkpoint(&bytes, Limits::default())?
        .materialize(Limits::default())?
        .graphs
        .remove(&graph.graph_id())
        .unwrap())
}
