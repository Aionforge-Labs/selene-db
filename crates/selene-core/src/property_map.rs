//! Property maps per spec 02 section 5.2.
//!
//! Keys are ordered in memory lexicographically by [`DbString`] for fast
//! binary-search lookups. Serialize canonicalizes
//! (sorts) the keys before emitting — a no-op for the common case (construction
//! keeps them sorted) but load-bearing because the `Standard`/`Compact` variants
//! are public and can be built non-canonically. Deserialize then *validates* the
//! canonical invariant — strictly-ascending, no-duplicate keys — rejecting a
//! non-canonical payload as malformed rather than re-sorting it.
//! Compact maps are closed-shape views over a fixed key set; inserting an
//! unknown key widens them to the standard open representation.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smallvec::SmallVec;

use crate::{CoreError, CoreResult, DbString, Value};

const MAX_PROPERTY_COUNT: usize = u32::MAX as usize;

/// Property storage for open and closed graph values.
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyMap {
    /// Open graph representation: sorted key/value pairs.
    Standard(SmallVec<[(DbString, Value); 6]>),
    /// Closed graph representation: fixed sorted keys with positional values.
    Compact {
        /// Sorted key set defined by a schema type.
        keys: Arc<[DbString]>,
        /// Positional values aligned with `keys`; `None` means absent.
        values: SmallVec<[Option<Value>; 6]>,
    },
}

impl PropertyMap {
    /// Construct an empty standard property map.
    #[must_use]
    pub fn new() -> Self {
        Self::Standard(SmallVec::new())
    }

    /// Construct a standard property map from pairs, sorting by `DbString` order.
    ///
    /// Later duplicate keys overwrite earlier keys.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ConstructedValueTooLarge`] if the final distinct
    /// key count exceeds the implementation-defined cardinality cap.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (DbString, Value)>) -> CoreResult<Self> {
        let mut entries = pairs
            .into_iter()
            .collect::<SmallVec<[(DbString, Value); 6]>>();
        for (_, value) in &entries {
            crate::StoredValue::validate(value)?;
        }
        if entries.len() <= 1 {
            return Ok(Self::Standard(entries));
        }
        if entries.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            ensure_within_cap(entries.len())?;
            return Ok(Self::Standard(entries));
        }
        // `sort_by` is stable: equal keys keep source order, so the collapse
        // loop below preserves the documented "later duplicate wins" contract.
        entries.sort_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));

        let mut deduped = SmallVec::new();
        for (key, value) in entries {
            if let Some((last_key, last_value)) = deduped.last_mut()
                && last_key == &key
            {
                *last_value = value;
                continue;
            }
            deduped.push((key, value));
        }
        ensure_within_cap(deduped.len())?;
        Ok(Self::Standard(deduped))
    }

    /// Construct a compact property map from a fixed schema key set.
    ///
    /// Keys are sorted with their corresponding value slots. Duplicate keys
    /// keep the last value.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ConstructedValueTooLarge`] if key count exceeds the
    /// implementation-defined cardinality cap.
    pub fn compact(
        keys: impl IntoIterator<Item = DbString>,
        values: impl IntoIterator<Item = Option<Value>>,
    ) -> CoreResult<Self> {
        let keys: SmallVec<[DbString; 6]> = keys.into_iter().collect();
        let values: SmallVec<[Option<Value>; 6]> = values.into_iter().collect();
        for value in values.iter().flatten() {
            crate::StoredValue::validate(value)?;
        }
        if keys.len() != values.len() {
            return Err(CoreError::CompactKeyValueLengthMismatch {
                keys: keys.len(),
                values: values.len(),
            });
        }
        ensure_within_cap(keys.len())?;
        if keys.len() <= 1 {
            return Ok(Self::Compact {
                keys: Arc::from(keys.into_vec()),
                values,
            });
        }
        if keys.windows(2).all(|pair| pair[0] < pair[1]) {
            return Ok(Self::Compact {
                keys: Arc::from(keys.into_vec()),
                values,
            });
        }

        let mut slots: SmallVec<[(DbString, Option<Value>); 6]> =
            keys.into_iter().zip(values).collect();
        slots.sort_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));

        let mut deduped: SmallVec<[(DbString, Option<Value>); 6]> = SmallVec::new();
        for (key, value) in slots {
            if let Some((last_key, last_value)) = deduped.last_mut()
                && last_key == &key
            {
                *last_value = value;
                continue;
            }
            deduped.push((key, value));
        }
        let (keys, values): (Vec<_>, SmallVec<_>) = deduped.into_iter().unzip();
        Ok(Self::Compact {
            keys: Arc::from(keys),
            values,
        })
    }

    /// Return the value for `key`, if present.
    #[must_use]
    pub fn get(&self, key: &DbString) -> Option<&Value> {
        match self {
            Self::Standard(entries) => entries
                .binary_search_by(|(entry_key, _)| entry_key.cmp(key))
                .ok()
                .map(|idx| &entries[idx].1),
            Self::Compact { keys, values } => keys
                .binary_search(key)
                .ok()
                .and_then(|idx| values.get(idx))
                .and_then(Option::as_ref),
        }
    }

    /// Set `key` to `value`.
    ///
    /// Unknown-key writes against a compact map widen it to standard form and
    /// drop absent compact slots because standard maps only store present
    /// key/value pairs.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ConstructedValueTooLarge`] if inserting a distinct
    /// key would exceed the implementation-defined cardinality cap.
    pub fn set(&mut self, key: DbString, value: Value) -> CoreResult<()> {
        crate::StoredValue::validate(&value)?;
        match self {
            Self::Standard(entries) => set_standard(entries, key, value),
            Self::Compact { keys, values } => match keys.binary_search(&key) {
                Ok(idx) => {
                    values[idx] = Some(value);
                    Ok(())
                }
                Err(_) => {
                    let mut entries = compact_to_standard(keys, values);
                    set_standard(&mut entries, key, value)?;
                    *self = Self::Standard(entries);
                    Ok(())
                }
            },
        }
    }

    /// Remove a present property.
    pub fn remove(&mut self, key: &DbString) -> Option<Value> {
        match self {
            Self::Standard(entries) => entries
                .binary_search_by(|(entry_key, _)| entry_key.cmp(key))
                .ok()
                .map(|idx| entries.remove(idx).1),
            Self::Compact { keys, values } => keys
                .binary_search(key)
                .ok()
                .and_then(|idx| values.get_mut(idx))
                .and_then(Option::take),
        }
    }

    /// Number of present properties.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Standard(entries) => entries.len(),
            Self::Compact { values, .. } => values.iter().filter(|value| value.is_some()).count(),
        }
    }

    /// Return true if no properties are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate present key/value pairs in sorted-key order.
    ///
    /// Returns a concrete [`PropertyMapIter`] (not a boxed trait object): this
    /// is exercised per-label per-node on the commit path and per-write during
    /// validation, so the allocation a `Box<dyn Iterator>` would cost on every
    /// call is removed here.
    #[must_use]
    pub fn iter(&self) -> PropertyMapIter<'_> {
        match self {
            Self::Standard(entries) => PropertyMapIter::Standard(entries.iter()),
            Self::Compact { keys, values } => PropertyMapIter::Compact {
                keys: keys.iter(),
                values: values.iter(),
            },
        }
    }

    /// Iterate present property keys in sorted-key order.
    #[must_use]
    pub fn keys(&self) -> PropertyMapKeys<'_> {
        PropertyMapKeys(self.iter())
    }

    /// Iterate present property values in sorted-key order.
    #[must_use]
    pub fn values(&self) -> PropertyMapValues<'_> {
        PropertyMapValues(self.iter())
    }

    /// Return true if `key` has a present value.
    #[must_use]
    pub fn contains_key(&self, key: &DbString) -> bool {
        self.get(key).is_some()
    }

    /// Validate all payloads, including direct construction of public legacy
    /// variants, before entering a mutation or publication funnel.
    pub fn validate_stored_values(&self) -> CoreResult<()> {
        match self {
            Self::Standard(entries) => {
                for (_, value) in entries {
                    crate::StoredValue::validate(value)?;
                }
            }
            Self::Compact { values, .. } => {
                for value in values.iter().flatten() {
                    crate::StoredValue::validate(value)?;
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn sorted_invariant_holds(&self) -> bool {
        match self {
            Self::Standard(entries) => entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            Self::Compact { keys, values } => {
                keys.len() == values.len() && keys.windows(2).all(|pair| pair[0] < pair[1])
            }
        }
    }
}

impl Default for PropertyMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Borrowing iterator over a [`PropertyMap`]'s present key/value pairs.
///
/// Concrete (non-boxed) so the hot commit/validate paths pay no per-call
/// allocation. Yields pairs in sorted-key order for both representations.
#[derive(Debug)]
pub enum PropertyMapIter<'a> {
    /// Iterator over a [`PropertyMap::Standard`] entry slice.
    Standard(std::slice::Iter<'a, (DbString, Value)>),
    /// Iterator over a [`PropertyMap::Compact`] key/value slot pair.
    Compact {
        /// Sorted key slots.
        keys: std::slice::Iter<'a, DbString>,
        /// Positional value slots aligned with `keys`; `None` means absent.
        values: std::slice::Iter<'a, Option<Value>>,
    },
}

impl<'a> Iterator for PropertyMapIter<'a> {
    type Item = (&'a DbString, &'a Value);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Standard(entries) => entries.next().map(|(key, value)| (key, value)),
            Self::Compact { keys, values } => loop {
                // Compact maps store absent slots inline; skip them so the
                // observable sequence matches Standard's "present only" contract.
                let value = values.next()?;
                let key = keys.next()?;
                if let Some(value) = value.as_ref() {
                    return Some((key, value));
                }
            },
        }
    }
}

/// Borrowing iterator over a [`PropertyMap`]'s present keys in sorted-key order.
#[derive(Debug)]
pub struct PropertyMapKeys<'a>(PropertyMapIter<'a>);

impl<'a> Iterator for PropertyMapKeys<'a> {
    type Item = &'a DbString;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(key, _)| key)
    }
}

/// Borrowing iterator over a [`PropertyMap`]'s present values in sorted-key order.
#[derive(Debug)]
pub struct PropertyMapValues<'a>(PropertyMapIter<'a>);

impl<'a> Iterator for PropertyMapValues<'a> {
    type Item = &'a Value;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(_, value)| value)
    }
}

#[derive(Deserialize, Serialize)]
enum PropertyMapWire {
    Standard(SmallVec<[(DbString, Value); 6]>),
    Compact {
        keys: Arc<[DbString]>,
        values: SmallVec<[Option<Value>; 6]>,
    },
}

#[derive(Serialize)]
enum PropertyMapWireRef<'a> {
    Standard(&'a [(DbString, Value)]),
    Compact {
        keys: &'a [DbString],
        values: &'a [Option<Value>],
    },
}

impl Serialize for PropertyMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate_stored_values()
            .map_err(serde::ser::Error::custom)?;
        // Canonicalize on serialize. Construction through `set_standard` /
        // `compact` / `from_pairs` already keeps keys in lexicographic `DbString`
        // order, so this sort is a no-op (byte-identical) for those values. But
        // `Standard` / `Compact` are PUBLIC variants, so a caller can build a
        // non-canonical map directly; canonicalizing here guarantees the wire
        // is always canonical and round-trips through the strict (validate,
        // no-resort) deserializer below. The deserializer rejects a
        // non-canonical payload rather than silently re-sorting it.
        match self {
            Self::Standard(entries) => {
                if standard_entries_are_canonical(entries) {
                    return PropertyMapWireRef::Standard(entries.as_slice()).serialize(serializer);
                }

                let mut entries = entries.clone();
                entries.sort_by(|(lhs, _), (rhs, _)| lhs.as_str().cmp(rhs.as_str()));
                PropertyMapWire::Standard(entries).serialize(serializer)
            }
            Self::Compact { keys, values } => {
                if keys.len() == values.len() && compact_keys_are_canonical(keys) {
                    return PropertyMapWireRef::Compact {
                        keys,
                        values: values.as_slice(),
                    }
                    .serialize(serializer);
                }

                let mut pairs: Vec<(DbString, Option<Value>)> =
                    keys.iter().cloned().zip(values.iter().cloned()).collect();
                pairs.sort_by(|(lhs, _), (rhs, _)| lhs.as_str().cmp(rhs.as_str()));
                let (keys, values): (Vec<_>, SmallVec<_>) = pairs.into_iter().unzip();
                PropertyMapWire::Compact {
                    keys: Arc::from(keys),
                    values,
                }
                .serialize(serializer)
            }
        }
    }
}

fn standard_entries_are_canonical(entries: &[(DbString, Value)]) -> bool {
    entries.windows(2).all(|pair| pair[0].0 < pair[1].0)
}

fn compact_keys_are_canonical(keys: &[DbString]) -> bool {
    keys.windows(2).all(|pair| pair[0] < pair[1])
}

impl<'de> Deserialize<'de> for PropertyMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // The wire is canonical (lexicographic, dedup'd) by construction, so
        // the decoder validates that invariant rather than re-sorting:
        // strictly-ascending keys (which also rejects duplicates) and the
        // Compact key/value length match. A non-canonical or duplicate-keyed
        // payload is rejected as malformed.
        let wire = PropertyMapWire::deserialize(deserializer)?;
        match wire {
            PropertyMapWire::Standard(entries) => {
                for (_, value) in &entries {
                    crate::StoredValue::validate(value).map_err(serde::de::Error::custom)?;
                }
                for window in entries.windows(2) {
                    if window[0].0 >= window[1].0 {
                        return Err(serde::de::Error::custom(
                            "PropertyMap::Standard entries must be sorted by DbString order with no duplicate keys",
                        ));
                    }
                }
                Ok(Self::Standard(entries))
            }
            PropertyMapWire::Compact { keys, values } => {
                for value in values.iter().flatten() {
                    crate::StoredValue::validate(value).map_err(serde::de::Error::custom)?;
                }
                if keys.len() != values.len() {
                    return Err(serde::de::Error::custom(format!(
                        "PropertyMap::Compact key/value length mismatch: {} keys, {} values",
                        keys.len(),
                        values.len(),
                    )));
                }
                for window in keys.windows(2) {
                    if window[0] >= window[1] {
                        return Err(serde::de::Error::custom(
                            "PropertyMap::Compact keys must be sorted by DbString order with no duplicates",
                        ));
                    }
                }
                Ok(Self::Compact { keys, values })
            }
        }
    }
}

fn ensure_within_cap(count: usize) -> CoreResult<()> {
    if count > MAX_PROPERTY_COUNT {
        Err(CoreError::ConstructedValueTooLarge {
            got: count,
            max: u32::MAX,
        })
    } else {
        Ok(())
    }
}

fn set_standard(
    entries: &mut SmallVec<[(DbString, Value); 6]>,
    key: DbString,
    value: Value,
) -> CoreResult<()> {
    match entries.binary_search_by(|(entry_key, _)| entry_key.cmp(&key)) {
        Ok(idx) => {
            entries[idx].1 = value;
            Ok(())
        }
        Err(idx) => {
            ensure_within_cap(entries.len().saturating_add(1))?;
            entries.insert(idx, (key, value));
            Ok(())
        }
    }
}

fn compact_to_standard(
    keys: &Arc<[DbString]>,
    values: &SmallVec<[Option<Value>; 6]>,
) -> SmallVec<[(DbString, Value); 6]> {
    keys.iter()
        .cloned()
        .zip(values.iter())
        .filter_map(|(key, value)| value.clone().map(|value| (key, value)))
        .collect()
}

#[cfg(test)]
mod tests;
