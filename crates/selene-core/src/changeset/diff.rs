//! Canonical label and property diffs carried by WAL changes.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smallvec::SmallVec;

use crate::{CoreError, CoreResult, DbString, Value};

/// Label set difference.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelDiff {
    /// Labels added by the mutation.
    pub added: SmallVec<[DbString; 2]>,
    /// Labels removed by the mutation.
    pub removed: SmallVec<[DbString; 2]>,
}

impl LabelDiff {
    /// Construct a sorted, deduplicated label diff.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::OverlappingDiff`] when a label appears in both
    /// `added` and `removed`. Contradictory diffs would make WAL replay
    /// order-dependent, so the constructor refuses to build them.
    pub fn new(
        added: impl IntoIterator<Item = DbString>,
        removed: impl IntoIterator<Item = DbString>,
    ) -> CoreResult<Self> {
        let added = sorted_deduped(added);
        let removed = sorted_deduped(removed);
        ensure_disjoint("label", &added, &removed)?;
        Ok(Self { added, removed })
    }

    /// Return true if no labels changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[derive(Deserialize, Serialize)]
struct LabelDiffWire {
    added: SmallVec<[DbString; 2]>,
    removed: SmallVec<[DbString; 2]>,
}

#[derive(Serialize)]
struct LabelDiffWireRef<'a> {
    added: &'a [DbString],
    removed: &'a [DbString],
}

impl Serialize for LabelDiff {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Canonicalize on serialize. `LabelDiff::new` already sorts, so this is
        // a no-op (byte-identical) for constructed diffs. The fields are public,
        // so direct construction still emits canonical wire.
        if db_strings_are_canonical(&self.added) && db_strings_are_canonical(&self.removed) {
            return LabelDiffWireRef {
                added: self.added.as_slice(),
                removed: self.removed.as_slice(),
            }
            .serialize(serializer);
        }

        let mut added = self.added.clone();
        let mut removed = self.removed.clone();
        added.sort_by(|lhs, rhs| lhs.as_str().cmp(rhs.as_str()));
        removed.sort_by(|lhs, rhs| lhs.as_str().cmp(rhs.as_str()));
        LabelDiffWire { added, removed }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LabelDiff {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Validate the canonical (strictly-ascending, dedup'd, disjoint)
        // invariant rather than re-sorting; a non-canonical payload is
        // rejected as malformed.
        let wire = LabelDiffWire::deserialize(deserializer)?;
        validate_sorted_unique(&wire.added, "LabelDiff.added")?;
        validate_sorted_unique(&wire.removed, "LabelDiff.removed")?;
        validate_disjoint(&wire.added, &wire.removed, "label")?;
        Ok(Self {
            added: wire.added,
            removed: wire.removed,
        })
    }
}

/// Property map difference.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyDiff {
    /// Keys set to a new value. Use [`Value::Null`] for an explicit null set.
    pub set: SmallVec<[(DbString, Value); 4]>,
    /// Keys whose entries are removed entirely.
    pub removed: SmallVec<[DbString; 2]>,
}

impl PropertyDiff {
    /// Construct a sorted, deduplicated property diff.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::OverlappingDiff`] when a key appears in both `set`
    /// and `removed`. Contradictory diffs would make WAL replay
    /// order-dependent, so the constructor refuses to build them.
    pub fn new(
        set: impl IntoIterator<Item = (DbString, Value)>,
        removed: impl IntoIterator<Item = DbString>,
    ) -> CoreResult<Self> {
        let mut set: SmallVec<[(DbString, Value); 4]> = set.into_iter().collect();
        for (_, value) in &set {
            crate::StoredValue::validate(value)?;
        }
        if set.len() > 1 && !set.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            set.sort_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));
            let mut deduped = SmallVec::new();
            for (key, value) in set {
                if let Some((last_key, last_value)) = deduped.last_mut()
                    && last_key == &key
                {
                    *last_value = value;
                    continue;
                }
                deduped.push((key, value));
            }
            set = deduped;
        }
        let removed = sorted_deduped(removed);
        if let Some(key) = first_property_overlap(&set, &removed) {
            return Err(CoreError::OverlappingDiff {
                kind: "property",
                key: key.clone(),
            });
        }
        Ok(Self { set, removed })
    }

    /// Return true if no properties changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.removed.is_empty()
    }

    /// Validate directly constructed legacy diff payloads before mutation.
    pub fn validate_stored_values(&self) -> CoreResult<()> {
        for (_, value) in &self.set {
            crate::StoredValue::validate(value)?;
        }
        Ok(())
    }
}

fn sorted_deduped(values: impl IntoIterator<Item = DbString>) -> SmallVec<[DbString; 2]> {
    let mut values: SmallVec<[DbString; 2]> = values.into_iter().collect();
    if values.len() > 1 && !db_strings_are_canonical(&values) {
        values.sort();
        values.dedup();
    }
    values
}

fn db_strings_are_canonical(values: &[DbString]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn ensure_disjoint(
    kind: &'static str,
    added: &SmallVec<[DbString; 2]>,
    removed: &SmallVec<[DbString; 2]>,
) -> CoreResult<()> {
    if let Some(label) = first_overlap(added, removed) {
        return Err(CoreError::OverlappingDiff {
            kind,
            key: label.clone(),
        });
    }
    Ok(())
}

fn first_overlap<'a>(left: &'a [DbString], right: &[DbString]) -> Option<&'a DbString> {
    let mut left_index = 0;
    let mut right_index = 0;
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => return Some(&left[left_index]),
        }
    }
    None
}

fn first_property_overlap<'a>(
    set: &'a [(DbString, Value)],
    removed: &[DbString],
) -> Option<&'a DbString> {
    let mut set_index = 0;
    let mut removed_index = 0;
    while set_index < set.len() && removed_index < removed.len() {
        let key = &set[set_index].0;
        match key.cmp(&removed[removed_index]) {
            std::cmp::Ordering::Less => set_index += 1,
            std::cmp::Ordering::Greater => removed_index += 1,
            std::cmp::Ordering::Equal => return Some(key),
        }
    }
    None
}

fn validate_sorted_unique<E: serde::de::Error>(
    values: &SmallVec<[DbString; 2]>,
    label: &'static str,
) -> Result<(), E> {
    for window in values.windows(2) {
        if window[0] >= window[1] {
            return Err(E::custom(format!(
                "{label} must be sorted by DbString order with no duplicates"
            )));
        }
    }
    Ok(())
}

fn validate_disjoint<E: serde::de::Error>(
    added: &SmallVec<[DbString; 2]>,
    removed: &SmallVec<[DbString; 2]>,
    kind: &'static str,
) -> Result<(), E> {
    if let Some(label) = first_overlap(added, removed) {
        return Err(E::custom(format!(
            "overlapping {kind} diff: {label} appears in both add/set and remove",
        )));
    }
    Ok(())
}
