//! Semantic failures behind valid physical integrity, exercised through both public APIs.
use super::*;
use crate::{CreatePolicy, ObjectPath};
use selene_core::{
    Change, EdgeDirectionality, EdgeId, GraphId, NodeId, db_string,
    logical::{Encoder, GraphDelta},
};

fn image(records: &selene_catalog::CatalogLogicalRecords, graph: &GraphDelta) -> Vec<u8> {
    let mut e = Encoder::new(Limits::default()).unwrap();
    e.u32(1).unwrap();
    e.u8(1).unwrap();
    selene_catalog::codec::encode_records(records, &mut e).unwrap();
    e.u8(2).unwrap();
    e.count(0).unwrap(); // no named definitions
    e.u8(3).unwrap();
    e.count(1).unwrap();
    graph.encode(&mut e).unwrap();
    e.finish()
}

#[test]
fn public_semantic_inventory_highwater_endpoint_and_backing_fail_closed() {
    let memory = Database::builder().build();
    let path = ObjectPath::regular("selene", "semantic", "data").unwrap();
    memory
        .catalog()
        .create_schema(&path.schema_path(), CreatePolicy::Strict)
        .unwrap();
    memory
        .catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let records = memory.catalog().snapshot().logical_catalog().unwrap();
    let good = GraphDelta {
        id: GraphId::new(1),
        previous: None,
        generation: 1,
        next_node_id: 2,
        next_edge_id: 1,
        definition: None,
        backing_indexes: vec![],
        changes: vec![Change::NodeCreated {
            id: NodeId::new(1),
            labels: Default::default(),
            properties: Default::default(),
        }],
    };
    let healthy = image(&records, &good);
    assert!(ReplayState::from_checkpoint(&healthy, Limits::default()).is_ok());
    for case in 0..4 {
        let mut corrupt = good.clone();
        match case {
            0 => corrupt.id = GraphId::new(2), // graph omitted/replaced relative to catalog
            1 => corrupt.next_node_id = 1,     // known primary identity above allocator floor
            2 => {
                corrupt.next_edge_id = 2;
                corrupt.changes.push(Change::EdgeCreated {
                    id: EdgeId::new(1),
                    directionality: EdgeDirectionality::Directed,
                    source: NodeId::new(1),
                    target: NodeId::new(99),
                    label: db_string("missing_endpoint").unwrap(),
                    properties: Default::default(),
                });
            }
            _ => corrupt.backing_indexes.push(999),
        }
        let body = image(&records, &corrupt);
        let temp = tempfile::tempdir().unwrap();
        let dir = StoreDirectory::open(temp.path()).unwrap();
        let mut wal = LogicalWal::create(
            EmptyStoreControl::create_empty(&dir, compatibility().unwrap()).unwrap(),
        )
        .unwrap();
        let selected = wal.checkpoint(&body, 0).unwrap();
        drop(wal);
        let before = std::fs::read(temp.path().join(&selected.name)).unwrap();
        let verified = Database::verify(temp.path()).unwrap_err();
        assert_eq!(
            (verified.phase, verified.kind),
            (StoragePhase::Snapshot, StorageErrorKind::Semantic),
            "case {case}"
        );
        assert_eq!(verified.artifact.as_deref(), Some(selected.name.as_str()));
        let opened = Database::open(temp.path()).err().unwrap();
        assert_eq!((opened.phase, opened.kind), (verified.phase, verified.kind));
        assert_eq!(
            std::fs::read(temp.path().join(&selected.name)).unwrap(),
            before
        );
    }
}

#[test]
fn unsupported_allocation_domain_retains_rebuild_artifact_context() {
    let memory = Database::builder().build();
    let records = memory.catalog().snapshot().logical_catalog().unwrap();
    let mut water = records.high_water().clone();
    water.insert(selene_catalog::CatalogObjectKind::BindingTable, 1);
    let records = selene_catalog::CatalogLogicalRecords::new(
        records.reconstruct().unwrap().generation(),
        water,
        records.descriptors().to_vec(),
    )
    .unwrap();
    let body = encode_checkpoint(&records, &Default::default(), &[], Limits::default()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let dir = StoreDirectory::open(temp.path()).unwrap();
    let mut wal = LogicalWal::create(
        EmptyStoreControl::create_empty(&dir, compatibility().unwrap()).unwrap(),
    )
    .unwrap();
    wal.checkpoint(&body, 0).unwrap();
    drop(wal);
    let verified = Database::verify(temp.path()).unwrap_err();
    assert_eq!(
        (verified.phase, verified.kind),
        (StoragePhase::Rebuild, StorageErrorKind::NativeAdmission)
    );
    assert!(
        verified
            .artifact
            .as_deref()
            .unwrap()
            .starts_with("MANIFEST-")
    );
    assert_eq!(verified.expected_sequence, Some(0));
    let opened = Database::open(temp.path()).err().unwrap();
    assert_eq!(
        (opened.phase, opened.kind, opened.artifact),
        (verified.phase, verified.kind, verified.artifact)
    );
}
