use super::*;

fn context() -> Context {
    let mut id = [0; 16];
    id[6] = 0x40;
    id[8] = 0x80;
    Context {
        store: StoreId::from_bytes(id).unwrap(),
        epoch: StoreEpoch::new(1).unwrap(),
        sequence: 1,
        segment: [0; 32],
        previous: [0; 32],
    }
}

fn fixture(body: &[u8], codec: u8, expanded: u64) -> Vec<u8> {
    // Independent assembly, with documented literal offsets. Does not call encode.
    let mut bytes = vec![0; 160];
    bytes[..12].copy_from_slice(b"SLTXN2\0\0\x02\0\0\0");
    bytes[12] = codec;
    bytes[16..24].copy_from_slice(&(body.len() as u64).to_le_bytes());
    bytes[24..32].copy_from_slice(&expanded.to_le_bytes());
    bytes[32] = 1;
    bytes[46] = 0x40;
    bytes[48] = 0x80;
    bytes[56] = 1;
    let hash = blake3::hash(&bytes[..128]);
    bytes[128..160].copy_from_slice(hash.as_bytes());
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(b"SLTXEND2");
    bytes.extend_from_slice(blake3::hash(&bytes).as_bytes());
    bytes
}
#[test]
fn independent_raw_frame_and_every_partial_boundary() {
    let body = [2, 254, 255, 255, 255, 255, 255, 255, 255];
    let bytes = fixture(&body, 0, 9);
    assert_eq!(bytes.len(), 209);
    assert_eq!(
        encode(&body, context(), Compression::Raw, MAX_PAYLOAD).unwrap(),
        bytes
    );
    let Decoded::Complete {
        body: decoded,
        consumed,
        ..
    } = decode(&bytes, context(), Boundary::SealedEnd, MAX_PAYLOAD).unwrap()
    else {
        panic!("complete")
    };
    assert_eq!(&*decoded, body);
    assert_eq!(consumed, bytes.len());
    for cut in 0..bytes.len() {
        assert!(
            matches!(
                decode(&bytes[..cut], context(), Boundary::UnsealedEnd, MAX_PAYLOAD),
                Ok(Decoded::Incomplete { .. })
            ),
            "cut {cut}"
        );
        for boundary in [Boundary::Interior, Boundary::SealedEnd] {
            assert_eq!(
                decode(&bytes[..cut], context(), boundary, MAX_PAYLOAD).unwrap_err(),
                FrameError::CorruptIncomplete,
                "cut {cut}"
            );
        }
    }
}
#[test]
fn complete_bitflip_is_not_a_torn_tail() {
    let valid = fixture(&[0; 8], 0, 8);
    for offset in 0..valid.len() {
        for bit in 0..8 {
            let mut bytes = valid.clone();
            bytes[offset] ^= 1 << bit;
            assert!(
                decode(&bytes, context(), Boundary::UnsealedEnd, MAX_PAYLOAD).is_err(),
                "offset {offset} bit {bit}"
            );
        }
    }
}
#[test]
fn context_and_checked_header_lengths_before_body() {
    let bytes = fixture(&[], 0, 0);
    let mut wrong = context();
    wrong.sequence = 2;
    assert_eq!(
        decode(&bytes, wrong, Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Sequence {
            expected: 2,
            observed: 1
        }
    );
    wrong = context();
    wrong.epoch = StoreEpoch::new(2).unwrap();
    assert_eq!(
        decode(&bytes, wrong, Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Epoch
    );
    wrong = context();
    wrong.segment[0] = 1;
    assert_eq!(
        decode(&bytes, wrong, Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Segment
    );
    wrong = context();
    wrong.previous[0] = 1;
    assert_eq!(
        decode(&bytes, wrong, Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Origin
    );
    let mut bytes = bytes;
    bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    let hash = blake3::hash(&bytes[..128]);
    bytes[128..160].copy_from_slice(hash.as_bytes());
    assert_eq!(
        decode(&bytes[..160], context(), Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Limit
    );
}
#[test]
fn automatic_threshold_and_exact_single_zstd_frame() {
    for size in [0, 4095, 4096, 16_384, 1_048_576] {
        let body = vec![b'x'; size];
        let bytes = encode(&body, context(), Compression::Auto, MAX_PAYLOAD).unwrap();
        assert_eq!(bytes[12], u8::from(size >= 4096));
        let Decoded::Complete { body: decoded, .. } =
            decode(&bytes, context(), Boundary::SealedEnd, MAX_PAYLOAD).unwrap()
        else {
            panic!("complete")
        };
        assert_eq!(&*decoded, body);
    }
    let compressed = zstd::encode_all(&b"hello"[..], 1).unwrap();
    let mut concatenated = compressed.clone();
    concatenated.extend_from_slice(&compressed);
    for body in [concatenated, [compressed.clone(), vec![0]].concat()] {
        let bytes = fixture(&body, 1, 5);
        assert!(matches!(
            decode(&bytes, context(), Boundary::UnsealedEnd, MAX_PAYLOAD),
            Err(FrameError::Invalid("Zstd consumption/length"))
        ));
    }
    let bytes = fixture(&compressed, 1, 4);
    assert_eq!(
        decode(&bytes, context(), Boundary::SealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Limit
    );
    let bytes = fixture(&compressed, 1, 6);
    assert_eq!(
        decode(&bytes, context(), Boundary::SealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Invalid("Zstd consumption/length")
    );
}

#[test]
fn zstd_history_dictionary_and_skippable_envelopes_are_bounded() {
    // Standard Zstd frame, no FCS/dictionary, 16 MiB declared history, one raw final
    // block containing x. This exceeds our independent eight-MiB history budget.
    let large_history = [0x28, 0xb5, 0x2f, 0xfd, 0, 0x70, 9, 0, 0, b'x'];
    let bytes = fixture(&large_history, 1, 1);
    assert_eq!(
        decode(&bytes, context(), Boundary::SealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Compression
    );
    for body in [
        &[0x28, 0xb5, 0x2f, 0xfd, 1, 0, 1][..],
        &[0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0],
    ] {
        let bytes = fixture(body, 1, 0);
        assert_eq!(
            decode(&bytes, context(), Boundary::SealedEnd, MAX_PAYLOAD).unwrap_err(),
            FrameError::Compression
        );
    }
    let compressed = zstd::encode_all(&vec![b'x'; 65_536][..], 1).unwrap();
    let bytes = fixture(&compressed, 1, 1024);
    assert_eq!(
        decode(&bytes, context(), Boundary::SealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Limit
    );
}

#[test]
fn auto_keeps_raw_when_zstd_is_not_smaller_and_large_encoder_frames_decode() {
    let mut entropy = vec![0; 4096];
    blake3::Hasher::new().finalize_xof().fill(&mut entropy);
    let bytes = encode(&entropy, context(), Compression::Auto, MAX_PAYLOAD).unwrap();
    assert_eq!(bytes[12], 0);
    assert_eq!(bytes.len(), entropy.len() + FRAME_OVERHEAD);
    let body = vec![b'x'; 9 * 1024 * 1024];
    let bytes = encode(&body, context(), Compression::Auto, MAX_PAYLOAD).unwrap();
    let Decoded::Complete { body: decoded, .. } =
        decode(&bytes, context(), Boundary::SealedEnd, MAX_PAYLOAD).unwrap()
    else {
        panic!("complete")
    };
    assert_eq!(&*decoded, body);
}

#[test]
fn formats_are_visibly_isolated_and_foreign_store_is_rejected() {
    let bytes = fixture(&[0], 0, 1);
    assert!(crate::WalReader::from_bytes(&bytes).is_err());
    assert_eq!(
        decode(b"SLDB", context(), Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Invalid("magic")
    );
    let mut foreign = context();
    let mut id = *foreign.store.as_bytes();
    id[0] = 9;
    foreign.store = StoreId::from_bytes(id).unwrap();
    assert_eq!(
        decode(&bytes, foreign, Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Store
    );
}

#[test]
fn integrity_precedes_version_and_complete_reserved_flags_are_not_torn_tails() {
    let mut bytes = fixture(&[0], 0, 1);
    bytes[8] = 3;
    assert_eq!(
        decode(&bytes, context(), Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Integrity("header")
    );
    let hash = blake3::hash(&bytes[..128]);
    bytes[128..160].copy_from_slice(hash.as_bytes());
    assert_eq!(
        decode(&bytes, context(), Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Unsupported("version")
    );
    bytes[8] = 2;
    bytes[13] = 1;
    let hash = blake3::hash(&bytes[..128]);
    bytes[128..160].copy_from_slice(hash.as_bytes());
    assert_eq!(
        decode(&bytes, context(), Boundary::UnsealedEnd, MAX_PAYLOAD).unwrap_err(),
        FrameError::Invalid("reserved")
    );
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(64))]
    #[test]
    fn exact_sequence_model_rejects_every_gap_and_overlap(expected in 1u64..1000, observed in 0u64..1000) {
        let mut bytes = fixture(b"bounded", 0, 7);
        bytes[32..40].copy_from_slice(&observed.to_le_bytes());
        let header = *blake3::hash(&bytes[..128]).as_bytes();
        bytes[128..160].copy_from_slice(&header);
        let end = bytes.len() - 32;
        let digest = *blake3::hash(&bytes[..end]).as_bytes();
        bytes[end..].copy_from_slice(&digest);
        let trusted = Context { sequence: expected, ..context() };
        let result = decode(&bytes, trusted, Boundary::SealedEnd, 1024);
        if expected == observed {
            proptest::prop_assert!(matches!(result, Ok(Decoded::Complete { .. })), "complete exact sequence");
        } else {
            proptest::prop_assert_eq!(result.unwrap_err(), FrameError::Sequence { expected, observed });
        }
    }
}
