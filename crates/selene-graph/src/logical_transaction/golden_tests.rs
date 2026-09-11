use super::*;

// Independently assembled v1 body: no production encoder is used to build it.
fn golden() -> Vec<u8> {
    let mut bytes = 1u32.to_le_bytes().to_vec();
    bytes.extend(1u64.to_le_bytes());
    bytes.extend(2u64.to_le_bytes());
    for water in [1u64, 1, 1, 1, 0, 0, 0, 0, 0] {
        bytes.extend(water.to_le_bytes());
    }
    bytes.extend(2u32.to_le_bytes());
    // Two Created descriptors: schema 1 "s" under directory 1, graph 1 "g" under schema 1.
    for (kind, name, parent) in [(3, b's', 2), (4, b'g', 3)] {
        bytes.extend([1, kind]);
        bytes.extend(1u64.to_le_bytes());
        bytes.push(1);
        bytes.extend(1u32.to_le_bytes());
        bytes.push(name);
        bytes.push(parent);
        bytes.extend(1u64.to_le_bytes());
        bytes.extend(2u64.to_le_bytes());
        bytes.extend(2u64.to_le_bytes());
        bytes.extend([0, kind]); // no creation principal; matching payload tag
        if kind == 4 {
            bytes.push(0);
        } // no named constraining graph type
    }
    assert_eq!(bytes.len(), 183);
    bytes.extend(0u32.to_le_bytes()); // named type count
    bytes.extend(1u32.to_le_bytes()); // graph count
    bytes.extend(1u64.to_le_bytes());
    bytes.push(0); // graph 1 has no previous generation
    for number in [1u64, 2, 1] {
        bytes.extend(number.to_le_bytes());
    } // generation, next node/edge
    bytes.push(0);
    bytes.extend(0u32.to_le_bytes()); // open; no index backing identities
    bytes.extend(1u32.to_le_bytes()); // one graph operation
    assert_eq!(bytes.len(), 233);
    bytes.push(1);
    bytes.extend(1u64.to_le_bytes()); // NodeCreated 1
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(1u32.to_le_bytes()); // labels, properties
    bytes.extend(1u32.to_le_bytes());
    bytes.push(b'v');
    bytes.push(5); // property v, U128 tag
    bytes.extend([255; 16]);
    assert_eq!(bytes.len(), 272);
    bytes
}

#[test]
fn independently_assembled_complete_catalog_graph_transaction() {
    let bytes = golden();
    let seed = seed();
    let decoded = LogicalTransaction::decode(&bytes, Limits::default()).unwrap();
    assert_eq!(decoded.encode(Limits::default()).unwrap(), bytes);
    let state = seed.apply_body(&bytes, Limits::default()).unwrap();
    assert_eq!(state.graph_summary(GraphId::new(1)), Some((1, 0, 2, 1)));
    assert_eq!(
        state.node_property(GraphId::new(1), NodeId::new(1), &db_string("v").unwrap()),
        Some(&Value::Uint128(u128::MAX))
    );
    for cut in 0..bytes.len() {
        assert!(seed.apply_body(&bytes[..cut], Limits::default()).is_err());
    }
    let mut wrong = bytes.clone();
    wrong[233] = 255;
    assert_eq!(
        seed.apply_body(&wrong, Limits::default()).err(),
        Some(E::Unsupported("graph operation tag"))
    );
    let mut wrong = bytes;
    wrong[159] = 2; // corrupt a descriptor revision/parent field, not an enum layout
    assert!(seed.apply_body(&wrong, Limits::default()).is_err());
    assert!(seed.graphs.is_empty());
}

#[test]
fn framed_prefix_and_incomplete_suffix_do_not_publish_corrupt_or_partial_state() {
    use selene_persist::{
        control::{StoreEpoch, StoreId},
        logical_frame::{self as frame, Boundary, Compression, Context},
    };
    let mut id = [0; 16];
    id[6] = 0x40;
    id[8] = 0x80;
    let context = Context {
        store: StoreId::from_bytes(id).unwrap(),
        epoch: StoreEpoch::new(1).unwrap(),
        sequence: 1,
        segment: [0; 32],
        previous: [0; 32],
    };
    let seed = seed();
    let bytes = frame::encode(&golden(), context, Compression::Raw, frame::MAX_PAYLOAD).unwrap();
    let FrameCandidate::Complete { state, digest } = seed
        .apply_frame(&bytes, context, Boundary::UnsealedEnd, Limits::default())
        .unwrap()
    else {
        panic!("complete")
    };
    let next_context = Context {
        sequence: 2,
        previous: digest,
        ..context
    };
    let old = state.catalog.reconstruct().unwrap();
    let tx = LogicalTransaction {
        catalog: CatalogDelta::between(&old, &state.catalog).unwrap(),
        graph_types: vec![],
        graphs: vec![],
    };
    let suffix = frame::encode(
        &tx.encode(Limits::default()).unwrap(),
        next_context,
        Compression::Raw,
        frame::MAX_PAYLOAD,
    )
    .unwrap();
    for cut in [0, 12, 159, 160, suffix.len() - 1] {
        assert!(matches!(
            state
                .apply_frame(
                    &suffix[..cut],
                    next_context,
                    Boundary::UnsealedEnd,
                    Limits::default()
                )
                .unwrap(),
            FrameCandidate::Incomplete { .. }
        ));
        assert!(
            state
                .apply_frame(
                    &suffix[..cut],
                    next_context,
                    Boundary::SealedEnd,
                    Limits::default()
                )
                .is_err()
        );
    }
    let mut corrupt = suffix;
    let end = corrupt.len() - 1;
    corrupt[end] ^= 1;
    assert!(
        state
            .apply_frame(
                &corrupt,
                next_context,
                Boundary::UnsealedEnd,
                Limits::default()
            )
            .is_err()
    );
    assert_eq!(state.graph_summary(GraphId::new(1)), Some((1, 0, 2, 1)));
    assert!(seed.graphs.is_empty());
}
