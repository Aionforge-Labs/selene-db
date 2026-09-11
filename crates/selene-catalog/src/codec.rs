//! Explicit format-2 catalog delta codec. Never serializes catalog Rust enums.
//!
//! Apply returns isolated metadata only. A Ready descriptor does not activate code.

use crate::{
    CatalogDescriptor, CatalogGeneration, CatalogLogicalChange as Change, CatalogLogicalRecords,
    CatalogObjectId as Id, CatalogObjectKind as Kind, CatalogParent, CatalogPayload,
    CatalogSnapshot,
};
use selene_core::logical::{CodecError as E, CodecResult, Decoder, Encoder};
use std::collections::{BTreeMap, BTreeSet};

mod checkpoint;
mod declaration;
mod descriptor;
mod native;

pub use checkpoint::{decode_records, encode_records};

#[cfg(test)]
mod checkpoint_tests;

/// Wire domain order; IDs in distinct domains are never interchangeable.
pub const DOMAINS: [Kind; 9] = [
    Kind::Catalog,
    Kind::Directory,
    Kind::Schema,
    Kind::Graph,
    Kind::GraphType,
    Kind::BindingTable,
    Kind::Procedure,
    Kind::Index,
    Kind::Constraint,
];

/// Charge the complete retained catalog before cloning/revalidating isolated state.
/// Uses the real field codec in counting mode, including native payloads and strings.
pub fn account_records(
    records: &CatalogLogicalRecords,
    budget: &mut selene_core::logical::Budget,
) -> CodecResult<()> {
    let mut e = Encoder::counting(budget.clone());
    e.count_for::<CatalogDescriptor>(records.descriptors().len())?;
    e.budget.metadata(records.descriptors().len())?;
    for descriptor in records.descriptors() {
        self::descriptor::encode(&mut e, descriptor)?;
    }
    *budget = e.budget;
    Ok(())
}

/// One catalog revision transition in a complete logical transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogDelta {
    /// Required previous catalog snapshot generation.
    pub previous: CatalogGeneration,
    /// Resulting catalog snapshot generation (may be unchanged for graph-only writes).
    pub generation: CatalogGeneration,
    /// Last allocated identity in each domain, including deleted published identities.
    pub high_water: [u64; 9],
    /// Dependency-ordered descriptor creations, replacements, and removals.
    pub changes: Vec<Change>,
}

impl CatalogDelta {
    /// Adapt real catalog logical inputs to deterministic dependency order.
    pub fn between(previous: &CatalogSnapshot, next: &CatalogLogicalRecords) -> CodecResult<Self> {
        let snapshot = next.reconstruct().map_err(|_| E::Semantic)?;
        let high_water = DOMAINS.map(|kind| next.high_water().get(&kind).copied().unwrap_or(0));
        let mut pending = snapshot.logical_changes_from(previous);
        let mut state: BTreeMap<_, _> = previous
            .descriptors()
            .map(|d| (d.id(), d.clone()))
            .collect();
        let mut changes = Vec::new();
        while !pending.is_empty() {
            let Some(index) = pending
                .iter()
                .position(|change| order_ready(change, &state))
            else {
                return Err(E::Invalid("catalog dependency order"));
            };
            let change = pending.remove(index);
            change_map(&change, &mut state);
            changes.push(change);
        }
        Ok(Self {
            previous: previous.generation(),
            generation: snapshot.generation(),
            high_water,
            changes,
        })
    }

    /// Encode this delta into the caller's transaction-wide budget.
    pub fn encode(&self, e: &mut Encoder) -> CodecResult<()> {
        e.u64(self.previous.get())?;
        e.u64(self.generation.get())?;
        for water in self.high_water {
            e.u64(water)?;
        }
        e.count_for::<Change>(self.changes.len())?;
        let mut seen = BTreeSet::new();
        for change in &self.changes {
            if !seen.insert(change_id(change)) {
                return Err(E::Invalid("duplicate catalog change"));
            }
            e.budget.metadata(1)?;
            match change {
                Change::Created(d) => {
                    e.u8(1)?;
                    descriptor::encode(e, d)?;
                }
                Change::Replaced {
                    previous,
                    descriptor: d,
                } => {
                    e.u8(2)?;
                    e.u64(previous.get())?;
                    descriptor::encode(e, d)?;
                }
                Change::Dropped { id, generation } => {
                    e.u8(3)?;
                    descriptor::id_encode(e, *id)?;
                    e.u64(generation.get())?;
                }
            }
        }
        Ok(())
    }

    /// Decode a complete catalog delta without publishing or activating it.
    pub fn decode(d: &mut Decoder<'_, '_>) -> CodecResult<Self> {
        let previous = generation(d)?;
        let generation = generation(d)?;
        let mut high_water = [0; 9];
        for water in &mut high_water {
            *water = d.u64()?;
        }
        let count = d.count_for::<Change>()?;
        let mut changes = Vec::with_capacity(count);
        let mut seen = BTreeSet::new();
        for _ in 0..count {
            d.budget.metadata(1)?;
            let change = match d.u8()? {
                1 => Change::Created(descriptor::decode(d)?),
                2 => Change::Replaced {
                    previous: self::generation(d)?,
                    descriptor: descriptor::decode(d)?,
                },
                3 => Change::Dropped {
                    id: descriptor::id_decode(d)?,
                    generation: self::generation(d)?,
                },
                _ => return Err(E::Invalid("catalog operation tag")),
            };
            if !seen.insert(change_id(&change)) {
                return Err(E::Invalid("duplicate catalog change"));
            }
            changes.push(change);
        }
        Ok(Self {
            previous,
            generation,
            high_water,
            changes,
        })
    }

    /// Validate order, revisions, deleted-ID water, and the entire dependency graph.
    /// The original metadata is borrowed and remains unchanged even on a last-field error.
    pub fn apply(&self, original: &CatalogLogicalRecords) -> CodecResult<CatalogLogicalRecords> {
        let snapshot = original.reconstruct().map_err(|_| E::Semantic)?;
        if snapshot.generation() != self.previous
            || self.generation < self.previous
            || (!self.changes.is_empty() && self.generation == self.previous)
        {
            return Err(E::Invalid("catalog generation"));
        }
        let mut state: BTreeMap<_, _> = snapshot
            .descriptors()
            .map(|d| (d.id(), d.clone()))
            .collect();
        let mut seen = BTreeSet::new();
        for (kind, water) in DOMAINS.into_iter().zip(self.high_water) {
            if water < original.high_water().get(&kind).copied().unwrap_or(0) {
                return Err(E::Invalid("catalog high water"));
            }
        }
        for change in &self.changes {
            let id = change_id(change);
            if !seen.insert(id) || !order_ready(change, &state) {
                return Err(E::Invalid("catalog dependency order"));
            }
            match change {
                Change::Created(d) => {
                    if state.contains_key(&id)
                        || id.get() <= original.high_water().get(&id.kind()).copied().unwrap_or(0)
                        || d.generation() > self.generation
                        || d.creation().generation() <= self.previous
                    {
                        return Err(E::Invalid("catalog creation identity"));
                    }
                }
                Change::Replaced {
                    previous,
                    descriptor: d,
                } => {
                    let old = state
                        .get(&id)
                        .ok_or(E::Invalid("missing replaced identity"))?;
                    if old.generation() != *previous
                        || d.generation() <= *previous
                        || d.generation() > self.generation
                        || old.creation() != d.creation()
                        || old.parent() != d.parent()
                    {
                        return Err(E::Invalid("catalog replacement revision"));
                    }
                }
                Change::Dropped { generation, .. } => {
                    if state.get(&id).is_none_or(|d| d.generation() != *generation) {
                        return Err(E::Invalid("catalog drop revision"));
                    }
                }
            }
            change_map(change, &mut state);
        }
        CatalogLogicalRecords::new(
            self.generation,
            DOMAINS.into_iter().zip(self.high_water).collect(),
            state.into_values().collect(),
        )
        .map_err(|_| E::Semantic)
    }
}

fn generation(d: &mut Decoder<'_, '_>) -> CodecResult<CatalogGeneration> {
    CatalogGeneration::new(d.u64()?).map_err(|_| E::Semantic)
}
fn change_id(change: &Change) -> Id {
    match change {
        Change::Created(d) | Change::Replaced { descriptor: d, .. } => d.id(),
        Change::Dropped { id, .. } => *id,
    }
}
fn change_map(change: &Change, state: &mut BTreeMap<Id, CatalogDescriptor>) {
    match change {
        Change::Created(d) | Change::Replaced { descriptor: d, .. } => {
            state.insert(d.id(), d.clone());
        }
        Change::Dropped { id, .. } => {
            state.remove(id);
        }
    }
}
fn requirements(d: &CatalogDescriptor) -> Vec<(Id, Option<CatalogGeneration>)> {
    let mut ids = Vec::new();
    if let Some(id) = match d.parent() {
        CatalogParent::None => None,
        CatalogParent::Catalog(id) => Some(Id::Catalog(id)),
        CatalogParent::Directory(id) => Some(Id::Directory(id)),
        CatalogParent::Schema(id) => Some(Id::Schema(id)),
        CatalogParent::Graph(id) => Some(Id::Graph(id)),
        CatalogParent::GraphType(id) => Some(Id::GraphType(id)),
    } {
        ids.push((id, None));
    }
    if let CatalogPayload::Graph {
        graph_type: Some(id),
    } = d.payload()
    {
        ids.push((Id::GraphType(*id), None));
    }
    if let Some(metadata) = d.payload().declaration_metadata() {
        ids.extend(
            metadata
                .dependencies
                .iter()
                .map(|dep| (dep.id, Some(dep.generation))),
        );
    }
    ids
}
fn order_ready(change: &Change, state: &BTreeMap<Id, CatalogDescriptor>) -> bool {
    match change {
        Change::Created(d) | Change::Replaced { descriptor: d, .. } => {
            requirements(d).into_iter().all(|(id, generation)| {
                state
                    .get(&id)
                    .is_some_and(|target| generation.is_none_or(|g| g == target.generation()))
            })
        }
        Change::Dropped { id, .. } => state
            .values()
            .all(|d| !requirements(d).iter().any(|(required, _)| required == id)),
    }
}

#[cfg(test)]
mod tests;
