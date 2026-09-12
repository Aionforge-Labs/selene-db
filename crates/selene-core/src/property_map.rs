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
