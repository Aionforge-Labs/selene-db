//! Full self-contained catalog image; never a delta against an implicit runtime seed.

use super::*;

/// Encode every catalog descriptor and all allocation domains into a shared budget.
pub fn encode_records(records: &CatalogLogicalRecords, e: &mut Encoder) -> CodecResult<()> {
    let snapshot = records.reconstruct().map_err(|_| E::Semantic)?;
    e.u64(snapshot.generation().get())?;
    for kind in DOMAINS {
        e.u64(records.high_water().get(&kind).copied().unwrap_or(0))?;
    }
    e.count_for::<CatalogDescriptor>(records.descriptors().len())?;
    e.budget.metadata(records.descriptors().len())?;
    for record in records.descriptors() {
        descriptor::encode(e, record)?;
    }
    Ok(())
}

/// Decode and validate a complete catalog without supplying a generated seed.
/// Counts, descriptor allocations and native metadata share the caller's budget.
pub fn decode_records(d: &mut Decoder<'_, '_>) -> CodecResult<CatalogLogicalRecords> {
    let generation = generation(d)?;
    let mut high_water = BTreeMap::new();
    for kind in DOMAINS {
        high_water.insert(kind, d.u64()?);
    }
    let count = d.count_for::<CatalogDescriptor>()?;
    d.budget.metadata(count)?;
    let mut descriptors: Vec<CatalogDescriptor> = Vec::with_capacity(count);
    for _ in 0..count {
        let record = descriptor::decode(d)?;
        if descriptors
            .last()
            .is_some_and(|last| last.id() >= record.id())
        {
            return Err(E::Invalid("catalog image identity order"));
        }
        descriptors.push(record);
    }
    CatalogLogicalRecords::new(generation, high_water, descriptors).map_err(|_| E::Semantic)
}
