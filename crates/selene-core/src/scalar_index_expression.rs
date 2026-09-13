//! Bounded, deterministic scalar index programs. No source text is executable here.

use serde::{Deserialize, Serialize};

use crate::{PropertyMap, Value, db_string};

/// One constant JSON selector in an analyzed scalar expression.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ScalarIndexSelector {
    /// Exact object member name, after source escape decoding.
    Key(String),
    /// Signed array index; negative positions count from the end.
    Index(i64),
}

/// Scalar operations admitted by expression-index semantics version one.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ScalarIndexOperation {
    /// Unicode lowercase, with the ordinary scalar function's null behavior.
    Lower,
    /// Unicode uppercase, with the ordinary scalar function's null behavior.
    Upper,
    /// Select a native JSON scalar, without converting it to a string.
    JsonScalarPath(Vec<ScalarIndexSelector>),
    /// Select text with the explicit `json_get_path_text` function semantics.
    JsonTextPath(Vec<ScalarIndexSelector>),
}

/// A same-element program with one exact source-property dependency.
///
/// Operations are applied in order. This representation cannot contain parameters,
/// traversal, procedures, time, randomness, or multi-valued operations. Admission
/// additionally validates bounds and the semantic/profile coordinate. Dynamic data
/// can still fail evaluation; an index must not claim completeness for such data.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScalarIndexExpression {
    /// Evaluator contract, independent of package version.
    pub semantics: u32,
    /// Exact property name, not a synthetic materialized property.
    pub property: String,
    /// Bounded operations in evaluation order.
    pub operations: Vec<ScalarIndexOperation>,
}

/// A scalar-index evaluation failure. Callers must retain ordinary query errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ScalarIndexEvaluationError {
    /// The program is invalid or uses an unknown semantic coordinate.
    #[error("invalid scalar index expression")]
    InvalidExpression,
    /// A source value or selector has the wrong type.
    #[error("scalar index expression encountered an incompatible value")]
    Type,
    /// Extraction selected a JSON array or object instead of a scalar.
    #[error("selected JSON value is not a scalar")]
    Container,
    /// A result exceeds the engine's scalar representation limits.
    #[error("scalar index result is outside supported ranges")]
    Range,
}

impl ScalarIndexExpression {
    /// Validate the bounded program, including all decoded selector names.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.semantics == 1
            && !self.property.is_empty()
            && db_string(&self.property).is_ok()
            && self.operations.len() <= 16
            && self.operations.iter().all(|operation| match operation {
                ScalarIndexOperation::Lower | ScalarIndexOperation::Upper => true,
                ScalarIndexOperation::JsonScalarPath(path)
                | ScalarIndexOperation::JsonTextPath(path) => {
                    (1..=64).contains(&path.len())
                        && path.iter().all(|selector| match selector {
                            ScalarIndexSelector::Key(key) => db_string(key).is_ok(),
                            ScalarIndexSelector::Index(_) => true,
                        })
                }
            })
    }

    /// Evaluate against one element's properties. Missing and GQL null propagate
    /// null; selected JSON null also becomes GQL null, as in the scalar functions.
    /// Type errors are never represented as missing keys or nonmatches.
    pub fn evaluate(&self, properties: &PropertyMap) -> Result<Value, ScalarIndexEvaluationError> {
        if !self.is_valid() {
            return Err(ScalarIndexEvaluationError::InvalidExpression);
        }
        let key =
            db_string(&self.property).map_err(|_| ScalarIndexEvaluationError::InvalidExpression)?;
        let mut value = properties.get(&key).cloned().unwrap_or(Value::Null);
        for operation in &self.operations {
            if matches!(value, Value::Null) {
                continue;
            }
            value = match operation {
                ScalarIndexOperation::Lower | ScalarIndexOperation::Upper => {
                    let Value::String(text) = value else {
                        return Err(ScalarIndexEvaluationError::Type);
                    };
                    let text = if matches!(operation, ScalarIndexOperation::Lower) {
                        text.as_str().to_lowercase()
                    } else {
                        text.as_str().to_uppercase()
                    };
                    Value::String(db_string(&text).map_err(|_| ScalarIndexEvaluationError::Range)?)
                }
                ScalarIndexOperation::JsonScalarPath(path)
                | ScalarIndexOperation::JsonTextPath(path) => {
                    let Value::Json(json) = value else {
                        return Err(ScalarIndexEvaluationError::Type);
                    };
                    extract(
                        json.as_serde(),
                        path,
                        matches!(operation, ScalarIndexOperation::JsonTextPath(_)),
                    )?
                }
            };
        }
        Ok(value)
    }
}

fn extract(
    mut value: &serde_json::Value,
    path: &[ScalarIndexSelector],
    text: bool,
) -> Result<Value, ScalarIndexEvaluationError> {
    use serde_json::Value as J;
    for selector in path {
        let next = match (value, selector) {
            (J::Object(object), ScalarIndexSelector::Key(key)) => object.get(key),
            (J::Array(array), ScalarIndexSelector::Index(index)) => {
                let position = if *index >= 0 {
                    usize::try_from(*index).ok()
                } else {
                    usize::try_from(index.unsigned_abs())
                        .ok()
                        .and_then(|offset| array.len().checked_sub(offset))
                };
                position.and_then(|position| array.get(position))
            }
            (J::Object(_) | J::Array(_), _) => return Err(ScalarIndexEvaluationError::Type),
            _ => None,
        };
        let Some(next) = next else {
            return Ok(Value::Null);
        };
        value = next;
    }
    let string = |text: &str| {
        db_string(text)
            .map(Value::String)
            .map_err(|_| ScalarIndexEvaluationError::Range)
    };
    match value {
        J::Null => Ok(Value::Null),
        J::String(value) => string(value),
        other if text => {
            let json = crate::JsonValue::new(other.clone())
                .map_err(|_| ScalarIndexEvaluationError::Range)?;
            string(&json.to_canonical_string())
        }
        J::Bool(value) => Ok(Value::Bool(*value)),
        J::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(Value::Int(value))
            } else if let Some(value) = value.as_u64() {
                Ok(Value::Uint(value))
            } else if let Some(value) = value.as_f64().filter(|value| value.is_finite()) {
                Ok(Value::Float(value))
            } else {
                Err(ScalarIndexEvaluationError::Range)
            }
        }
        J::Array(_) | J::Object(_) => Err(ScalarIndexEvaluationError::Container),
    }
}
