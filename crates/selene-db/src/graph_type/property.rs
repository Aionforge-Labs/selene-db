use crate::{Error, PathSegment, Result, Type, Value};
use selene_graph::{
    GraphTypeDef, NodeTypeDef, PropertyDefaultValue, PropertyTypeDef, ValidationMode,
};

/// Validated property declaration using the facade's existing structural Type and Value.
/// Nullability is carried by Type. Defaults must already have the declared native
/// representation: this Rust API performs no implicit string-to-JSON or list-to-vector cast.
/// Unsupported schema types (including bounded lists and query references) fail early.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyDefinition(pub(crate) PropertyTypeDef, pub(crate) PathSegment);
// All fields of a validated PropertyTypeDef have reflexive equality; finite
// floating defaults are stored as canonical integer bits, not IEEE comparisons.
impl Eq for PropertyDefinition {}

impl PropertyDefinition {
    /// Validate a name/type pair. Existing schema nesting is limited to 64 levels.
    pub fn new(name: PathSegment, ty: Type) -> Result<Self> {
        let property = super::conversion::property(name.clone(), &ty)?;
        let result = Self(property, name);
        result.validate()?;
        Ok(result)
    }
    /// Add a typed default, rejecting mismatches and recursively non-storable values.
    pub fn with_default(mut self, value: Value) -> Result<Self> {
        let mut pending = vec![&value];
        let mut count = 0usize;
        while let Some(value) = pending.pop() {
            count += 1;
            let children = match value {
                Value::List(values) => values.len(),
                Value::Record(record) => {
                    let crate::Record::Open(fields) = record.as_ref();
                    fields.len()
                }
                Value::NodeRef(_) | Value::EdgeRef(_) | Value::GraphRef(_) | Value::Path(_) => {
                    return Err(Error::invalid_graph_type(
                        "query references and paths cannot be defaults",
                    ));
                }
                _ => 0,
            };
            if count.saturating_add(pending.len()).saturating_add(children) > 4096 {
                return Err(Error::invalid_graph_type(
                    "default exceeds 4096 value entries",
                ));
            }
            match value {
                Value::List(values) => pending.extend(values),
                Value::Record(record) => {
                    let crate::Record::Open(fields) = record.as_ref();
                    pending.extend(fields.iter().map(|(_, v)| v));
                }
                _ => {}
            }
        }
        value
            .validate_shape()
            .map_err(Error::invalid_graph_type_source)?;
        let lower = value.to_lower();
        selene_core::logical::Encoder::new(Default::default())
            .and_then(|mut e| e.value(&lower, 1))
            .map_err(Error::invalid_graph_type_source)?;
        self.0.default = Some(
            PropertyDefaultValue::from_value(&lower)
                .ok_or_else(|| Error::invalid_graph_type("unsupported property default"))?,
        );
        self.validate()?;
        Ok(self)
    }
    /// Enforce the engine's existing non-null single-property uniqueness rule.
    #[must_use]
    pub fn unique(mut self) -> Self {
        self.0.unique = true;
        self
    }
    /// Reject subsequent changes to a materialized property using existing immutability rules.
    #[must_use]
    pub fn immutable(mut self) -> Self {
        self.0.immutable = true;
        self
    }

    fn validate(&self) -> Result<()> {
        let name = selene_core::db_string("validation").expect("static name");
        GraphTypeDef {
            name: name.clone(),
            node_types: vec![NodeTypeDef {
                name: name.clone(),
                key_labels: selene_core::LabelSet::single(name),
                properties: vec![self.0.clone()],
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: vec![],
        }
        .validate_ref()
        .map_err(Error::invalid_graph_type_source)
    }
}
