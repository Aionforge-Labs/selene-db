use std::{cell::RefCell, fmt, sync::Arc};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{DeserializeSeed, Error as DeError, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map as SerdeJsonMap, Value as SerdeJsonValue, map::Entry as SerdeJsonMapEntry};

use crate::{
    CoreError, CoreResult, DbString, db_string::MAX_DB_STRING_BYTES, json_patch::apply_json_patch,
};

/// Map-key token used by serde-json's arbitrary-precision number transport.
///
/// With that feature unified into a consumer build, `Deserializer::deserialize_any`
/// presents a numeric lexeme as a one-entry map instead of calling a numeric
/// visitor method. Recognizing the token keeps the dependency feature from
/// changing a JSON number into an object during strict parsing.
const SERDE_JSON_ARBITRARY_PRECISION_NUMBER_TOKEN: &str = "$serde_json::private::Number";

/// Selector used by JSON path-existence helpers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonPathSelector {
    /// Select an object member by key.
    Key(DbString),
    /// Select an array element by signed index.
    ///
    /// Negative indexes count from the end, matching `json_has_path`.
    Index(i64),
    /// Select an array element by unsigned index.
    UnsignedIndex(u64),
}

/// Native JSON payload stored as a first-class [`crate::Value`].
///
/// `JsonValue` validates once at construction and deserialization so every
/// engine layer can assume a finite JSON data-model value whose strings and
/// container cardinalities stay inside the implementation-defined GQL caps.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonValue {
    value: Arc<SerdeJsonValue>,
}

/// Borrowed JSON subvalue selected from a validated [`JsonValue`].
///
/// This keeps scan paths from cloning selected JSON values until the caller
/// knows the value must be materialized into an owned result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JsonValueRef<'a> {
    value: &'a SerdeJsonValue,
}

impl<'a> JsonValueRef<'a> {
    /// Borrow the underlying serde-json value.
    #[must_use]
    pub fn as_serde(self) -> &'a SerdeJsonValue {
        self.value
    }

    /// Clone this validated subvalue into an owned [`JsonValue`].
    #[must_use]
    pub fn to_owned_json_value(self) -> JsonValue {
        JsonValue {
            value: Arc::new(self.value.clone()),
        }
    }
}

impl JsonValue {
    /// Build a validated JSON value from an owned serde-json value.
    pub fn new(value: SerdeJsonValue) -> CoreResult<Self> {
        validate_json_value(&value)?;
        Ok(Self::from_validated(value))
    }

    fn from_validated(value: SerdeJsonValue) -> Self {
        Self {
            value: Arc::new(value),
        }
    }

    /// Parse and validate a JSON value from text.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::JsonParse`] when `text` is not valid JSON, or the
    /// usual value-limit errors when the parsed value exceeds engine caps.
    pub fn parse_str(text: &str) -> CoreResult<Self> {
        let value = parse_json_text(text).map_err(JsonTextError::into_core_error)?;
        Ok(Self::from_validated(value))
    }

    /// Borrow the underlying serde-json value.
    #[must_use]
    pub fn as_serde(&self) -> &SerdeJsonValue {
        &self.value
    }

    /// Clone the shared JSON storage.
    #[must_use]
    pub fn as_arc(&self) -> Arc<SerdeJsonValue> {
        Arc::clone(&self.value)
    }

    /// Return a stable compact JSON rendering with object keys sorted.
    #[must_use]
    pub fn to_canonical_string(&self) -> String {
        // serde_json::Map is BTreeMap-backed in this workspace (no
        // `preserve_order` feature), so compact whole-value serialization
        // preserves the canonical key order established at parse/construction.
        serde_json::to_string(self.as_serde()).expect("validated JSON rendering succeeds")
    }

    /// Return the JSON data-model type name.
    #[must_use]
    pub fn json_type_name(&self) -> &'static str {
        match self.as_serde() {
            SerdeJsonValue::Null => "null",
            SerdeJsonValue::Bool(_) => "boolean",
            SerdeJsonValue::Number(_) => "number",
            SerdeJsonValue::String(_) => "string",
            SerdeJsonValue::Array(_) => "array",
            SerdeJsonValue::Object(_) => "object",
        }
    }

    /// Return true when this JSON value recursively contains `candidate`.
    ///
    /// Objects contain candidates whose keys are present and whose values are
    /// themselves contained. Arrays contain array candidates when each
    /// candidate element is contained by at least one target element. A scalar
    /// or object candidate also matches any containing target array element.
    #[must_use]
    pub fn contains(&self, candidate: &Self) -> bool {
        json_contains_value(self.as_serde(), candidate.as_serde())
    }

    /// Return true when every selector in `path` resolves inside this value.
    ///
    /// Stored-value shape mismatches return false. This makes the helper useful
    /// for heterogeneous graph scans where one malformed document should not
    /// abort the entire candidate search.
    #[must_use]
    pub fn path_exists(&self, path: &[JsonPathSelector]) -> bool {
        select_json_path(self.as_serde(), path).is_some()
    }

    /// Return the JSON subvalue selected by `path`.
    ///
    /// Stored-value shape mismatches return [`None`]. A selected JSON `null`
    /// still returns `Some(JsonValue)`, which lets callers distinguish present
    /// null values from absent paths.
    #[must_use]
    pub fn path_value(&self, path: &[JsonPathSelector]) -> Option<Self> {
        self.path_value_ref(path)
            .map(JsonValueRef::to_owned_json_value)
    }

    /// Borrow the JSON subvalue selected by `path`.
    ///
    /// Stored-value shape mismatches return [`None`]. A selected JSON `null`
    /// still returns `Some(JsonValueRef)`, which lets callers distinguish
    /// present null values from absent paths without cloning the subvalue.
    #[must_use]
    pub fn path_value_ref(&self, path: &[JsonPathSelector]) -> Option<JsonValueRef<'_>> {
        select_json_path(self.as_serde(), path).map(|value| JsonValueRef { value })
    }

    /// Return true when `path` selects a JSON value that contains `candidate`.
    ///
    /// Stored-value shape mismatches return false, matching
    /// [`Self::path_exists`]. Containment follows [`Self::contains`] semantics
    /// on the selected subvalue.
    #[must_use]
    pub fn path_contains(&self, path: &[JsonPathSelector], candidate: &Self) -> bool {
        select_json_path(self.as_serde(), path)
            .is_some_and(|value| json_contains_value(value, candidate.as_serde()))
    }

    /// Return the result of applying an RFC 7396 JSON Merge Patch document.
    ///
    /// The operation is copy-on-write: this value and `patch` are left
    /// unchanged, and the merged result is validated as a fresh [`JsonValue`].
    ///
    /// # Errors
    ///
    /// Returns the usual value-limit errors if the merged result exceeds engine
    /// caps.
    pub fn merge_patch(&self, patch: &Self) -> CoreResult<Self> {
        let mut target = self.as_serde().clone();
        merge_patch_value(&mut target, patch.as_serde());
        Self::new(target)
    }

    /// Return the result of applying an RFC 6902 JSON Patch document.
    ///
    /// The operation is atomic from the caller's perspective: this value and
    /// `patch` are left unchanged, and an invalid patch returns an error
    /// without exposing any intermediate document state.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::JsonPatch`] when the patch document is malformed or
    /// an operation fails. Returns the usual value-limit errors if the patched
    /// result exceeds engine caps.
    pub fn apply_patch(&self, patch: &Self) -> CoreResult<Self> {
        Self::new(apply_json_patch(self.as_serde(), patch.as_serde())?)
    }
}

fn select_json_path<'a>(
    mut current: &'a SerdeJsonValue,
    path: &[JsonPathSelector],
) -> Option<&'a SerdeJsonValue> {
    for selector in path {
        current = select_json_child(current, selector)?;
    }
    Some(current)
}

fn select_json_child<'a>(
    current: &'a SerdeJsonValue,
    selector: &JsonPathSelector,
) -> Option<&'a SerdeJsonValue> {
    match (current, selector) {
        (SerdeJsonValue::Object(values), JsonPathSelector::Key(key)) => values.get(key.as_str()),
        (SerdeJsonValue::Array(values), JsonPathSelector::Index(index)) => {
            signed_array_index(*index, values.len()).and_then(|index| values.get(index))
        }
        (SerdeJsonValue::Array(values), JsonPathSelector::UnsignedIndex(index)) => {
            usize::try_from(*index)
                .ok()
                .and_then(|index| values.get(index))
        }
        _ => None,
    }
}

fn signed_array_index(index: i64, len: usize) -> Option<usize> {
    if index >= 0 {
        usize::try_from(index).ok().filter(|index| *index < len)
    } else {
        let offset = usize::try_from(index.unsigned_abs()).ok()?;
        len.checked_sub(offset)
    }
}

impl TryFrom<SerdeJsonValue> for JsonValue {
    type Error = CoreError;

    fn try_from(value: SerdeJsonValue) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<JsonValue> for SerdeJsonValue {
    fn from(value: JsonValue) -> Self {
        value.as_serde().clone()
    }
}

impl Serialize for JsonValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            self.as_serde().serialize(serializer)
        } else {
            serializer.serialize_str(&self.to_canonical_string())
        }
    }
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = if deserializer.is_human_readable() {
            StrictJsonValueSeed::plain().deserialize(deserializer)?
        } else {
            let value = String::deserialize(deserializer)?;
            parse_json_text(&value).map_err(serde::de::Error::custom)?
        };
        Ok(Self::from_validated(value))
    }
}

enum JsonTextError {
    Json(serde_json::Error),
    Value(CoreError),
}

impl JsonTextError {
    fn into_core_error(self) -> CoreError {
        match self {
            Self::Json(err) => CoreError::JsonParse {
                message: err.to_string(),
            },
            Self::Value(err) => err,
        }
    }
}

impl fmt::Display for JsonTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => err.fmt(formatter),
            Self::Value(err) => err.fmt(formatter),
        }
    }
}

fn parse_json_text(text: &str) -> Result<SerdeJsonValue, JsonTextError> {
    let value_error = RefCell::new(None);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = match StrictJsonValueSeed::tracking(&value_error).deserialize(&mut deserializer) {
        Ok(value) => value,
        Err(err) => {
            return Err(value_error
                .into_inner()
                .map_or(JsonTextError::Json(err), JsonTextError::Value));
        }
    };
    deserializer.end().map_err(JsonTextError::Json)?;
    Ok(value)
}

#[derive(Clone, Copy)]
struct StrictJsonValueSeed<'a> {
    value_error: Option<&'a RefCell<Option<CoreError>>>,
}

impl StrictJsonValueSeed<'_> {
    fn plain() -> Self {
        Self { value_error: None }
    }

    fn tracking<'a>(value_error: &'a RefCell<Option<CoreError>>) -> StrictJsonValueSeed<'a> {
        StrictJsonValueSeed {
            value_error: Some(value_error),
        }
    }

    fn value_error<E>(self, error: CoreError) -> E
    where
        E: DeError,
    {
        match self.value_error {
            Some(value_error) => {
                let message = error.to_string();
                if value_error.borrow().is_none() {
                    *value_error.borrow_mut() = Some(error);
                }
                E::custom(message)
            }
            None => E::custom(error),
        }
    }

    fn validate_string_len<E>(self, len: usize) -> Result<(), E>
    where
        E: DeError,
    {
        validate_json_string_len(len).map_err(|err| self.value_error(err))
    }

    fn ensure_container_len<E>(self, len: usize) -> Result<(), E>
    where
        E: DeError,
    {
        ensure_json_container_len(len).map_err(|err| self.value_error(err))
    }
}

impl<'de> DeserializeSeed<'de> for StrictJsonValueSeed<'_> {
    type Value = SerdeJsonValue;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonValueVisitor { seed: self })
    }
}

struct StrictJsonValueVisitor<'a> {
    seed: StrictJsonValueSeed<'a>,
}

impl<'de> Visitor<'de> for StrictJsonValueVisitor<'_> {
    type Value = SerdeJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value with unique object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(SerdeJsonValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(SerdeJsonValue::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(SerdeJsonValue::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        let number = serde_json::Number::from_f64(value)
            .ok_or_else(|| E::custom("JSON number is not finite"))?;
        Ok(SerdeJsonValue::Number(number))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        self.seed.validate_string_len(value.len())?;
        Ok(SerdeJsonValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        self.seed.validate_string_len(value.len())?;
        Ok(SerdeJsonValue::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(SerdeJsonValue::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(SerdeJsonValue::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.seed.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(value) = seq.next_element_seed(self.seed)? {
            values.push(value);
        }
        self.seed.ensure_container_len(values.len())?;
        Ok(SerdeJsonValue::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = SerdeJsonMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.is_empty() && key == SERDE_JSON_ARBITRARY_PRECISION_NUMBER_TOKEN {
                let lexeme = map.next_value::<String>()?;
                let number = lexeme.parse().map_err(A::Error::custom)?;
                if map.next_key::<String>()?.is_some() {
                    return Err(A::Error::custom(
                        "arbitrary-precision JSON number transport has trailing fields",
                    ));
                }
                return Ok(SerdeJsonValue::Number(number));
            }
            self.seed.validate_string_len(key.len())?;
            match values.entry(key) {
                SerdeJsonMapEntry::Vacant(entry) => {
                    let value = map.next_value_seed(self.seed)?;
                    entry.insert(value);
                }
                SerdeJsonMapEntry::Occupied(entry) => {
                    return Err(A::Error::custom(format!(
                        "duplicate JSON object key '{}'",
                        entry.key()
                    )));
                }
            }
        }
        self.seed.ensure_container_len(values.len())?;
        Ok(SerdeJsonValue::Object(values))
    }
}

fn validate_json_value(value: &SerdeJsonValue) -> CoreResult<()> {
    match value {
        SerdeJsonValue::Null | SerdeJsonValue::Bool(_) | SerdeJsonValue::Number(_) => Ok(()),
        SerdeJsonValue::String(value) => {
            validate_json_string_len(value.len())?;
            Ok(())
        }
        SerdeJsonValue::Array(values) => {
            ensure_json_container_len(values.len())?;
            for value in values {
                validate_json_value(value)?;
            }
            Ok(())
        }
        SerdeJsonValue::Object(values) => {
            ensure_json_container_len(values.len())?;
            for (key, value) in values {
                validate_json_string_len(key.len())?;
                validate_json_value(value)?;
            }
            Ok(())
        }
    }
}

fn validate_json_string_len(len: usize) -> CoreResult<()> {
    if len > MAX_DB_STRING_BYTES {
        return Err(CoreError::StringTooLong {
            got: len,
            max: u32::MAX,
        });
    }
    Ok(())
}

fn ensure_json_container_len(len: usize) -> CoreResult<()> {
    if len > u32::MAX as usize {
        return Err(CoreError::ConstructedValueTooLarge {
            got: len,
            max: u32::MAX,
        });
    }
    Ok(())
}

fn json_contains_value(target: &SerdeJsonValue, candidate: &SerdeJsonValue) -> bool {
    match (target, candidate) {
        (SerdeJsonValue::Object(target), SerdeJsonValue::Object(candidate)) => {
            candidate.iter().all(|(key, value)| {
                target
                    .get(key)
                    .is_some_and(|found| json_contains_value(found, value))
            })
        }
        (SerdeJsonValue::Array(target), SerdeJsonValue::Array(candidate)) => candidate
            .iter()
            .all(|value| target.iter().any(|found| json_contains_value(found, value))),
        (SerdeJsonValue::Array(target), candidate) => target
            .iter()
            .any(|found| json_contains_value(found, candidate)),
        _ => target == candidate,
    }
}

fn merge_patch_value(target: &mut SerdeJsonValue, patch: &SerdeJsonValue) {
    let SerdeJsonValue::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };

    if !target.is_object() {
        *target = SerdeJsonValue::Object(SerdeJsonMap::new());
    }
    let target = target
        .as_object_mut()
        .expect("target was normalized to object");

    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            let entry = target.entry(key.clone()).or_insert(SerdeJsonValue::Null);
            merge_patch_value(entry, value);
        }
    }
}

#[cfg(test)]
mod tests;
