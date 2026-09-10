#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use selene_persist::{
    SectionCompression, SnapshotBuilder, SnapshotConfig, SnapshotReader, snapshot_path,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-snapshot-outcome-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("temp dir is created");
    dir
}

#[test]
fn finalize_returns_snapshot_metadata_matching_reader_hash() {
    let dir = temp_dir("metadata");
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.clone(),
        sequence: 9,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    builder
        .add_section(*b"CORE", *b"META", b"metadata".to_vec())
        .expect("first section adds");
    builder
        .add_section(*b"CORE", *b"NODE", b"nodes".to_vec())
        .expect("second section adds");

    let outcome = builder.finalize().expect("snapshot finalizes");
    let path = snapshot_path(&dir, outcome.snapshot_seq);
    let mut reader = SnapshotReader::open(&path).expect("snapshot opens");

    assert_eq!(outcome.snapshot_seq, 9);
    assert_eq!(outcome.section_count, 2);
    assert_eq!(outcome.body_hash, reader.header().body_hash);
    reader.verify_body_hash().expect("body hash verifies");

    let _ = fs::remove_dir_all(dir);
}
