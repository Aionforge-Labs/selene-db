#![no_main]
use libfuzzer_sys::fuzz_target;
use selene_core::logical::Limits;
use selene_graph::logical_transaction::{ReplayState, encode_checkpoint};
use selene_persist::{
    control::{StoreEpoch, StoreId},
    logical_snapshot::{self, SnapshotContext},
    logical_stream::Position,
};
use std::sync::OnceLock;
mod logical_named_seed;

fn context() -> SnapshotContext {
    let mut id = [0; 16];
    id[6] = 0x40;
    id[8] = 0x80;
    SnapshotContext {
        boundary: Position {
            store: StoreId::from_bytes(id).unwrap(),
            epoch: StoreEpoch::new(1).unwrap(),
            segment: [1; 32],
            sequence: 2,
            offset: 512,
            digest: [2; 32],
        },
        publication: 2,
    }
}
fn seed() -> &'static Vec<u8> {
    static SEED: OnceLock<Vec<u8>> = OnceLock::new();
    SEED.get_or_init(|| {
        let state = &logical_named_seed::fixture().0;
        let runtime = state.materialize(Limits::default()).unwrap();
        let graphs: Vec<_> = runtime.graphs.values().map(|g| g.read()).collect();
        encode_checkpoint(
            state.catalog(),
            &runtime.graph_types,
            &graphs.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            Limits::default(),
        )
        .unwrap()
    })
}
fuzz_target!(|input: &[u8]| {
    if input.len() > 16384 {
        return;
    }
    let limits = Limits {
        bytes: 1 << 20,
        allocation: 16 << 20,
        items: 32768,
        metadata: 1024,
        ..Limits::default()
    };
    let _ = logical_snapshot::decode(input, context(), &[0; 32], limits.bytes);
    let _ = ReplayState::from_checkpoint(input, limits);
    let mut body = seed().clone();
    for edit in input.chunks_exact(3).take(32) {
        let i = usize::from(u16::from_le_bytes([edit[0], edit[1]])) % body.len();
        body[i] ^= edit[2];
    }
    // Production framing deliberately repairs integrity to reach every semantic section.
    let framed = logical_snapshot::encode(&body, context(), limits.bytes).unwrap();
    let digest = framed[framed.len() - 32..].try_into().unwrap();
    let body = logical_snapshot::decode(&framed, context(), &digest, limits.bytes).unwrap();
    if let Ok(state) = ReplayState::from_checkpoint(body, limits) {
        // Native callable admission is outside this graph-only pure decoder harness.
        let _ = state.materialize(limits);
    }
});
