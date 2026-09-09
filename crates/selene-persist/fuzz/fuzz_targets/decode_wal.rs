#![no_main]

use libfuzzer_sys::fuzz_target;
use selene_persist::{WAL_VERSION_MAJOR, WAL_VERSION_MINOR, WalReader};
use xxhash_rust::xxh3::xxh3_64;

// Drives the whole streaming WAL decode on attacker bytes: SLDB file-header
// decode, per-entry header decode (fixed prefix + replicated tail + principal),
// payload-length-vs-remaining bound, and `body()` = xxh3 checksum + bounded
// zstd decompress + postcard `Vec<Change>` decode. Only a panic/OOM/hang fails;
// any malformed input yields a typed error and the stream self-stops.
//
// Two passes, because WAL 3.0 put the framing under integrity protection and a
// single pass can no longer reach both properties:
//
//   raw        - arbitrary bytes must not panic. This is the original property,
//                and it is now almost entirely a test of the reject paths: a
//                mutated byte anywhere in a frame fails the prefix checksum.
//   structured - the same bytes wrapped in framing whose checksums are correct.
//                Without this the target is vacuous. A coverage-guided fuzzer
//                cannot invert xxh3, so every input would die at the first
//                frame and the extent, principal, payload, zstd, and postcard
//                paths would never be entered again.
//
// Because the structured pass builds its own envelope, seeds no longer have to
// be valid WAL files and are not pinned to a format version.

const FILE_HEADER_LEN: usize = 24;
const FILE_HEADER_CHECKSUM_OFFSET: usize = 20;
const PREFIX_LEN: usize = 40;
const REPLICATED_TAIL_BYTES: usize = 16;
const FLAG_REPLICATED_ORIGIN: u8 = 0b0000_0100;
const KNOWN_FLAGS: u8 = 0b0000_0111;
/// Bytes of input consumed per frame to choose that frame's shape.
const CONTROL_BYTES: usize = 4;

fn checksum_lo(bytes: &[u8]) -> u32 {
    xxh3_64(bytes) as u32
}

fn synthesize(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + FILE_HEADER_LEN);

    let mut header = [0_u8; FILE_HEADER_LEN];
    header[0..4].copy_from_slice(b"SLDB");
    header[4..6].copy_from_slice(&WAL_VERSION_MAJOR.to_le_bytes());
    header[6..8].copy_from_slice(&WAL_VERSION_MINOR.to_le_bytes());
    // snapshot_seq and the reserved word stay zero, so sequence 1 is the first
    // frame the reader will accept.
    let checksum = checksum_lo(&header[..FILE_HEADER_CHECKSUM_OFFSET]);
    header[FILE_HEADER_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(&header);

    let mut cursor = data;
    let mut sequence = 1_u64;
    while cursor.len() > CONTROL_BYTES {
        let (control, rest) = cursor.split_at(CONTROL_BYTES);
        // Masked to the known bits: an undefined flag is rejected before the
        // extent is read, which the raw pass already covers. The point here is
        // to get PAST the framing.
        let flags = control[0] & KNOWN_FLAGS;
        let want = usize::from(u16::from_le_bytes([control[2], control[3]]));
        let body_len = want.min(rest.len());
        let (body, next) = rest.split_at(body_len);
        let principal_len = usize::from(control[1]).min(body.len());
        let (principal, payload) = body.split_at(principal_len);

        let mut extent = Vec::with_capacity(REPLICATED_TAIL_BYTES + principal_len);
        if flags & FLAG_REPLICATED_ORIGIN != 0 {
            extent.extend_from_slice(&[0_u8; REPLICATED_TAIL_BYTES]);
        }
        extent.extend_from_slice(principal);

        let principal_len_p1 = if principal_len == 0 {
            0
        } else {
            u16::try_from(principal_len + 1).expect("principal length is bounded by a u8")
        };

        let mut prefix = [0_u8; PREFIX_LEN];
        prefix[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        prefix[8..12].copy_from_slice(&checksum_lo(payload).to_le_bytes());
        prefix[12..20].copy_from_slice(&sequence.to_le_bytes());
        prefix[20..28].copy_from_slice(&sequence.to_le_bytes());
        prefix[32] = flags;
        prefix[34..36].copy_from_slice(&principal_len_p1.to_le_bytes());
        prefix[36..40].copy_from_slice(&checksum_lo(&extent).to_le_bytes());
        let prefix_checksum = checksum_lo(&prefix[4..PREFIX_LEN]);
        prefix[0..4].copy_from_slice(&prefix_checksum.to_le_bytes());

        out.extend_from_slice(&prefix);
        out.extend_from_slice(&extent);
        out.extend_from_slice(payload);

        sequence += 1;
        cursor = next;
    }
    out
}

fn drive(bytes: &[u8]) {
    let Ok(stream) = WalReader::from_bytes(bytes) else {
        return;
    };
    for item in stream {
        match item {
            Ok(view) => {
                let _ = view.body();
            }
            Err(_) => break,
        }
    }
}

fuzz_target!(|bytes: &[u8]| {
    drive(bytes);
    drive(&synthesize(bytes));
});
