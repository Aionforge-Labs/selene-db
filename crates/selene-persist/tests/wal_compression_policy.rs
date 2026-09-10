#![allow(missing_docs)]

use std::fs;
use std::sync::Arc;

use selene_core::{Change, HlcTimestamp, LabelSet, NodeId, Origin, PropertyMap, Value, db_string};
use selene_persist::{
    COMPRESS_THRESHOLD, DEFAULT_WAL_FILE_NAME, WalCompression, WalConfig, WalReader, WalWriter,
};

fn temp_path(_name: &str) -> selene_testing::PersistenceTestPath {
    selene_testing::PersistenceTestPath::new()
}

fn byte_changes(len: usize) -> Vec<Change> {
    vec![Change::NodeCreated {
        id: NodeId::new(1),
        labels: LabelSet::single(db_string("writer.node").unwrap()),
        properties: PropertyMap::from_pairs([(
            db_string("writer.payload").unwrap(),
            Value::Bytes(Arc::from(vec![7_u8; len])),
        )])
        .unwrap(),
    }]
}

#[test]
fn open_with_compression_applies_payload_policy() {
    let path = temp_path("compression-policy");
    let changes = byte_changes(COMPRESS_THRESHOLD * 4);
    {
        let mut writer = WalWriter::open_with_compression(
            &path,
            WalConfig::default(),
            WalCompression::disabled(),
        )
        .unwrap();
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes)
            .unwrap();
    }
    let reader = WalReader::open(&path).unwrap();
    let view = reader.iterate(|_| true).unwrap().next().unwrap().unwrap();
    assert!(!view.header.is_payload_compressed());
    assert_eq!(view.body().unwrap(), changes);
    let _ = fs::remove_file(path);
}

#[test]
fn default_open_uses_configured_compression_threshold() {
    let path = temp_path("compression-default");
    let changes = byte_changes(COMPRESS_THRESHOLD * 4);
    {
        let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes)
            .unwrap();
    }
    let reader = WalReader::open(&path).unwrap();
    let view = reader.iterate(|_| true).unwrap().next().unwrap().unwrap();
    assert!(view.header.is_payload_compressed());
    assert_eq!(view.body().unwrap(), changes);
    let _ = fs::remove_file(path);
}

#[test]
fn default_wal_file_name_remains_available_with_policy_api() {
    assert_eq!(DEFAULT_WAL_FILE_NAME, "wal.log");
    assert_eq!(
        WalCompression::default().threshold_bytes(),
        Some(COMPRESS_THRESHOLD)
    );
}
