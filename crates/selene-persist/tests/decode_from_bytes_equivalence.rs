//! PERSIST-26: the `from_bytes`-style slice decoders must agree byte-for-byte
//! with the file-path readers on valid input, and never panic on arbitrary
//! input. These are the cross-format equivalence + no-panic acceptance tests;
//! the targeted malformed-input regressions live next to each decoder.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, LabelSet, NodeId, Origin, PropertyMap, Value, db_string};
use selene_persist::{
    AuditLog, AuditRecord, MANIFEST_FILE_NAME, Manifest, SectionCompression, SnapshotBuilder,
    SnapshotConfig, SnapshotReader, WalConfig, WalReader, WalWriter, snapshot_path,
};

/// Unique temp path under the OS temp dir (no two tests collide on a name).
fn unique(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "selene-persist-eq-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

fn changes(id: u64) -> Vec<Change> {
    vec![Change::NodeCreated {
        id: NodeId::new(id),
        labels: LabelSet::single(db_string("eq.node").unwrap()),
        properties: PropertyMap::from_pairs([(db_string("eq.p").unwrap(), Value::Int(id as i64))])
            .unwrap(),
    }]
}

/// A change whose encoded payload exceeds the compression threshold, so the WAL
/// entry is zstd-compressed on disk and `body()` exercises the bounded
/// decompress path on both the file and slice readers.
fn big_change() -> Vec<Change> {
    vec![Change::NodeCreated {
        id: NodeId::new(99),
        labels: LabelSet::single(db_string("eq.big").unwrap()),
        properties: PropertyMap::from_pairs([(
            db_string("eq.bytes").unwrap(),
            Value::Bytes(Arc::from(vec![0xAB_u8; 512])),
        )])
        .unwrap(),
    }]
}

#[test]
fn manifest_file_path_and_slice_decode_agree() {
    let dir = unique("manifest");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = Manifest {
        live_snapshot_seq: 7,
        active_wal_header_seq: 6,
        compaction_epoch: 0,
        active_wal: "wal.log".to_owned(),
        archived_wal_seqs: vec![1, 2, 3],
    };
    manifest.write_atomic(&dir).unwrap();

    let via_read = Manifest::read(&dir).unwrap().unwrap();
    let raw = std::fs::read(dir.join(MANIFEST_FILE_NAME)).unwrap();
    let via_decode = Manifest::decode(&raw).unwrap();
    assert_eq!(via_read, via_decode);
    assert_eq!(via_read, manifest);
    // In-memory encode -> decode round-trips identically (no file-shell transform).
    assert_eq!(
        Manifest::decode(&manifest.encode().unwrap()).unwrap(),
        manifest
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn wal_file_path_and_slice_iteration_agree() {
    let path = unique("wal");
    {
        let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes(1))
            .unwrap();
        writer
            .append(
                HlcTimestamp::new(2, 0),
                Origin::Local,
                Some(Arc::from(b"alice".as_slice())),
                &changes(2),
            )
            .unwrap();
        // A compressed entry exercises the bounded zstd decompress on both paths.
        writer
            .append(HlcTimestamp::new(3, 0), Origin::Local, None, &big_change())
            .unwrap();
    }

    let via_file: Vec<_> = WalReader::open(&path)
        .unwrap()
        .iterate(|_| true)
        .unwrap()
        .map(|result| result.unwrap().into_entry().unwrap())
        .collect();

    let bytes = std::fs::read(&path).unwrap();
    let via_slice: Vec<_> = WalReader::from_bytes(&bytes)
        .unwrap()
        .map(|result| result.unwrap().into_entry().unwrap())
        .collect();

    assert_eq!(via_file, via_slice);
    assert_eq!(via_file.len(), 3);
    assert_eq!(via_file[0].changes, changes(1));
    assert_eq!(via_file[2].changes, big_change());

    let _ = std::fs::remove_file(path);
}

#[test]
fn audit_file_path_and_slice_decode_agree() {
    let path = unique("audit");
    {
        let mut log = AuditLog::open(&path).unwrap();
        for (nanos, kind, payload) in [
            (10_u64, 0_u16, Vec::new()),
            (20, 1, b"event".to_vec()),
            (30, 2, vec![7_u8; 4096]),
        ] {
            log.append(&AuditRecord {
                recorded_at_unix_nanos: nanos,
                kind,
                payload,
            })
            .unwrap();
        }
    }

    let via_read = AuditLog::read_all(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let via_decode = AuditLog::decode_all(&bytes).unwrap();

    assert_eq!(via_read, via_decode);
    assert_eq!(via_read.len(), 3);
    assert_eq!(via_read[1].payload, b"event");
    assert_eq!(via_read[2].payload, vec![7_u8; 4096]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn snapshot_envelope_open_and_decode_agree() {
    for (sequence, compression) in [
        (1_u64, SectionCompression::None),
        (2, SectionCompression::PerSection { level: 1 }),
    ] {
        let dir = unique("snapshot");
        std::fs::create_dir_all(&dir).unwrap();
        let mut builder = SnapshotBuilder::new(SnapshotConfig {
            dir: dir.clone(),
            sequence,
            compression,
            fsync: true,
        })
        .unwrap();
        builder
            .add_section(*b"CORE", *b"META", vec![1_u8; 100])
            .unwrap();
        builder
            .add_section(*b"CORE", *b"NODE", vec![2_u8; 2048])
            .unwrap();
        builder.finalize().unwrap();

        let path = snapshot_path(&dir, sequence);
        let reader = SnapshotReader::open(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let (header, sections) = SnapshotReader::decode_envelope(&bytes).unwrap();

        assert_eq!(&header, reader.header());
        assert_eq!(sections.as_slice(), reader.sections());
        assert_eq!(sections.len(), 2);

        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn arbitrary_bytes_never_panic_across_all_decoders() {
    // The trivial corpus a fuzzer starts from: empty, single byte, all-ones,
    // bare magics, and zeroed/garbage runs. Each must yield Ok or a typed
    // PersistError — never a panic.
    let inputs: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0_u8],
        vec![0xFF_u8; 3],
        b"SLMF".to_vec(),
        b"SLDB".to_vec(),
        b"SLAU".to_vec(),
        b"SLSN".to_vec(),
        vec![0_u8; 64],
        vec![0xAB_u8; 1000],
    ];
    for input in &inputs {
        let _ = Manifest::decode(input);
        let _ = AuditLog::decode_all(input);
        let _ = SnapshotReader::decode_envelope(input);
        if let Ok(stream) = WalReader::from_bytes(input) {
            for item in stream {
                if item.is_err() {
                    break;
                }
            }
        }
    }
}
