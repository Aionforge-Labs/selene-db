//! Typed facade request and session parameters.

use std::collections::BTreeMap;

use selene_core::DbString;

use crate::{Error, Result, Type, Value};

/// One explicitly declared GQL parameter value.
///
/// Construction and runtime declarations use the core structural type service.
/// The descriptor is owned and independent of compiler/source arenas.
#[derive(Clone, Debug, PartialEq)]
pub struct GeneralParameter {
    declared_type: Type,
    value: Value,
}

impl GeneralParameter {
    /// Validate and construct a typed parameter.
    ///
    /// # Errors
    ///
    /// Returns `22G03` when `value` does not satisfy `declared_type`.
    pub fn new(declared_type: Type, value: Value) -> Result<Self> {
        validate_parameter_type(&declared_type)?;
        Self::from_session_value(declared_type, value)
    }

    // Source session declarations can retain inferred analysis types. External
    // callers must supply a supported explicit declaration through `new`.
    pub(crate) fn from_session_value(declared_type: Type, value: Value) -> Result<Self> {
        value.validate_shape()?;
        selene_gql::validate_parameter_value(
            &value.to_lower(),
            &selene_gql::lower_value_type(&declared_type),
        )
        .map_err(Error::from_engine)?;
        Ok(Self {
            declared_type,
            value,
        })
    }

    /// Borrow the parameter's explicit declaration.
    #[must_use]
    pub const fn declared_type(&self) -> &Type {
        &self.declared_type
    }

    /// Borrow the parameter value.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.value
    }

    pub(crate) fn to_lower(&self) -> selene_gql::RequestParameter {
        selene_gql::RequestParameter::new(
            selene_gql::lower_value_type(&self.declared_type),
            self.value.to_lower(),
        )
    }
}

fn validate_parameter_type(ty: &Type) -> Result<()> {
    use crate::TypeKind;
    match ty.kind() {
        TypeKind::Dynamic
        | TypeKind::Property
        | TypeKind::Null
        | TypeKind::Empty
        | TypeKind::TableRef(_) => {
            return Err(Error::from_engine(
                selene_gql::ExecutorError::DataException {
                    subclass: selene_gql::DataExceptionSubclass::InvalidValueType,
                    message: "parameter declaration is not a supported facade value type".into(),
                    span: selene_gql::SourceSpan::default(),
                },
            ));
        }
        TypeKind::List { element, .. } => validate_parameter_type(element)?,
        TypeKind::Record(Some(fields)) => {
            for (_, field) in fields.iter() {
                validate_parameter_type(field)?;
            }
        }
        TypeKind::Union(members) => {
            for member in members.iter() {
                validate_parameter_type(member)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Deterministic exact-name request parameter dictionary.
///
/// Names omit `$`, are case-sensitive, and follow the parser's Unicode
/// parameter rule. Insertion rejects an existing exact name rather than
/// replacing it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestParams {
    entries: BTreeMap<DbString, GeneralParameter>,
}

impl RequestParams {
    /// Construct an empty dictionary.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Insert a parameter after validating its decoded name.
    ///
    /// # Errors
    ///
    /// Returns an invalid-name diagnostic for spellings the GQL parser would
    /// reject after `$`, or a duplicate diagnostic when the exact name exists.
    pub fn insert(&mut self, name: &str, parameter: GeneralParameter) -> Result<()> {
        let name = validated_parameter_name(name)?;
        if self.entries.contains_key(&name) {
            return Err(Error::duplicate_parameter(name.as_str()));
        }
        self.entries.insert(name, parameter);
        Ok(())
    }

    /// Borrow one exact-name parameter.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&GeneralParameter> {
        self.entries.get(name)
    }

    /// Return the number of request parameters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether this dictionary is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate in exact-name lexical order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &GeneralParameter)> {
        self.entries
            .iter()
            .map(|(name, parameter)| (name.as_str(), parameter))
    }

    pub(crate) fn overlay(session: &BTreeMap<DbString, GeneralParameter>, request: &Self) -> Self {
        let mut entries = session.clone();
        entries.extend(request.entries.clone());
        Self { entries }
    }

    pub(crate) fn to_lower(&self) -> BTreeMap<DbString, selene_gql::RequestParameter> {
        self.entries
            .iter()
            .map(|(name, parameter)| (name.clone(), parameter.to_lower()))
            .collect()
    }

    pub(crate) fn reference_graph(
        &self,
        database: crate::DatabaseId,
    ) -> Result<Option<crate::GraphId>> {
        let mut domain = None;
        for (_, parameter) in self.iter() {
            if let Some(graph) = parameter.value.reference_graph(database)? {
                if domain.is_some_and(|previous| previous != graph) {
                    return Err(Error::invalid_runtime_reference(
                        "reference belongs to another graph",
                    ));
                }
                domain = Some(graph);
            }
        }
        Ok(domain)
    }
}

pub(crate) fn validated_parameter_name(name: &str) -> Result<DbString> {
    if !selene_gql::is_parameter_name(name) {
        return Err(Error::invalid_parameter_name(name));
    }
    selene_core::db_string(name)
        .map_err(|source| Error::invalid_parameter_name_source(name, source))
}
