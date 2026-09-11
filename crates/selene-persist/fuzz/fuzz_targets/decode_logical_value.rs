#![no_main]

use libfuzzer_sys::fuzz_target;
use selene_core::logical::{Limits, decode_value, encode_value};

fuzz_target!(|input: &[u8]| {
    let limits = Limits {
        bytes: 1 << 20,
        allocation: 8 << 20,
        items: 32_768,
        ..Limits::default()
    };
    if let Ok(value) = decode_value(input, limits) {
        let bytes = encode_value(&value, limits).expect("decoder-admitted value is encodable");
        assert_eq!(decode_value(&bytes, limits).unwrap(), value);
    }
    // Exercise 256-depth rejection without waiting for random container prefixes.
    let depth = input.first().copied().unwrap_or(0) as usize;
    let mut nested = Vec::with_capacity(depth * 5 + input.len());
    for _ in 0..depth {
        nested.extend_from_slice(&[11, 1, 0, 0, 0]);
    }
    nested.extend_from_slice(input.get(1..).unwrap_or(&[]));
    let _ = decode_value(&nested, limits);
});
