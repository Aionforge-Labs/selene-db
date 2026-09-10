//! Torn tails are repaired; corruption before the tail refuses.
//!
//! The tests that matter here are the ones asserting the file is UNCHANGED
//! after a refusal. An implementation that returns the right error after
//! truncating has already destroyed the committed frames and the evidence
//! needed to recover them by other means, which is the defect itself.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use selene_core::{Change, HlcTimestamp, LabelSet, NodeId, Origin, PropertyMap, db_string};

use super::*;
use crate::{WAL_FILE_HEADER_LEN, WalConfig, WalWriter};

fn temp_path(_name: &str) -> selene_testing::PersistenceTestPath {
    selene_testing::PersistenceTestPath::new()
}

fn changes() -> Vec<Change> {
    vec![Change::NodeCreated {
        id: NodeId::new(1),
        labels: LabelSet::single(db_string("waltail.node").unwrap()),
        properties: PropertyMap::new(),
    }]
}

/// Write `count` flushed frames and return each frame's start offset.
fn seed(path: &std::path::Path, count: u64) -> Vec<u64> {
    seed_with_principal(path, count, None)
}

fn seed_with_principal(path: &std::path::Path, count: u64, principal: Option<&[u8]>) -> Vec<u64> {
    seed_with_origin(path, count, principal, Origin::Local)
}

fn seed_with_origin(
    path: &std::path::Path,
    count: u64,
    principal: Option<&[u8]>,
    origin: Origin,
) -> Vec<u64> {
    let mut writer = WalWriter::open(path, WalConfig::default()).unwrap();
    let mut starts = Vec::new();
    for sequence in 1..=count {
        starts.push(writer.committed_offset());
        writer
            .append(
                HlcTimestamp::new(sequence, 0),
                origin,
                principal.map(std::sync::Arc::from),
                &changes(),
            )
            .unwrap();
    }
    writer.flush().unwrap();
    starts
}

/// A replicated origin, which sets [`FLAG_REPLICATED_ORIGIN`] and writes a
/// 16-byte provenance tail at the head of the extent.
fn replicated() -> Origin {
    Origin::Replicated {
        source_node_id: NodeId::new(7),
        source_seq: 42,
    }
}

fn bytes_of(path: &std::path::Path) -> Vec<u8> {
    fs::read(path).unwrap()
}

fn flip_bit(path: &std::path::Path, offset: u64) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0b0000_0001;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&byte).unwrap();
}

fn len_of(path: &std::path::Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

/// `WalWriter` is not `Debug`, so `expect_err` is unavailable.
fn expect_open_err(path: &std::path::Path, context: &str) -> PersistError {
    let Err(error) = WalWriter::open(path, WalConfig::default()) else {
        panic!("{context}");
    };
    error
}

/// The #1089 defect. Corrupting a frame in the MIDDLE of the log used to
/// truncate every committed frame after it and return `Ok`.
#[test]
fn midlog_payload_corruption_refuses_and_preserves_the_log() {
    let path = temp_path("midlog-payload");
    let starts = seed(&path, 5);
    let before = len_of(&path);
    // Past frame 2's 40-byte prefix, into its payload.
    flip_bit(&path, starts[1] + FIXED_PREFIX_FOR_TEST + 1);

    let error = expect_open_err(
        &path,
        "a corrupt frame with four frames after it is not a torn tail",
    );
    let PersistError::WalMidLogCorruption {
        offset,
        trailing_bytes,
        source,
    } = &error
    else {
        panic!("expected a refusal, got {error:?}");
    };
    assert_eq!(*offset, starts[1]);
    assert_eq!(*trailing_bytes, before - starts[1]);
    assert!(
        matches!(**source, PersistError::ChecksumMismatch { sequence: 2 }),
        "the refusal must carry the underlying cause, got {source:?}"
    );
    assert_eq!(
        len_of(&path),
        before,
        "a refusal must not truncate: the committed frames and the evidence must survive"
    );
    let _ = fs::remove_file(path);
}

/// The #1090 half of the same defect: a flipped length field used to make a
/// mid-file frame look truncated, which routed it into the same silent
/// truncation. The prefix checksum now catches it before the length is used.
#[test]
fn midlog_header_corruption_refuses_and_preserves_the_log() {
    let path = temp_path("midlog-header");
    let starts = seed(&path, 5);
    let before = len_of(&path);
    // Frame 2's payload_len lives at prefix offset 4.
    flip_bit(&path, starts[1] + 4);

    let error = expect_open_err(
        &path,
        "a corrupt length in the middle of the log is not a torn tail",
    );
    let PersistError::WalMidLogCorruption { offset, source, .. } = &error else {
        panic!("expected a refusal, got {error:?}");
    };
    assert_eq!(*offset, starts[1]);
    assert!(
        matches!(
            **source,
            PersistError::WalHeaderChecksumMismatch { offset } if offset == starts[1]
        ),
        "expected a header checksum failure naming frame 2, got {source:?}"
    );
    assert_eq!(len_of(&path), before, "a refusal must not truncate");
    let _ = fs::remove_file(path);
}

/// Ordinary crash recovery: the file ends part-way through the final frame.
#[test]
fn short_final_frame_is_repaired() {
    let path = temp_path("short-final");
    let starts = seed(&path, 3);
    let keep = starts[2] + 8;
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(keep)
        .unwrap();

    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(writer.last_sequence(), 2);
    let repair = writer.tail_repair().expect("a repair must be reported");
    assert_eq!(repair.reason, WalTailReason::ShortFrame);
    assert_eq!(repair.offset, starts[2]);
    assert_eq!(len_of(&path), starts[2]);
    let _ = fs::remove_file(path);
}

/// A complete final frame that fails validation with nothing after it. It was
/// never acknowledged, so discarding exactly it is correct — and this is the
/// case a zero-fill-only rule would wrongly refuse, bricking crash recovery.
#[test]
fn corrupt_final_frame_is_repaired_not_refused() {
    let path = temp_path("corrupt-final");
    let starts = seed(&path, 3);
    flip_bit(&path, starts[2] + FIXED_PREFIX_FOR_TEST + 1);

    let writer = WalWriter::open(&path, WalConfig::default())
        .expect("the last frame has nothing after it, so it is a tail");
    assert_eq!(writer.last_sequence(), 2);
    assert_eq!(
        writer.tail_repair().expect("repair").reason,
        WalTailReason::CorruptFinalFrame
    );
    assert_eq!(len_of(&path), starts[2]);
    let _ = fs::remove_file(path);
}

/// The same tail, damaged one byte earlier — in the principal rather than the
/// payload — must reach the same verdict.
///
/// It did not. Every failure raised after the prefix checksum passed was routed
/// into the no-trustworthy-extent fallback, whose zero probe sees the intact
/// 40-byte prefix, calls it corruption, and refuses. So a store with nothing
/// wrong but an unacknowledged tail would not open, while the identical bit flip
/// one byte later opened fine. The principal is written on every
/// `commit_with_principal`, so this is not an exotic frame shape.
#[test]
fn corrupt_principal_in_the_final_frame_is_repaired_not_refused() {
    let path = temp_path("corrupt-final-principal");
    let starts = seed_with_principal(&path, 3, Some(b"alice"));
    // Past the fixed prefix is the extent: no provenance tail for a local
    // origin, so this lands directly in the principal bytes.
    flip_bit(&path, starts[2] + FIXED_PREFIX_FOR_TEST);

    let writer = WalWriter::open(&path, WalConfig::default())
        .expect("a corrupt principal in the LAST frame is still a tail");
    assert_eq!(writer.last_sequence(), 2);
    assert_eq!(
        writer.tail_repair().expect("repair").reason,
        WalTailReason::CorruptFinalFrame
    );
    assert_eq!(len_of(&path), starts[2]);
    let _ = fs::remove_file(path);
}

/// ...and mid-log, the same damage must still refuse. The stage-2 verdict has to
/// depend on the frame's position, not on which stage raised the error.
#[test]
fn corrupt_principal_before_the_tail_refuses() {
    let path = temp_path("midlog-principal");
    let starts = seed_with_principal(&path, 5, Some(b"alice"));
    let before = len_of(&path);
    flip_bit(&path, starts[1] + FIXED_PREFIX_FOR_TEST);

    let error = expect_open_err(
        &path,
        "a corrupt principal with frames after it is corruption",
    );
    assert!(
        matches!(
            error,
            PersistError::WalMidLogCorruption { offset, .. } if offset == starts[1]
        ),
        "expected a refusal naming frame 2, got {error:?}"
    );
    assert_eq!(len_of(&path), before, "a refusal must not truncate");
    let _ = fs::remove_file(path);
}

/// The provenance tail is the one extent field with no local-origin producer,
/// so every other seed in this file writes a zero-length one. It sits ahead of
/// the principal in the extent and is covered by the extent checksum rather
/// than the prefix checksum, which is a different branch from the header cases
/// above.
#[test]
fn corrupt_replicated_provenance_before_the_tail_refuses() {
    let path = temp_path("midlog-provenance");
    let starts = seed_with_origin(&path, 5, Some(b"alice"), replicated());
    // The extent opens with the 16-byte provenance tail; the principal follows.
    flip_bit(&path, starts[1] + FIXED_PREFIX_FOR_TEST);
    // Baseline taken AFTER the flip: the damaged file is what recovery must
    // hand back untouched, corruption included, so the evidence survives.
    let corrupted = bytes_of(&path);

    let error = expect_open_err(
        &path,
        "corrupt provenance with frames after it is corruption, not a tail",
    );
    assert!(
        matches!(
            error,
            PersistError::WalMidLogCorruption { offset, .. } if offset == starts[1]
        ),
        "expected a refusal naming frame 2, got {error:?}"
    );
    assert_eq!(
        bytes_of(&path),
        corrupted,
        "a refusal must leave the file byte-identical, not merely the same length"
    );
    let _ = fs::remove_file(path);
}

/// The tail twin: the same damage in the LAST frame is an unacknowledged write
/// and must be repaired, so the verdict tracks position rather than which field
/// failed.
#[test]
fn corrupt_replicated_provenance_in_the_final_frame_is_repaired() {
    let path = temp_path("final-provenance");
    let starts = seed_with_origin(&path, 3, Some(b"alice"), replicated());
    flip_bit(&path, starts[2] + FIXED_PREFIX_FOR_TEST);

    let writer = WalWriter::open(&path, WalConfig::default())
        .expect("corrupt provenance in the LAST frame is still a tail");
    assert_eq!(writer.last_sequence(), 2);
    assert_eq!(
        writer.tail_repair().expect("repair").reason,
        WalTailReason::CorruptFinalFrame
    );
    assert_eq!(len_of(&path), starts[2]);
    let _ = fs::remove_file(path);
}

/// Every fixed-header field in a non-final frame must refuse, including the
/// prefix checksum itself at offset 0.
///
/// The decode-level sweep in `entry_header` proves each field round-trips and
/// is covered; this proves the coverage actually reaches the recovery verdict,
/// which is the property #1114 asks about. A field that silently kept its
/// meaning under a bit flip would let a damaged interior frame be classified as
/// a torn tail and discard every committed frame after it.
#[test]
fn every_fixed_header_field_flip_before_the_tail_refuses() {
    for (field_offset, field) in [
        (0_u64, "prefix checksum"),
        (4, "payload length"),
        (8, "payload checksum"),
        (12, "sequence"),
        (20, "hlc seconds"),
        (28, "hlc subseconds"),
        (32, "flags"),
        (33, "reserved"),
        (34, "principal length"),
        (36, "extent checksum"),
    ] {
        let path = temp_path(&format!("midlog-field-{field_offset}"));
        let starts = seed_with_principal(&path, 5, Some(b"alice"));
        flip_bit(&path, starts[1] + field_offset);
        let corrupted = bytes_of(&path);

        let error = expect_open_err(
            &path,
            &format!("a flipped {field} with four frames after it is not a torn tail"),
        );
        assert!(
            matches!(
                error,
                PersistError::WalMidLogCorruption { offset, .. } if offset == starts[1]
            ),
            "flipping {field} must refuse naming frame 2, got {error:?}"
        );
        assert_eq!(
            bytes_of(&path),
            corrupted,
            "a refusal on {field} must leave the file byte-identical"
        );
        let _ = fs::remove_file(path);
    }
}

/// A short extending write leaves zeros. The prefix checksum of a zero prefix
/// does not match zero, so this reaches the classifier rather than decoding as
/// a well-formed sequence-0 frame.
#[test]
fn zero_filled_tail_is_repaired() {
    let path = temp_path("zero-tail");
    let starts = seed(&path, 3);
    let end = len_of(&path);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[0_u8; 128])
        .unwrap();

    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(writer.last_sequence(), 3, "all three frames must survive");
    let repair = writer.tail_repair().expect("repair");
    assert_eq!(repair.reason, WalTailReason::ZeroFilledTail);
    assert_eq!(repair.discarded_bytes, 128);
    assert_eq!(len_of(&path), end);
    assert!(starts.len() == 3);
    let _ = fs::remove_file(path);
}

/// Zeros in the MIDDLE are not a tail. This is the case that separates
/// "everything from here is zero" from "this frame is zero".
#[test]
fn zeroed_frame_before_more_data_refuses() {
    let path = temp_path("zero-midlog");
    let starts = seed(&path, 5);
    let before = len_of(&path);
    {
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(starts[1])).unwrap();
        let span = usize::try_from(starts[2] - starts[1]).unwrap();
        file.write_all(&vec![0_u8; span]).unwrap();
    }

    let error = expect_open_err(
        &path,
        "zeros with committed frames after them are corruption, not a tear",
    );
    assert!(
        matches!(
            error,
            PersistError::WalMidLogCorruption { offset, .. } if offset == starts[1]
        ),
        "expected a refusal naming the zeroed frame, got {error:?}"
    );
    assert_eq!(len_of(&path), before, "a refusal must not truncate");
    let _ = fs::remove_file(path);
}

/// A clean log reports no repair, so `tail_repair()` is a signal and not noise.
#[test]
fn clean_log_reports_no_repair() {
    let path = temp_path("clean");
    seed(&path, 3);
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(writer.tail_repair(), None);
    assert_eq!(writer.last_sequence(), 3);
    assert!(writer.committed_offset() > WAL_FILE_HEADER_LEN as u64);
    let _ = fs::remove_file(path);
}

/// Pinned locally so a prefix-size change fails this file loudly rather than
/// silently moving every offset these tests poke.
const FIXED_PREFIX_FOR_TEST: u64 = 40;

#[test]
fn test_prefix_constant_matches_the_format() {
    assert_eq!(
        FIXED_PREFIX_FOR_TEST as usize,
        crate::entry_header::FIXED_ENTRY_HEADER_BYTES
    );
}
