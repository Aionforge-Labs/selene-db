//! Legacy default serde admission, retaining the existing Option/Value bytes.

use crate::{StoredValue, Value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub(super) fn serialize<S: Serializer>(
    value: &Option<Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if let Some(value) = value {
        StoredValue::validate(value).map_err(serde::ser::Error::custom)?;
    }
    value.serialize(serializer)
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    if let Some(value) = &value {
        StoredValue::validate(value).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}
