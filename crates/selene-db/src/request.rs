//! Facade request values, immutable context, and top-level outcome.

use std::sync::Arc;

use crate::{
    DiagnosticBundle, Error, ExecutionOutcome, RequestParams, Result,
    session::cache::RequestPlanKey,
};

/// One source statement and its request-scoped parameters.
#[derive(Clone, Debug)]
pub struct Request {
    source: String,
    parameters: RequestParams,
}

impl Request {
    /// Construct a request without request-scoped parameters.
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            parameters: RequestParams::new(),
        }
    }

    /// Construct a request from a validated parameter dictionary.
    #[must_use]
    pub fn with_params(source: impl Into<String>, parameters: RequestParams) -> Self {
        Self {
            source: source.into(),
            parameters,
        }
    }

    /// Borrow the GQL source statement.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Borrow the request-scoped parameter dictionary.
    #[must_use]
    pub const fn parameters(&self) -> &RequestParams {
        &self.parameters
    }

    /// Mutably borrow the validated request dictionary.
    pub const fn parameters_mut(&mut self) -> &mut RequestParams {
        &mut self.parameters
    }
}

/// Immutable wall-clock instant captured once when a request starts.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestTimestamp(jiff::Timestamp);

impl RequestTimestamp {
    pub(crate) fn capture() -> Self {
        Self(jiff::Timestamp::now())
    }

    #[cfg(test)]
    pub(crate) fn from_parts(seconds: i64, nanoseconds: i32) -> Self {
        Self(jiff::Timestamp::new(seconds, nanoseconds).expect("test timestamp is in range"))
    }

    /// Return whole seconds from the Unix epoch.
    #[must_use]
    pub fn unix_seconds(self) -> i64 {
        self.0.as_second()
    }

    /// Return the signed fractional nanosecond component.
    #[must_use]
    pub fn subsec_nanoseconds(self) -> i32 {
        self.0.subsec_nanosecond()
    }

    pub(crate) const fn lower(self) -> jiff::Timestamp {
        self.0
    }
}

/// Immutable parameter and temporal snapshot associated with one execution.
///
/// The context contains no graph allocation, lifecycle lease, catalog snapshot,
/// transaction, or physical row. Its private request-runtime handle owns the
/// execution stack, status collection, and sole binding-table authority; none
/// of those lower runtime types is exposed through the facade.
#[derive(Debug)]
pub struct RequestContext {
    parameters: RequestParams,
    timestamp: RequestTimestamp,
    runtime: selene_gql::RequestRuntimeHandle,
}

impl RequestContext {
    pub(crate) fn new(parameters: RequestParams, timestamp: RequestTimestamp) -> Self {
        Self {
            parameters,
            timestamp,
            runtime: selene_gql::RequestRuntimeHandle::new(),
        }
    }

    /// Borrow the merged session/request parameter snapshot.
    #[must_use]
    pub const fn parameters(&self) -> &RequestParams {
        &self.parameters
    }

    /// Return the instant captured when this request began.
    #[must_use]
    pub const fn timestamp(&self) -> RequestTimestamp {
        self.timestamp
    }

    pub(crate) fn lower_input(
        &self,
        time_zone: jiff::tz::TimeZone,
        database: crate::DatabaseId,
    ) -> Result<selene_gql::RequestExecutionInput> {
        let reference_graph = self.parameters.reference_graph(database)?;
        Ok(selene_gql::RequestExecutionInput::with_runtime(
            self.parameters.to_lower(),
            self.timestamp.lower(),
            time_zone,
            self.runtime.clone(),
        )
        .with_reference_graph(reference_graph.map(|id| selene_core::GraphId::new(id.get()))))
    }

    pub(crate) fn plan_key(&self, source: &str) -> RequestPlanKey {
        RequestPlanKey {
            source: source.to_owned(),
            parameter_types: self
                .parameters
                .iter()
                .map(|(name, parameter)| (name.to_owned(), parameter.declared_type().clone()))
                .collect(),
        }
    }
}

/// Uniform result of request validation, compilation, dispatch, and execution.
#[derive(Debug)]
#[non_exhaustive]
pub enum RequestOutcome {
    /// Request execution completed with the existing statement summary.
    Succeeded {
        /// Context that was active for the request.
        context: Arc<RequestContext>,
        /// Existing statement-level summary.
        outcome: ExecutionOutcome,
    },
    /// Request validation, compilation, dispatch, or execution failed.
    Failed {
        /// Context that was active, or was refused because another was active.
        context: Arc<RequestContext>,
        /// Facade diagnostic with its original source chain and GQLSTATUS.
        error: Error,
        /// Structured primary/additional/nested request diagnostics.
        diagnostics: DiagnosticBundle,
    },
}

impl RequestOutcome {
    /// Borrow the request context retained by either outcome variant.
    #[must_use]
    pub const fn context(&self) -> &Arc<RequestContext> {
        match self {
            Self::Succeeded { context, .. } | Self::Failed { context, .. } => context,
        }
    }

    /// Borrow the successful statement summary, when present.
    #[must_use]
    pub const fn execution(&self) -> Option<&ExecutionOutcome> {
        match self {
            Self::Succeeded { outcome, .. } => Some(outcome),
            Self::Failed { .. } => None,
        }
    }

    /// Borrow the request failure, when present.
    #[must_use]
    pub const fn error(&self) -> Option<&Error> {
        match self {
            Self::Failed { error, .. } => Some(error),
            Self::Succeeded { .. } => None,
        }
    }

    /// Borrow the structured diagnostics for either success or failure.
    #[must_use]
    pub const fn diagnostics(&self) -> &DiagnosticBundle {
        match self {
            Self::Succeeded { outcome, .. } => outcome.diagnostics(),
            Self::Failed { diagnostics, .. } => diagnostics,
        }
    }

    pub(crate) fn failed(context: Arc<RequestContext>, error: Error) -> Self {
        let diagnostics =
            DiagnosticBundle::from_error_and_engine_statuses(&error, &context.runtime.statuses());
        Self::Failed {
            context,
            error,
            diagnostics,
        }
    }

    /// Convert to the compatibility result returned by [`Session::execute`](crate::Session::execute).
    pub fn into_result(self) -> Result<ExecutionOutcome> {
        match self {
            Self::Succeeded { outcome, .. } => Ok(outcome),
            Self::Failed { error, .. } => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    #[test]
    fn binding_table_program_limit_is_a_structured_failed_request() {
        let context = Arc::new(RequestContext::new(
            RequestParams::new(),
            RequestTimestamp::from_parts(1_788_692_096, 0),
        ));
        let error = Error::from_engine(selene_gql::ExecutorError::ProgramLimitExceeded {
            detail: "binding-table request authority exhausted",
            span: selene_gql::SourceSpan::default(),
        });
        let outcome = RequestOutcome::failed(context, error);

        assert_eq!(outcome.error().unwrap().kind(), ErrorKind::Execution);
        assert_eq!(
            outcome.error().unwrap().gqlstatus().unwrap().as_str(),
            "5GQL1"
        );
        assert_eq!(outcome.diagnostics().primary().status().as_str(), "5GQL1");
        assert!(
            outcome
                .diagnostics()
                .primary()
                .message()
                .contains("binding-table request authority exhausted")
        );
        assert!(outcome.diagnostics().additional().is_empty());
        assert!(outcome.into_result().is_err());
    }
}
