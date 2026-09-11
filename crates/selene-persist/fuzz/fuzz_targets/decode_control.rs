#![no_main]

use libfuzzer_sys::fuzz_target;
use selene_persist::control::{CurrentSelector, EmptyManifest};

// Pure bounded decoders: malformed framing, checksum, identity, lineage, and
// selector names must fail before file access or input-sized unbounded allocation.
fuzz_target!(|bytes: &[u8]| {
    let _ = CurrentSelector::decode(bytes);
    let _ = EmptyManifest::decode(bytes);
    let _ = selene_persist::logical_stream::validate_manifest(bytes);
    for magic in [*b"SLDM", *b"SLLM", *b"SLEM", *b"SLCU"] {
        if let Ok(repaired) = selene_persist::control::frame_fuzz_payload(bytes, magic) {
            let _ = CurrentSelector::decode(&repaired);
            let _ = EmptyManifest::decode(&repaired);
            let _ = selene_persist::logical_stream::validate_manifest(&repaired);
        }
    }
});
