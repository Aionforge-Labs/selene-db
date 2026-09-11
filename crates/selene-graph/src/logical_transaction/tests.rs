use super::*;
use selene_catalog::{
    CatalogDescriptor, CatalogGeneration, CatalogId, CatalogLogicalChange, CatalogName,
    CatalogSnapshotBuilder, CreationMetadata, DirectoryId, SchemaId,
};
use selene_core::{Change, EdgeDirectionality, EdgeId, LabelSet, PropertyMap, db_string};

#[path = "checkpoint_tests.rs"]
mod checkpoint;
#[path = "golden_tests.rs"]
mod golden;
#[path = "named_type_tests.rs"]
mod named_type;
#[path = "semantic_tests.rs"]
mod semantic;

#[test]
fn logical_body_rejects_unassigned_version() {
    assert_eq!(
        LogicalTransaction::decode(&[2, 0, 0, 0], Limits::default()).unwrap_err(),
        E::Invalid("logical body version")
    );
}

fn generation(n: u64) -> CatalogGeneration {
    CatalogGeneration::new(n).unwrap()
}
fn seed() -> ReplayState {
    let catalog = CatalogId::new(1).unwrap();
    let root = DirectoryId::new(1).unwrap();
    let creation = CreationMetadata::new(generation(1), None);
    let snapshot = CatalogSnapshotBuilder::new(
        generation(1),
        CatalogDescriptor::catalog(
            catalog,
            CatalogName::regular("selene").unwrap(),
            generation(1),
            creation.clone(),
        )
        .unwrap(),
        CatalogDescriptor::root_directory(root, catalog, generation(1), creation).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap();
    ReplayState::seed(
        CatalogLogicalRecords::new(
            generation(1),
            BTreeMap::from([
                (selene_catalog::CatalogObjectKind::Catalog, 1),
                (selene_catalog::CatalogObjectKind::Directory, 1),
            ]),
            snapshot.descriptors().cloned().collect(),
        )
        .unwrap(),
    )
    .unwrap()
}
fn transaction(seed: &ReplayState) -> LogicalTransaction {
    let old = seed.catalog.reconstruct().unwrap();
    let mut descriptors: Vec<_> = old.descriptors().cloned().collect();
    let schema = SchemaId::new(1).unwrap();
    let creation = CreationMetadata::new(generation(2), None);
    descriptors.push(
        CatalogDescriptor::schema(
            schema,
            CatalogName::regular("data").unwrap(),
            old.root_directory_id(),
            generation(2),
            creation.clone(),
        )
        .unwrap(),
    );
    for (id, name) in [(1, "one"), (2, "two")] {
        descriptors.push(
            CatalogDescriptor::graph(
                selene_catalog::GraphId::new(id).unwrap(),
                CatalogName::regular(name).unwrap(),
                schema,
                generation(2),
                creation.clone(),
                None,
            )
            .unwrap(),
        );
    }
    let mut water = seed.catalog.high_water().clone();
    water.insert(selene_catalog::CatalogObjectKind::Schema, 1);
    water.insert(selene_catalog::CatalogObjectKind::Graph, 2);
    let next = CatalogLogicalRecords::new(generation(2), water, descriptors).unwrap();
    let graphs = [1, 2]
        .map(|id| GraphDelta {
            id: GraphId::new(id),
            previous: None,
            generation: 1,
            next_node_id: 3,
            next_edge_id: 2,
            definition: None,
            backing_indexes: vec![],
            changes: vec![node(1), node(2), edge(1, 1, 2)],
        })
        .into();
    LogicalTransaction {
        catalog: CatalogDelta::between(&old, &next).unwrap(),
        graph_types: vec![],
        graphs,
    }
}
fn node(id: u64) -> Change {
    Change::NodeCreated {
        id: NodeId::new(id),
        labels: LabelSet::new(),
        properties: PropertyMap::from_pairs([(db_string("v").unwrap(), Value::Int(id as i64))])
            .unwrap(),
    }
}
fn edge(id: u64, source: u64, target: u64) -> Change {
    Change::EdgeCreated {
        id: EdgeId::new(id),
        directionality: EdgeDirectionality::Undirected,
        label: db_string("LINK").unwrap(),
        source: NodeId::new(source),
        target: NodeId::new(target),
        properties: PropertyMap::new(),
    }
}
fn apply(seed: &ReplayState, tx: &LogicalTransaction) -> CodecResult<ReplayState> {
    seed.apply_body(&tx.encode(Limits::default())?, Limits::default())
}

#[test]
fn catalog_and_two_graphs_are_one_atomic_unit_including_invalid_last_record() {
    let seed = seed();
    let tx = transaction(&seed);
    let valid = apply(&seed, &tx).unwrap();
    for id in [1, 2] {
        assert_eq!(valid.graph_summary(GraphId::new(id)), Some((2, 1, 3, 2)));
    }
    assert_eq!(seed.catalog.descriptors().len(), 2);
    assert!(seed.graphs.is_empty());
    let mut bad = tx.clone();
    bad.graphs[1].changes.push(Change::EdgeUpdated {
        id: EdgeId::new(99),
        properties_diff: selene_core::PropertyDiff::new([], []).unwrap(),
    });
    assert!(apply(&seed, &bad).is_err());
    assert_eq!(seed.catalog.descriptors().len(), 2);
    assert!(seed.graphs.is_empty());
    let mut bad_bytes = tx.encode(Limits::default()).unwrap();
    // The last field is the last edge's property count. Oversized final count must
    // not leave graph one or the catalog visible, regardless of earlier validity.
    let len = bad_bytes.len();
    bad_bytes[len - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(seed.apply_body(&bad_bytes, Limits::default()).is_err());
    assert_eq!(seed.catalog.descriptors().len(), 2);
    assert!(seed.graphs.is_empty());
}

#[test]
fn order_endpoints_foreign_graph_and_high_water_fail_closed() {
    let seed = seed();
    let tx = transaction(&seed);
    let mut bad = tx.clone();
    bad.graphs[1].changes.swap(0, 2);
    assert!(apply(&seed, &bad).is_err(), "forward endpoint");
    let mut bad = tx.clone();
    bad.graphs[1].changes[2] = edge(1, 1, 99);
    assert!(apply(&seed, &bad).is_err(), "missing endpoint");
    let mut bad = tx.clone();
    bad.graphs[1].id = GraphId::new(99);
    assert!(apply(&seed, &bad).is_err(), "missing catalog graph");
    let mut bad = tx.clone();
    bad.graphs[1].next_node_id = 2;
    assert!(apply(&seed, &bad).is_err(), "low element water");
    let mut bad = tx.clone();
    bad.catalog.changes.swap(0, 1);
    assert!(apply(&seed, &bad).is_err(), "forward catalog owner");
    let mut bad = tx.clone();
    bad.catalog.high_water[3] = 1;
    assert!(apply(&seed, &bad).is_err(), "low catalog water");
}

fn followup(state: &ReplayState, changes: Vec<Change>) -> LogicalTransaction {
    let old = state.catalog.reconstruct().unwrap();
    LogicalTransaction {
        catalog: CatalogDelta::between(&old, &state.catalog).unwrap(),
        graph_types: vec![],
        graphs: vec![GraphDelta {
            id: GraphId::new(1),
            previous: Some(state.graphs[&GraphId::new(1)].meta.generation),
            generation: state.graphs[&GraphId::new(1)].meta.generation + 1,
            next_node_id: 3,
            next_edge_id: 2,
            definition: None,
            backing_indexes: vec![],
            changes,
        }],
    }
}
#[test]
fn deleted_ids_and_mixed_topology_survive_subsequent_transactions() {
    let seed = seed();
    let state = apply(&seed, &transaction(&seed)).unwrap();
    let record = state.graphs[&GraphId::new(1)]
        .edge_record(EdgeId::new(1))
        .unwrap();
    assert_eq!(record.directionality, EdgeDirectionality::Undirected);
    assert_eq!(
        (record.first, record.second),
        (NodeId::new(1), NodeId::new(2))
    );
    let deleted = apply(
        &state,
        &followup(
            &state,
            vec![
                Change::EdgeDeleted { id: EdgeId::new(1) },
                Change::NodeDeleted { id: NodeId::new(1) },
            ],
        ),
    )
    .unwrap();
    assert_eq!(deleted.graph_summary(GraphId::new(1)), Some((1, 0, 3, 2)));
    assert!(apply(&deleted, &followup(&deleted, vec![node(1)])).is_err());
    assert!(apply(&deleted, &followup(&deleted, vec![edge(1, 2, 2)])).is_err());
    assert_eq!(state.graph_summary(GraphId::new(1)), Some((2, 1, 3, 2)));
}
#[test]
fn reset_and_truncate_keep_allocation_water() {
    let seed = seed();
    let state = apply(&seed, &transaction(&seed)).unwrap();
    let reset = apply(&state, &followup(&state, vec![Change::GraphReset {}])).unwrap();
    assert_eq!(reset.graph_summary(GraphId::new(1)), Some((0, 0, 3, 2)));
    assert_eq!(reset.graph_summary(GraphId::new(2)), Some((2, 1, 3, 2)));
    let truncated = apply(
        &state,
        &followup(
            &state,
            vec![Change::EdgesOfTypeTruncated {
                label: db_string("LINK").unwrap(),
            }],
        ),
    )
    .unwrap();
    assert_eq!(truncated.graph_summary(GraphId::new(1)), Some((2, 0, 3, 2)));
}

#[test]
fn independent_empty_body_and_complete_prefix_reject_trailing_bytes() {
    let seed = seed();
    let old = seed.catalog.reconstruct().unwrap();
    let tx = LogicalTransaction {
        catalog: CatalogDelta::between(&old, &seed.catalog).unwrap(),
        graph_types: vec![],
        graphs: vec![],
    };
    // Body v1 (u32), catalog before/after (u64 each), nine u64 high waters,
    // catalog change count, named type count, graph count (u32 each): 104 bytes.
    let mut golden = vec![0; 104];
    golden[0] = 1;
    golden[4] = 1;
    golden[12] = 1;
    golden[20] = 1;
    golden[28] = 1;
    assert_eq!(tx.encode(Limits::default()).unwrap(), golden);
    assert_eq!(
        LogicalTransaction::decode(&golden, Limits::default()).unwrap(),
        tx
    );
    for cut in 0..golden.len() {
        assert!(LogicalTransaction::decode(&golden[..cut], Limits::default()).is_err());
    }
    golden.push(0);
    assert_eq!(
        LogicalTransaction::decode(&golden, Limits::default()).unwrap_err(),
        E::Invalid("trailing bytes")
    );
}

#[test]
fn catalog_drop_keeps_deleted_identity_high_water() {
    let seed = seed();
    let populated = apply(&seed, &transaction(&seed)).unwrap();
    let state = apply(
        &populated,
        &followup(&populated, vec![Change::GraphReset {}]),
    )
    .unwrap();
    let mut tx = followup(&state, vec![]);
    tx.graphs.clear();
    tx.catalog.generation = generation(3);
    let id = CatalogObjectId::Graph(selene_catalog::GraphId::new(1).unwrap());
    tx.catalog.changes = vec![CatalogLogicalChange::Dropped {
        id,
        generation: generation(2),
    }];
    let dropped = apply(&state, &tx).unwrap();
    assert!(dropped.graph_summary(GraphId::new(1)).is_none());
    let descriptor = state
        .catalog
        .reconstruct()
        .unwrap()
        .descriptor(id)
        .unwrap()
        .clone();
    let mut reused = tx;
    reused.catalog.previous = generation(3);
    reused.catalog.generation = generation(4);
    reused.catalog.changes = vec![CatalogLogicalChange::Created(descriptor)];
    assert!(apply(&dropped, &reused).is_err());
}
