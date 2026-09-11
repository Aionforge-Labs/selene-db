#![no_main]

use libfuzzer_sys::fuzz_target;
use selene_core::logical::Limits;
use selene_graph::logical_transaction::LogicalTransaction;
use selene_persist::{
    control::{StoreEpoch, StoreId},
    logical_frame::{self, Boundary, Context, Decoded},
};

mod logical_named_seed;

const LIMIT: usize = 1 << 20;
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
fn drive(bytes: &[u8]) {
    let limits = Limits {
        bytes: LIMIT,
        allocation: 8 * LIMIT,
        items: 32_768,
        metadata: 1024,
        ..Limits::default()
    };
    for boundary in [
        Boundary::UnsealedEnd,
        Boundary::SealedEnd,
        Boundary::Interior,
    ] {
        if let Ok(Decoded::Complete { body, consumed, .. }) =
            logical_frame::decode(bytes, context(), boundary, LIMIT)
        {
            assert!(consumed >= logical_frame::FRAME_OVERHEAD && consumed <= bytes.len());
            if let Ok(tx) = LogicalTransaction::decode(&body, limits) {
                let encoded = tx
                    .encode(limits)
                    .expect("decoder-admitted transaction re-encodes within limits");
                assert!(LogicalTransaction::decode(&encoded, limits).is_ok());
            }
        }
    }
}

fuzz_target!(|input: &[u8]| {
    if input.len() > LIMIT {
        return;
    }
    drive(input);
    // Raw semantic bytes bypass framing integrity so the body decoder remains reachable.
    let limits = Limits {
        bytes: LIMIT,
        allocation: 8 * LIMIT,
        items: 32_768,
        metadata: 1024,
        ..Limits::default()
    };
    let _ = LogicalTransaction::decode(input, limits);
    let frame =
        logical_frame::encode(input, context(), logical_frame::Compression::Raw, LIMIT).unwrap();
    drive(&frame);
    // Reach typed lineage/version classification with valid common integrity.
    // The expected cause comes from the independently chosen fixed header field.
    if let Some(choice) = input.first() {
        use selene_persist::logical_frame::FrameError as E;
        let offsets = [8, 40, 56, 64, 96];
        let index = usize::from(*choice) % offsets.len();
        let mut changed = frame.clone();
        changed[offsets[index]] ^= 1;
        selene_persist::logical_stream::repair_fixture_integrity(&mut changed, 160);
        let error =
            logical_frame::decode(&changed, context(), Boundary::UnsealedEnd, LIMIT).unwrap_err();
        assert!(matches!(
            (index, error),
            (0, E::Unsupported(_))
                | (1, E::Store)
                | (2, E::Epoch)
                | (3, E::Segment)
                | (4, E::Origin)
        ));
    }
    // Mutate a complete named-type revision over retained graphs, repairing the
    // frame integrity so these owning semantic checks cannot hide behind BLAKE3.
    let (state, template) = logical_named_seed::fixture();
    let mut body = template.clone();
    for edit in input.chunks_exact(3).take(16) {
        let offset = usize::from(u16::from_le_bytes([edit[0], edit[1]])) % body.len();
        body[offset] ^= edit[2];
    }
    let expected = Context {
        sequence: 2,
        ..context()
    };
    let encoded =
        logical_frame::encode(&body, expected, logical_frame::Compression::Raw, LIMIT).unwrap();
    let _ = state.apply_frame(&encoded, expected, Boundary::SealedEnd, limits);
    assert_eq!(
        state.graph_summary(selene_core::GraphId::new(1)),
        Some((1, 0, 2, 1))
    );
    if input.len() >= logical_frame::FRAME_OVERHEAD {
        let mut repaired = input.to_vec();
        repaired[..12].copy_from_slice(b"SLTXN2\0\0\x02\0\0\0");
        repaired[12] &= 1;
        repaired[13..16].fill(0);
        let body_len = repaired.len() - logical_frame::FRAME_OVERHEAD;
        repaired[16..24].copy_from_slice(&(body_len as u64).to_le_bytes());
        // Preserve mutated expanded length for expansion/history/length rejection.
        repaired[32..40].copy_from_slice(&1u64.to_le_bytes());
        repaired[40..56].copy_from_slice(context().store.as_bytes());
        repaired[56..64].copy_from_slice(&1u64.to_le_bytes());
        repaired[64..128].fill(0);
        let header = blake3::hash(&repaired[..128]);
        repaired[128..160].copy_from_slice(header.as_bytes());
        let end = repaired.len() - 32;
        repaired[end - 8..end].copy_from_slice(b"SLTXEND2");
        let digest = blake3::hash(&repaired[..end]);
        repaired[end..].copy_from_slice(digest.as_bytes());
        drive(&repaired);
    }
});
