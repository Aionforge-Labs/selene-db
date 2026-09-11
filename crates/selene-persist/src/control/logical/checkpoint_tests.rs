use super::*;
use crate::logical_stream::{LogicalWal, ReopeningWal};

#[test]
fn independently_reencoded_wrong_boundary_origin_identity_and_version_are_rejected() {
    for damage in [
        "offset", "sequence", "origin", "epoch", "profile", "version",
    ] {
        let (_temp, dir) = crate::control::tests::directory();
        let identity = crate::control::tests::identity();
        let mut wal =
            LogicalWal::create(EmptyStoreControl::create_empty(&dir, identity.clone()).unwrap())
                .unwrap();
        let info = wal.checkpoint(b"image", 0).unwrap();
        let name = format!("MANIFEST-{:020}.control", info.generation);
        let mut manifest: DataManifest =
            codec::decode(&read_bounded(&dir, Path::new(&name)).unwrap(), *b"SLDM").unwrap();
        match damage {
            "offset" => {
                manifest.snapshot.boundary.sequence = 1;
                manifest.snapshot.boundary.offset = 209;
            }
            "sequence" => manifest.snapshot.boundary.sequence = 1,
            "origin" => manifest.origin = [4; 32],
            "epoch" => manifest.snapshot.boundary.epoch = StoreEpoch::new(9).unwrap(),
            "profile" => {
                manifest.metadata.identity =
                    CompatibilityIdentity::new("foreign", 1, [2; 32], [17, 0, 0], "binary", 1)
                        .unwrap()
            }
            _ => manifest.metadata.format = [2, 1],
        }
        let bytes = crate::control::tests::envelope(&manifest, *b"SLDM");
        let selector =
            CurrentSelector::from_manifest(&manifest.metadata, *blake3::hash(&bytes).as_bytes());
        crate::control::tests::overwrite(&dir, &name, &bytes);
        crate::control::tests::overwrite(&dir, CURRENT_FILE_NAME, &selector.encode().unwrap());
        drop(wal);
        match ReopeningWal::open(&dir, &identity, 1024) {
            Err(_) => {}
            Ok(reopen) => assert!(reopen.snapshot_body().is_err(), "{damage}"),
        }
        assert_eq!(read_bounded(&dir, Path::new(&name)).unwrap(), bytes);
    }
}
