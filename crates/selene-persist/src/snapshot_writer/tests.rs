use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::{SnapshotReader, snapshot_path, snapshot_tmp_path};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-snapshot-writer-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    dir
}

fn config(dir: PathBuf, sequence: u64, compression: SectionCompression) -> SnapshotConfig {
    SnapshotConfig {
        dir,
        sequence,
        compression,
        fsync: true,
    }
}

#[test]
fn empty_snapshot_round_trips() {
    let dir = temp_dir("empty");
    let outcome = SnapshotBuilder::new(config(dir.clone(), 1, SectionCompression::None))
        .unwrap()
        .finalize()
        .unwrap();
    assert_eq!(outcome.snapshot_seq, 1);
    let path = snapshot_path(&dir, 1);
    let mut reader = SnapshotReader::open(&path).unwrap();
    reader.verify_body_hash().unwrap();
    assert!(reader.sections().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn single_section_round_trips_per_section_compressed() {
    let dir = temp_dir("compressed");
    let mut builder = SnapshotBuilder::new(config(
        dir.clone(),
        2,
        SectionCompression::PerSection { level: 1 },
    ))
    .unwrap();
    builder
        .add_section(*b"CORE", *b"META", vec![7_u8; 1024])
        .unwrap();
    let outcome = builder.finalize().unwrap();
    assert_eq!(outcome.section_count, 1);
    let path = snapshot_path(&dir, 2);
    let mut reader = SnapshotReader::open(&path).unwrap();
    assert!(reader.header().is_section_compressed());
    assert_eq!(
        reader.read_section(*b"CORE", *b"META").unwrap(),
        vec![7_u8; 1024]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn single_section_round_trips_uncompressed() {
    let dir = temp_dir("raw");
    let mut builder =
        SnapshotBuilder::new(config(dir.clone(), 3, SectionCompression::None)).unwrap();
    builder
        .add_section(*b"CORE", *b"NODE", b"nodes".to_vec())
        .unwrap();
    let outcome = builder.finalize().unwrap();
    assert_eq!(
        outcome.body_hash,
        SnapshotReader::open(&snapshot_path(&dir, 3))
            .unwrap()
            .header()
            .body_hash
    );
    let path = snapshot_path(&dir, 3);
    let mut reader = SnapshotReader::open(&path).unwrap();
    assert!(!reader.header().is_section_compressed());
    assert_eq!(reader.read_section(*b"CORE", *b"NODE").unwrap(), b"nodes");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn duplicate_provider_sub_is_rejected() {
    let dir = temp_dir("dup");
    let mut builder =
        SnapshotBuilder::new(config(dir.clone(), 4, SectionCompression::None)).unwrap();
    builder.add_section(*b"CORE", *b"META", vec![]).unwrap();
    assert!(matches!(
        builder.add_section(*b"CORE", *b"META", vec![]),
        Err(PersistError::DuplicateSection { provider, sub })
            if provider == *b"CORE" && sub == *b"META"
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn section_size_boundary_uses_validator_without_allocating() {
    crate::section::validate_section_payload_len(crate::MAX_SECTION_PAYLOAD_BYTES).unwrap();
    assert!(matches!(
        crate::section::validate_section_payload_len(crate::MAX_SECTION_PAYLOAD_BYTES + 1),
        Err(PersistError::SectionTooLarge { .. })
    ));
}

#[test]
fn compressed_sections_parallel_gate_uses_payload_floor() {
    let single_large = [RawSection {
        provider: *b"CORE",
        sub: *b"DATA",
        payload: vec![0_u8; PARALLEL_SNAPSHOT_COMPRESSION_MIN_BYTES],
    }];
    assert!(!should_prepare_compressed_sections_parallel(&single_large));

    let below_floor = [
        RawSection {
            provider: *b"CORE",
            sub: *b"DATA",
            payload: vec![0_u8; PARALLEL_SNAPSHOT_COMPRESSION_MIN_BYTES - 1],
        },
        RawSection {
            provider: *b"CORE",
            sub: *b"IDX0",
            payload: Vec::new(),
        },
    ];
    assert!(!should_prepare_compressed_sections_parallel(&below_floor));

    let at_floor = [
        RawSection {
            provider: *b"CORE",
            sub: *b"DATA",
            payload: vec![0_u8; PARALLEL_SNAPSHOT_COMPRESSION_MIN_BYTES],
        },
        RawSection {
            provider: *b"CORE",
            sub: *b"IDX0",
            payload: Vec::new(),
        },
    ];
    assert!(should_prepare_compressed_sections_parallel(&at_floor));
}

#[test]
fn stale_fixed_tmp_does_not_prevent_snapshot_retry() {
    let dir = temp_dir("stale-tmp");
    fs::write(snapshot_tmp_path(&dir, 5), b"partial").unwrap();
    let outcome = SnapshotBuilder::new(config(dir.clone(), 5, SectionCompression::None))
        .unwrap()
        .finalize()
        .unwrap();
    assert_eq!(outcome.snapshot_seq, 5);
    assert!(snapshot_tmp_path(&dir, 5).exists());
    let mut reader = SnapshotReader::open(&snapshot_path(&dir, 5)).unwrap();
    reader.verify_body_hash().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn final_snapshot_path_already_exists_is_rejected_atomically() {
    let dir = temp_dir("final-exists");
    fs::write(snapshot_path(&dir, 6), b"existing").unwrap();
    let err = SnapshotBuilder::new(config(dir.clone(), 6, SectionCompression::None))
        .unwrap()
        .finalize()
        .unwrap_err();
    assert!(matches!(
        err,
        PersistError::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert!(!snapshot_tmp_path(&dir, 6).exists());
    assert_eq!(fs::read(snapshot_path(&dir, 6)).unwrap(), b"existing");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn snapshot_finalize_same_sequence_is_race_safe() {
    use std::sync::{Arc as StdArc, Barrier};

    let dir = StdArc::new(temp_dir("same-seq-race"));
    let owner = crate::StoreWriter::acquire(&crate::StoreDirectory::open(&dir).unwrap()).unwrap();
    const THREADS: usize = 6;
    let barrier = StdArc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let dir = StdArc::clone(&dir);
            let barrier = StdArc::clone(&barrier);
            let authority = owner.clone();
            std::thread::spawn(move || {
                let mut builder =
                    SnapshotBuilder::new(config((*dir).clone(), 20, SectionCompression::None))
                        .unwrap();
                builder
                    .add_section(*b"CORE", *b"META", vec![i as u8; 64])
                    .unwrap();
                barrier.wait();
                match builder.finalize_with_authority(&authority) {
                    Ok(_) => true,
                    Err(PersistError::Io(error))
                        if error.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        false
                    }
                    Err(other) => panic!("unexpected finalize error: {other:?}"),
                }
            })
        })
        .collect();

    let wins = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1, "exactly one same-sequence finalize may win");

    let path = snapshot_path(&dir, 20);
    assert!(path.exists());
    let mut reader = SnapshotReader::open(&path).unwrap();
    reader.verify_body_hash().unwrap();
    assert!(fs::read_dir(dir.as_path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("snapshot.20.snap.tmp.")
    }));
    let _ = fs::remove_dir_all(dir.as_path());
}

#[test]
fn byte_identical_uncompressed_writes() {
    let dir = temp_dir("identical");
    for sequence in [7, 8] {
        let mut builder =
            SnapshotBuilder::new(config(dir.clone(), sequence, SectionCompression::None)).unwrap();
        builder
            .add_section(*b"CORE", *b"META", b"meta".to_vec())
            .unwrap();
        builder
            .add_section(*b"CORE", *b"NODE", b"nodes".to_vec())
            .unwrap();
        builder.finalize().unwrap();
    }
    let left = fs::read(snapshot_path(&dir, 7)).unwrap();
    let right = fs::read(snapshot_path(&dir, 8)).unwrap();
    assert_eq!(left, right);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn snapshot_par_iter_roundtrip() {
    let dir = temp_dir("par-iter");
    let sections = [
        (*b"CORE", *b"META", vec![1_u8; 257 * 1024]),
        (*b"CORE", *b"NODE", vec![2_u8; 256 * 1024]),
        (*b"CORE", *b"EDGE", vec![3_u8; 256 * 1024]),
        (*b"DEMO", *b"SUBT", vec![4_u8; 256 * 1024]),
        (*b"AUX1", *b"LIST", vec![5_u8; 64 * 1024]),
    ];
    let mut builder = SnapshotBuilder::new(config(
        dir.clone(),
        9,
        SectionCompression::PerSection { level: 1 },
    ))
    .unwrap();
    for (provider, sub, payload) in &sections {
        builder
            .add_section(*provider, *sub, payload.clone())
            .unwrap();
    }
    let outcome = builder.finalize().unwrap();
    assert_eq!(outcome.section_count, sections.len() as u32);

    let mut reader = SnapshotReader::open(&snapshot_path(&dir, 9)).unwrap();
    reader.verify_body_hash().unwrap();
    assert!(reader.header().is_section_compressed());
    assert_eq!(reader.sections().len(), sections.len());
    let entries = reader.sections().to_vec();
    for ((provider, sub, expected), entry) in sections.iter().zip(&entries) {
        assert_eq!(entry.provider, *provider);
        assert_eq!(entry.sub, *sub);
        assert_eq!(
            reader.read_section(*provider, *sub).unwrap(),
            expected.as_slice()
        );
    }
    let _ = fs::remove_dir_all(dir);
}
