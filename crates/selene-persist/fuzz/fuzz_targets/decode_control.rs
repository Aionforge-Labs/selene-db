#![no_main]

use libfuzzer_sys::fuzz_target;
use selene_persist::control::{CurrentSelector, EmptyManifest};

// Pure bounded decoders: malformed framing, checksum, identity, lineage, and
// selector names must fail before file access or input-sized unbounded allocation.
fuzz_target!(|bytes: &[u8]| {
    let _ = CurrentSelector::decode(bytes);
    let _ = EmptyManifest::decode(bytes);
    let _ = selene_persist::logical_stream::validate_manifest(bytes);
    for magic in [*b"SLRM", *b"SLDM", *b"SLLM", *b"SLEM", *b"SLCU"] {
        if let Ok(repaired) = selene_persist::control::frame_fuzz_payload(bytes, magic) {
            let _ = CurrentSelector::decode(&repaired);
            let _ = EmptyManifest::decode(&repaired);
            let _ = selene_persist::logical_stream::validate_manifest(&repaired);
        }
    }
    // Mutate an otherwise valid selected-layout payload, then repair the outer
    // integrity so the new bases, sealed bounds and parent/name checks are reached.
    let mut body = selene_persist::logical_stream::rotating_manifest_fuzz_payload();
    for edit in bytes.chunks_exact(3).take(32) {
        let index = usize::from(u16::from_le_bytes([edit[0], edit[1]])) % body.len();
        body[index] ^= edit[2];
    }
    let repaired = selene_persist::control::frame_fuzz_payload(&body, *b"SLRM").unwrap();
    let _ = selene_persist::logical_stream::validate_manifest(&repaired);
});
