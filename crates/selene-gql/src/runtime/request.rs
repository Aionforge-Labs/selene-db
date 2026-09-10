//! Facade request input and parameter preflight.

use std::{collections::BTreeMap, sync::Arc};

use selene_core::{DbString, GraphId, Record, Value};
use selene_graph::SeleneGraph;

use crate::{GqlType, SourceSpan, analyze::ParameterUse};

use super::{
    BindingTableRegistry, ExecutorError, parameter_type,
    request_runtime::{RequestRuntime, RequestRuntimeHandle},
};

/// Typed parameter input for one facade request.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct RequestParameter {
    declared_type: GqlType,
    value: Value,
}

impl RequestParameter {
    /// Construct one typed request parameter.
    #[must_use]
    pub const fn new(declared_type: GqlType, value: Value) -> Self {
        Self {
            declared_type,
            value,
        }
    }

    pub(crate) const fn declared_type(&self) -> &GqlType {
        &self.declared_type
    }

    pub(crate) const fn value(&self) -> &Value {
        &self.value
    }
}

/// Explicit lower-runtime input for one facade request.
#[doc(hidden)]
pub struct RequestExecutionInput {
    reference_graph: Option<GraphId>,
    pub(crate) parameters: BTreeMap<DbString, RequestParameter>,
    pub(crate) timestamp: jiff::Timestamp,
    pub(crate) time_zone: jiff::tz::TimeZone,
    pub(crate) runtime: Arc<RequestRuntime>,
}

impl RequestExecutionInput {
    /// Copy normalized parameter descriptors into semantic analysis without
    /// borrowing runtime values or modifying the immutable source tree.
    #[doc(hidden)]
    pub fn parameter_types(
        &self,
    ) -> Result<BTreeMap<DbString, selene_core::StructuralType>, ExecutorError> {
        self.parameters
            .iter()
            .map(|(name, parameter)| {
                crate::normalize_value_type(parameter.declared_type())
                    .map(|ty| (name.clone(), ty))
                    .map_err(|source| ExecutorError::Analysis {
                        source: crate::AnalysisError::StructuralType {
                            source,
                            span: SourceSpan::default(),
                        },
                    })
            })
            .collect()
    }
    /// Construct lower request input from a facade-owned immutable snapshot.
    #[must_use]
    pub fn new(
        parameters: BTreeMap<DbString, RequestParameter>,
        timestamp: jiff::Timestamp,
        time_zone: jiff::tz::TimeZone,
    ) -> Self {
        Self::with_runtime(
            parameters,
            timestamp,
            time_zone,
            RequestRuntimeHandle::new(),
        )
    }

    /// Construct lower input using an existing facade-owned request authority.
    #[doc(hidden)]
    #[must_use]
    pub fn with_runtime(
        parameters: BTreeMap<DbString, RequestParameter>,
        timestamp: jiff::Timestamp,
        time_zone: jiff::tz::TimeZone,
        runtime: RequestRuntimeHandle,
    ) -> Self {
        Self {
            reference_graph: None,
            parameters,
            timestamp,
            time_zone,
            runtime: runtime.inner(),
        }
    }

    pub(crate) fn runtime(&self) -> Arc<RequestRuntime> {
        Arc::clone(&self.runtime)
    }

    /// Attach the graph provenance checked by the owning facade before lowering
    /// opaque references into this graph-scoped row execution context.
    #[doc(hidden)]
    #[must_use]
    pub fn with_reference_graph(mut self, graph: Option<GraphId>) -> Self {
        self.reference_graph = graph;
        self
    }

    fn validate_domain(&self, graph: &SeleneGraph) -> Result<(), ExecutorError> {
        if self
            .reference_graph
            .is_some_and(|id| id != graph.graph_id())
        {
            return Err(ExecutorError::InvalidReference {
                name: "parameter reference belongs to another graph".to_owned(),
                span: SourceSpan::default(),
            });
        }
        Ok(())
    }
}

pub(super) fn validate(
    request: &RequestExecutionInput,
    uses: &[ParameterUse],
    graph: &SeleneGraph,
) -> Result<(), ExecutorError> {
    request.validate_domain(graph)?;
    let binding_tables = request.runtime.binding_tables();
    for (name, parameter) in &request.parameters {
        parameter_type::validate_declared_type(
            name.clone(),
            parameter.value(),
            parameter.declared_type(),
            SourceSpan::default(),
        )?;
        validate_value_references(
            name,
            parameter.value(),
            graph,
            &binding_tables,
            SourceSpan::default(),
        )?;
    }
    for parameter_use in uses {
        let parameter = request.parameters.get(&parameter_use.name).ok_or_else(|| {
            ExecutorError::UnboundParameter {
                name: parameter_use.name.clone(),
                span: parameter_use.span,
            }
        })?;
        if let Some(declared_type) = &parameter_use.declared_type {
            parameter_type::validate_declared_type(
                parameter_use.name.clone(),
                parameter.value(),
                declared_type,
                parameter_use.span,
            )?;
        }
    }
    Ok(())
}

pub(super) fn validate_references(
    request: &RequestExecutionInput,
    graph: &SeleneGraph,
) -> Result<(), ExecutorError> {
    request.validate_domain(graph)?;
    let binding_tables = request.runtime.binding_tables();
    for (name, parameter) in &request.parameters {
        validate_value_references(
            name,
            parameter.value(),
            graph,
            &binding_tables,
            SourceSpan::default(),
        )?;
    }
    Ok(())
}

fn validate_value_references(
    name: &DbString,
    value: &Value,
    graph: &SeleneGraph,
    binding_tables: &BindingTableRegistry,
    span: SourceSpan,
) -> Result<(), ExecutorError> {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::GraphRef(graph_id) if *graph_id != graph.graph_id() => {
                return Err(invalid_reference(
                    name,
                    "graph reference selects another graph",
                    span,
                ));
            }
            Value::Path(path) if path.graph != graph.graph_id() => {
                return Err(invalid_reference(name, "path selects another graph", span));
            }
            Value::TableRef(id) => {
                if let Err(error) = binding_tables.resolve(*id) {
                    return Err(invalid_reference(name, &error.to_string(), span));
                }
            }
            Value::List(values) => pending.extend(values),
            Value::Record(record) => {
                if let Record::Open(fields) = record.as_ref() {
                    pending.extend(fields.iter().map(|(_, value)| value));
                }
            }
            Value::RecordTyped(record) => pending.extend(record.values.iter().flatten()),
            _ => {}
        }
    }
    Ok(())
}

fn invalid_reference(name: &DbString, detail: &str, span: SourceSpan) -> ExecutorError {
    ExecutorError::InvalidReference {
        name: format!("parameter ${name}: {detail}"),
        span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BindingTable, BindingTableSchema, BindingTableType};
    use selene_core::{GraphId, db_string};
    use std::sync::Arc;

    fn request() -> RequestExecutionInput {
        RequestExecutionInput::new(
            BTreeMap::new(),
            jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
            jiff::tz::TimeZone::UTC,
        )
    }

    fn table() -> Arc<BindingTable> {
        Arc::new(BindingTable::empty(BindingTableSchema {
            columns: Vec::new(),
        }))
    }

    #[test]
    fn table_refs_resolve_only_through_their_request_authority() {
        let graph = SeleneGraph::new(GraphId::new(91_001));
        let mut owner = request();
        let id = owner.runtime.binding_tables().register(table()).unwrap();
        let name = db_string("table").unwrap();
        owner.parameters.insert(
            name.clone(),
            RequestParameter::new(
                GqlType::TableRef(BindingTableType::Any),
                Value::TableRef(id),
            ),
        );
        validate_references(&owner, &graph).expect("same-request table resolves");

        let mut foreign = request();
        foreign.parameters.insert(
            name,
            RequestParameter::new(
                GqlType::List(Box::new(GqlType::TableRef(BindingTableType::Any))),
                Value::List(vec![Value::TableRef(id)]),
            ),
        );
        let error = validate_references(&foreign, &graph).unwrap_err();
        assert_eq!(error.gqlstatus(), crate::GqlStatus::INVALID_REFERENCE);
        assert!(error.to_string().contains("another request"));
    }
}
