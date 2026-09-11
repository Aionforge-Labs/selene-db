use super::*;
use crate::{CatalogId, CatalogName, CreationMetadata, DirectoryId};
use selene_core::logical::{Budget, Limits};

#[test]
fn full_catalog_image_is_self_contained_and_retains_deleted_domain_water() {
    let generation = CatalogGeneration::new(7).unwrap();
    let creation = CreationMetadata::new(CatalogGeneration::new(1).unwrap(), None);
    let catalog = CatalogDescriptor::catalog(
        CatalogId::new(1).unwrap(),
        CatalogName::regular("selene").unwrap(),
        generation,
        creation.clone(),
    )
    .unwrap();
    let root = CatalogDescriptor::root_directory(
        DirectoryId::new(1).unwrap(),
        CatalogId::new(1).unwrap(),
        generation,
        creation,
    )
    .unwrap();
    let water = [1, 1, 31, 19, 23, 0, 97, 43, 47];
    let records = CatalogLogicalRecords::new(
        generation,
        DOMAINS.into_iter().zip(water).collect(),
        vec![catalog, root],
    )
    .unwrap();
    let mut e = Encoder::new(Limits::default()).unwrap();
    encode_records(&records, &mut e).unwrap();
    let bytes = e.finish();
    // Independently inspect the fixed fields: no delta base or generated seed.
    assert_eq!(&bytes[..8], &7_u64.to_le_bytes());
    for (i, expected) in water.iter().enumerate() {
        assert_eq!(&bytes[8 + i * 8..16 + i * 8], &expected.to_le_bytes());
    }
    assert_eq!(&bytes[80..84], &2_u32.to_le_bytes());
    let mut budget = Budget::new(Limits::default()).unwrap();
    let mut d = Decoder::new(&bytes, &mut budget).unwrap();
    let decoded = decode_records(&mut d).unwrap();
    d.finish().unwrap();
    assert_eq!(decoded, records);
    assert_eq!(decoded.reconstruct().unwrap().descriptors().count(), 2);
}
