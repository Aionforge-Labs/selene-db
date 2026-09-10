//! Public writer/reader round-trip coverage for the WAL v2 entry format.

use std::fs;
use std::sync::Arc;

use selene_core::{Change, HlcTimestamp, LabelSet, NodeId, Origin, PropertyMap, Value, db_string};
use selene_persist::{SyncPolicy, WalConfig, WalWriter};

fn temp_path(_name: &str) -> selene_testing::PersistenceTestPath {
    selene_testing::PersistenceTestPath::new()
}

fn principal(index: usize) -> Option<Arc<[u8]>> {
    match index % 4 {
        0 => None,
        1 => Some(Arc::from([])),
        2 => Some(Arc::from(b"alice".as_slice())),
        _ => Some(Arc::from(vec![index as u8; 4096])),
    }
}

fn origin(index: usize) -> Origin {
    if index.is_multiple_of(2) {
        Origin::Local
    } else {
        Origin::Replicated {
            source_node_id: NodeId::new(index as u64 + 1),
            source_seq: index as u64 * 10,
        }
    }
}

fn changes(index: usize) -> Vec<Change> {
    vec![Change::NodeCreated {
        id: NodeId::new(1),
        labels: LabelSet::single(db_string("wal.v2.roundtrip").unwrap()),
        properties: PropertyMap::from_pairs([(
            db_string("wal.v2.payload").unwrap(),
            Value::Bytes(Arc::from(format!("entry-{index}").into_bytes())),
        )])
        .unwrap(),
    }]
}

#[test]
fn wal_v2_round_trips_mixed_origins_and_principals() {
    let path = temp_path("v2-round-trip");
    let expected: Vec<_> = (0..100)
        .map(|index| (origin(index), principal(index), changes(index)))
        .collect();
    {
        let mut writer = WalWriter::open(
            &path,
            WalConfig {
                sync_policy: SyncPolicy::OnFlushOnly,
                snapshot_seq: 0,
            },
        )
        .unwrap();
        for (index, (origin, principal, changes)) in expected.iter().enumerate() {
            let sequence = writer
                .append(
                    HlcTimestamp::new(index as u64, index as u32),
                    *origin,
                    principal.clone(),
                    changes,
                )
                .unwrap();
            assert_eq!(sequence, index as u64 + 1);
        }
    }

    let reader = selene_persist::WalReader::open(&path).unwrap();
    let observed: Vec<_> = reader
        .iterate(|_| true)
        .unwrap()
        .map(|result| result.unwrap())
        .collect();
    assert_eq!(observed.len(), expected.len());
    for (index, (view, (origin, principal, changes))) in
        observed.into_iter().zip(expected).enumerate()
    {
        assert_eq!(view.header.sequence, index as u64 + 1);
        assert_eq!(view.header.origin, origin);
        assert_eq!(view.header.principal, principal);
        assert_eq!(view.body().unwrap(), changes);
    }

    let _ = fs::remove_file(path);
}
