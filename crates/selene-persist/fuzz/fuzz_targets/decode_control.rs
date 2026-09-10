#![no_main]

use libfuzzer_sys::fuzz_target;
use selene_persist::control::{CurrentSelector, EmptyManifest};

// Pure bounded decoders: malformed framing, checksum, identity, lineage, and
// selector names must fail before file access or input-sized unbounded allocation.
fuzz_target!(|bytes: &[u8]| {
    let _ = CurrentSelector::decode(bytes);
    let _ = EmptyManifest::decode(bytes);
});
