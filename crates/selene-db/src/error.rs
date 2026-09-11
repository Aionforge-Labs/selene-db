//! Facade-owned diagnostics.

use std::{error::Error as StdError, fmt};

mod durable;
pub use durable::*;

/// Stable category for a facade failure.
///
/// Catalog categories carry a fixed GQLSTATUS (see [`ErrorKind::gqlstatus`])
/// so Rust and GQL callers observe the same diagnostic for the same failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A logical catalog path segment is invalid (`42001`: the name fails the
    /// selected identifier profile).
    InvalidCatalogName,
    /// The requested catalog object, or a parent it needs, does not exist, or
    /// the reference has a shape the catalog cannot hold (`42002`, ISO/IEC
    /// 39075:2024 Table 8 "invalid reference"; §23.2 GR2d).
    CatalogObjectNotFound,
    /// Strict creation found an existing object of the requested kind
    /// (`42N10`, a selene-db subclass; ISO delegates the condition to the
    /// implementation under IE005).
    CatalogObjectAlreadyExists,
    /// A shared-namespace object exists with another kind (`42002`: §17.2
    /// SR2d(i)(1) makes the wrong kind a reference syntax-rule violation).
    CatalogObjectWrongKind,
    /// RESTRICT rejected a lifecycle mutation with live dependents or contents
    /// (`G1000`, the class-level "dependent object error"; not `G1001`, whose
    /// subclass names edges only).
    CatalogRestrictViolation,
    /// A catalog object reference crosses a prohibited ownership boundary
    /// (`42002`).
    CatalogReferenceViolation,
    /// A facade runtime handle has wrong ownership or no longer names a live
    /// graph element (`42002`).
    RuntimeInvalidReference,
    /// A session's selected stable graph identity is no longer registered.
    StaleGraphSelection,
    /// An authorization identifier is empty or exceeds the database-string limit.
    InvalidAuthorizationId,
    /// A principal identifier is empty or exceeds the database-string limit.
    InvalidPrincipalId,
    /// An explicit authorization identifier resolved to no principal.
    PrincipalNotFound,
    /// The embedder principal provider returned a failure.
    PrincipalProviderFailure,
    /// The embedder authorization policy denied session creation.
    AuthorizationDenied,
    /// The embedder authorization policy returned a failure.
    AuthorizationPolicyFailure,
    /// A principal's declared home paths are absent, wrong-kind, or incoherent.
    InvalidPrincipalHome,
    /// Session initialization produced an invalid current catalog reference.
    InvalidSessionReference,
    /// A copied home or current session reference is no longer current.
    StaleSessionReference,
    /// A facade graph-type definition is inconsistent.
    InvalidGraphType,
    /// Catalog identity or immutable-state validation failed internally.
    CatalogInvariant,
    /// A facade mutation was canceled before publication and is not visible.
    MutationCanceled,
    /// A facade mutation was published, but acknowledgement failed. The
    /// complete mutation may be visible and must not be retried blindly.
    MutationIndeterminate,
    /// Durable commit was proved absent, including synchronized rollback (`40N01`).
    DurableCommitCanceled,
    /// Durability is uncertain, without implying live publication (`40003`).
    DurableCommitUncertain,
    /// The complete record synchronized but acknowledgment failed (`40003`).
    DurableCommitUnacknowledged,
    /// A second transaction was requested while one was active (`25G01`).
    ActiveTransaction,
    /// Catalog and data modification classes were mixed (`25G02`).
    TransactionMixing,
    /// A write was attempted in a read-only transaction (`25G03`).
    ReadOnlyTransaction,
    /// A non-control statement or commit followed statement failure (`25N02`).
    FailedTransaction,
    /// COMMIT or ROLLBACK had no active transaction (`2D000`).
    NoActiveTransaction,
    /// Optimistic commit found that its pinned base was stale (`40000`).
    TransactionRollback,
    /// The monotonic transaction-ID domain is exhausted (`5GQL1`).
    TransactionIdExhausted,
    /// The source is not valid GQL.
    InvalidGql,
    /// A decoded request/session parameter name does not match GQL `$name` spelling.
    InvalidParameterName,
    /// A request parameter dictionary contains the same exact name twice.
    DuplicateParameter,
    /// A request was started while this single-session slot was occupied.
    RequestAlreadyActive,
    /// A request was issued after `SESSION CLOSE` (`2DN01`).
    SessionClosed,
    /// The requested GQL feature is not supported by this facade mode.
    FeatureNotSupported,
    /// Parsing, analysis, planning, or execution failed for another reason.
    Execution,
}

/// Facade-owned five-character GQLSTATUS code.
///
/// This value copies the semantic code emitted by the GQL engine without
/// re-exporting the lower crate's status type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GqlStatus([u8; 5]);

impl GqlStatus {
    /// Successful completion with a regular result.
    pub const SUCCESSFUL_COMPLETION: Self = Self(*b"00000");
    /// Completion condition of a successful catalog statement with an omitted
    /// result (ISO/IEC 39075:2024 §4.9.3).
    pub const SUCCESSFUL_COMPLETION_OMITTED_RESULT: Self = Self(*b"00001");
    /// Completion condition of `DROP GRAPH IF EXISTS` on an absent graph
    /// (§12.5 GR1).
    pub const GRAPH_DOES_NOT_EXIST: Self = Self(*b"01G03");
    /// Successful completion with no data.
    pub const NO_DATA: Self = Self(*b"02000");
    /// Runtime warning for aggregate NULL elimination.
    pub const NULL_VALUE_ELIMINATED_IN_SET_FUNCTION: Self = Self(*b"01G11");
    /// Runtime warning for a relaxed validation-mode write.
    pub const VALIDATION_MODE_RELAXED_WRITE: Self = Self(*b"01N01");
    /// Statement completion is unknown after an irreversible publication.
    pub const STATEMENT_COMPLETION_UNKNOWN: Self = Self(*b"40003");
    /// Optimistic transaction conflict rolled the transaction back.
    pub const TRANSACTION_ROLLBACK: Self = Self(*b"40000");
    /// Local durable commit prevented and the whole transaction canceled (IE008).
    pub const DURABLE_COMMIT_ROLLBACK: Self = Self(*b"40N01");
    /// A transaction is already active.
    pub const ACTIVE_TRANSACTION: Self = Self(*b"25G01");
    /// Catalog/data transaction mixing is unsupported.
    pub const INVALID_TRANSACTION_MIXING: Self = Self(*b"25G02");
    /// A write was attempted in a read-only transaction.
    pub const READ_ONLY_TRANSACTION: Self = Self(*b"25G03");
    /// The transaction is failed until demarcation.
    pub const IN_FAILED_TRANSACTION: Self = Self(*b"25N02");
    /// A transaction termination command had no active transaction.
    pub const INVALID_TRANSACTION_TERMINATION: Self = Self(*b"2D000");
    /// A request was issued against a closed session.
    pub const SESSION_CLOSED: Self = Self(*b"2DN01");
    /// An implementation-defined program limit was exhausted.
    pub const PROGRAM_LIMIT_EXCEEDED: Self = Self(*b"5GQL1");
    /// Implementation-defined runtime failure fallback.
    pub const IMPLEMENTATION_DEFINED_ERROR: Self = Self(*b"5GQL0");
    /// "Invalid syntax", used when a catalog name fails the identifier profile.
    pub const SYNTAX_ERROR: Self = Self(*b"42001");
    /// "Invalid reference", used for missing, wrong-kind, and unshapeable
    /// catalog references.
    pub const INVALID_REFERENCE: Self = Self(*b"42002");
    /// GQLSTATUS used when a facade mode does not support a GQL feature.
    pub const FEATURE_NOT_SUPPORTED: Self = Self(*b"42N01");
    /// GQLSTATUS used when a facade mutation is canceled before publication.
    pub const OPERATION_CANCELLED: Self = Self(*b"5GQL2");
    /// Duplicate catalog object under strict creation (selene-db subclass).
    pub const DUPLICATE_OBJECT: Self = Self(*b"42N10");
    /// Class-level "dependent object error", used for RESTRICT violations.
    pub const DEPENDENT_OBJECT_ERROR: Self = Self(*b"G1000");
    /// GQLSTATUS used when a closed graph violates its bound graph type.
    pub const GRAPH_TYPE_VIOLATION: Self = Self(*b"G2000");

    pub(crate) fn from_engine(status: selene_gql::GqlStatus) -> Self {
        let mut code = [0_u8; 5];
        code.copy_from_slice(status.as_str().as_bytes());
        Self(code)
    }

    /// Return the five-character code.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("5GQL0")
    }
}

impl fmt::Display for GqlStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Failure returned by a facade operation.
///
/// Public accessors return owned facade data. [`StdError::source`] retains the
/// concrete engine diagnostic behind a trait object for logs and error chains;
/// no lower error type appears in the facade signature.
#[derive(Debug)]
pub struct Error {
    durable: Option<Box<DurableCommitOutcome>>,
    kind: ErrorKind,
    status: Option<GqlStatus>,
    message: String,
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl ErrorKind {
    /// Return the GQLSTATUS the facade assigns to this category, if any.
    ///
    /// Engine-originated categories take their status from the engine
    /// diagnostic instead; session/authorization-hook, internal invariant,
    /// stale-selection, and graph-type definition failures have none.
    #[must_use]
    pub const fn gqlstatus(self) -> Option<GqlStatus> {
        match self {
            Self::InvalidCatalogName | Self::InvalidParameterName => Some(GqlStatus::SYNTAX_ERROR),
            Self::CatalogObjectNotFound
            | Self::CatalogObjectWrongKind
            | Self::CatalogReferenceViolation
            | Self::RuntimeInvalidReference => Some(GqlStatus::INVALID_REFERENCE),
            Self::CatalogObjectAlreadyExists => Some(GqlStatus::DUPLICATE_OBJECT),
            Self::CatalogRestrictViolation => Some(GqlStatus::DEPENDENT_OBJECT_ERROR),
            Self::MutationCanceled => Some(GqlStatus::OPERATION_CANCELLED),
            Self::MutationIndeterminate => Some(GqlStatus::STATEMENT_COMPLETION_UNKNOWN),
            Self::DurableCommitCanceled => Some(GqlStatus::DURABLE_COMMIT_ROLLBACK),
            Self::DurableCommitUncertain | Self::DurableCommitUnacknowledged => {
                Some(GqlStatus::STATEMENT_COMPLETION_UNKNOWN)
            }
            Self::ActiveTransaction => Some(GqlStatus::ACTIVE_TRANSACTION),
            Self::TransactionMixing => Some(GqlStatus::INVALID_TRANSACTION_MIXING),
            Self::ReadOnlyTransaction => Some(GqlStatus::READ_ONLY_TRANSACTION),
            Self::FailedTransaction => Some(GqlStatus::IN_FAILED_TRANSACTION),
            Self::NoActiveTransaction => Some(GqlStatus::INVALID_TRANSACTION_TERMINATION),
            Self::SessionClosed => Some(GqlStatus::SESSION_CLOSED),
            Self::TransactionRollback => Some(GqlStatus::TRANSACTION_ROLLBACK),
            Self::TransactionIdExhausted => Some(GqlStatus::PROGRAM_LIMIT_EXCEEDED),
            Self::StaleGraphSelection
            | Self::InvalidAuthorizationId
            | Self::InvalidPrincipalId
            | Self::PrincipalNotFound
            | Self::PrincipalProviderFailure
            | Self::AuthorizationDenied
            | Self::AuthorizationPolicyFailure
            | Self::InvalidPrincipalHome
            | Self::InvalidSessionReference
            | Self::StaleSessionReference
            | Self::InvalidGraphType
            | Self::CatalogInvariant
            | Self::InvalidGql
            | Self::DuplicateParameter
            | Self::RequestAlreadyActive
            | Self::FeatureNotSupported
            | Self::Execution => None,
        }
    }
}

impl Error {
    fn facade(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: kind.gqlstatus(),
            message: message.into(),
            source: None,
            durable: None,
        }
    }

    fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            status: kind.gqlstatus(),
            message: message.into(),
            source: Some(Box::new(source)),
            durable: None,
        }
    }

    pub(crate) fn from_engine(source: selene_gql::ExecutorError) -> Self {
        let status = GqlStatus::from_engine(source.gqlstatus());
        let kind = match status.as_str() {
            "42001" => ErrorKind::InvalidGql,
            "42N01" => ErrorKind::FeatureNotSupported,
            "40003" => ErrorKind::MutationIndeterminate,
            "5GQL2" => ErrorKind::MutationCanceled,
            _ => ErrorKind::Execution,
        };
        Self {
            kind,
            status: Some(status),
            message: source.to_string(),
            source: Some(Box::new(source)),
            durable: None,
        }
    }

    pub(crate) fn unsupported_engine_outcome() -> Self {
        Self {
            kind: ErrorKind::Execution,
            status: None,
            message: "the GQL engine returned an outcome unknown to this facade".to_owned(),
            source: None,
            durable: None,
        }
    }

    pub(crate) fn from_catalog_name(source: selene_catalog::CatalogError) -> Self {
        Self::with_source(ErrorKind::InvalidCatalogName, source.to_string(), source)
    }

    pub(crate) fn from_catalog_invariant(source: selene_catalog::CatalogError) -> Self {
        use selene_catalog::CatalogError;
        let kind = match &source {
            CatalogError::DuplicateCanonicalName { .. } => ErrorKind::CatalogObjectAlreadyExists,
            CatalogError::InvalidDependency {
                reason: "missing_dependency",
                ..
            } => ErrorKind::CatalogRestrictViolation,
            CatalogError::InvalidDependency { .. } => ErrorKind::CatalogReferenceViolation,
            CatalogError::InvalidDeclaration { reason }
                if reason.starts_with("unsupported_")
                    || *reason == "caller_asserted_activation"
                    || *reason == "active_declaration_requires_runtime_mutation" =>
            {
                ErrorKind::FeatureNotSupported
            }
            CatalogError::InvalidDeclaration { .. } => ErrorKind::CatalogReferenceViolation,
            _ => ErrorKind::CatalogInvariant,
        };
        let mut error = Self::with_source(kind, source.to_string(), source);
        if kind == ErrorKind::FeatureNotSupported {
            error.status = Some(GqlStatus::FEATURE_NOT_SUPPORTED);
        }
        error
    }

    pub(crate) fn invalid_graph_type_source(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::with_source(ErrorKind::InvalidGraphType, source.to_string(), source)
    }

    pub(crate) fn invalid_graph_type(message: impl Into<String>) -> Self {
        Self::facade(ErrorKind::InvalidGraphType, message)
    }

    pub(crate) fn not_found(path: &impl fmt::Display, kind: &str) -> Self {
        Self::facade(
            ErrorKind::CatalogObjectNotFound,
            format!("{kind} {path} does not exist"),
        )
    }

    /// A GQL reference that no facade path can represent.
    pub(crate) fn invalid_reference(reference: &str, detail: &str) -> Self {
        Self::facade(
            ErrorKind::CatalogObjectNotFound,
            format!("invalid catalog reference {reference}: {detail}"),
        )
    }

    /// `OR REPLACE` requested for an object kind whose replacement is not
    /// implemented. Carries `42N01` like every other feature rejection.
    pub(crate) fn unsupported_create_policy(kind: &str) -> Self {
        Self {
            kind: ErrorKind::FeatureNotSupported,
            status: Some(GqlStatus::FEATURE_NOT_SUPPORTED),
            message: format!("OR REPLACE is not supported for {kind} creation"),
            source: None,
            durable: None,
        }
    }

    pub(crate) fn already_exists(path: &impl fmt::Display) -> Self {
        Self::facade(
            ErrorKind::CatalogObjectAlreadyExists,
            format!("catalog object {path} already exists"),
        )
    }

    pub(crate) fn wrong_kind(
        path: &impl fmt::Display,
        expected: &str,
        actual: selene_catalog::CatalogObjectKind,
    ) -> Self {
        Self::facade(
            ErrorKind::CatalogObjectWrongKind,
            format!("catalog path {path} is a {actual}, not a {expected}"),
        )
    }

    pub(crate) fn empty_identity(kind: &'static str) -> Self {
        let error_kind = match kind {
            "authorization" => ErrorKind::InvalidAuthorizationId,
            "principal" => ErrorKind::InvalidPrincipalId,
            _ => ErrorKind::CatalogInvariant,
        };
        Self::facade(error_kind, format!("{kind} identifier must not be empty"))
    }

    pub(crate) fn identity_too_long(kind: &'static str) -> Self {
        let error_kind = match kind {
            "authorization" => ErrorKind::InvalidAuthorizationId,
            "principal" => ErrorKind::InvalidPrincipalId,
            _ => ErrorKind::CatalogInvariant,
        };
        Self::facade(
            error_kind,
            format!("{kind} identifier exceeds the database-string limit"),
        )
    }

    pub(crate) fn invalid_authorization_id_source(source: selene_core::CoreError) -> Self {
        Self::with_source(
            ErrorKind::InvalidAuthorizationId,
            "authorization identifier is invalid",
            source,
        )
    }

    pub(crate) fn invalid_principal_id_source(source: selene_core::CoreError) -> Self {
        Self::with_source(
            ErrorKind::InvalidPrincipalId,
            "principal identifier is invalid",
            source,
        )
    }

    pub(crate) fn principal_not_found() -> Self {
        Self::facade(
            ErrorKind::PrincipalNotFound,
            "the authorization identifier has no resolved principal",
        )
    }

    pub(crate) fn principal_provider_failure(source: crate::AuthHookError) -> Self {
        Self::with_source(
            ErrorKind::PrincipalProviderFailure,
            "principal resolution failed",
            source,
        )
    }

    pub(crate) fn authorization_denied() -> Self {
        Self::facade(
            ErrorKind::AuthorizationDenied,
            "authorization policy denied session creation",
        )
    }

    pub(crate) fn authorization_policy_failure(source: crate::AuthHookError) -> Self {
        Self::with_source(
            ErrorKind::AuthorizationPolicyFailure,
            "authorization policy evaluation failed",
            source,
        )
    }

    pub(crate) fn invalid_principal_home(message: &'static str) -> Self {
        Self::facade(ErrorKind::InvalidPrincipalHome, message)
    }

    pub(crate) fn invalid_principal_home_source(message: &'static str, source: Error) -> Self {
        Self::with_source(ErrorKind::InvalidPrincipalHome, message, source)
    }

    pub(crate) fn invalid_session_reference(source: Error) -> Self {
        Self::with_source(
            ErrorKind::InvalidSessionReference,
            "the selected session reference is invalid",
            source,
        )
    }

    pub(crate) fn invalid_runtime_reference(detail: &str) -> Self {
        Self::facade(
            ErrorKind::RuntimeInvalidReference,
            format!("invalid runtime reference: {detail}"),
        )
    }

    pub(crate) fn stale_session_reference() -> Self {
        Self::facade(
            ErrorKind::StaleSessionReference,
            "a session catalog reference is stale or invalidated",
        )
    }

    pub(crate) fn invalid_parameter_name(name: &str) -> Self {
        Self::facade(
            ErrorKind::InvalidParameterName,
            format!("invalid parameter name {name:?}: expected decoded GQL $name spelling"),
        )
    }

    pub(crate) fn invalid_parameter_name_source(
        name: &str,
        source: selene_core::CoreError,
    ) -> Self {
        Self::with_source(
            ErrorKind::InvalidParameterName,
            format!("invalid parameter name {name:?}"),
            source,
        )
    }

    pub(crate) fn duplicate_parameter(name: &str) -> Self {
        Self::facade(
            ErrorKind::DuplicateParameter,
            format!("duplicate request parameter name {name:?}"),
        )
    }

    pub(crate) fn request_already_active() -> Self {
        Self::facade(
            ErrorKind::RequestAlreadyActive,
            "a request is already active on this session",
        )
    }

    pub(crate) fn session_closed() -> Self {
        Self::facade(
            ErrorKind::SessionClosed,
            "the session is closed; open a new session",
        )
    }

    pub(crate) fn nonempty_graph(path: &impl fmt::Display, nodes: usize, edges: usize) -> Self {
        Self::facade(
            ErrorKind::CatalogRestrictViolation,
            format!(
                "cannot drop nonempty graph {path} under RESTRICT: {nodes} nodes, {edges} edges"
            ),
        )
    }

    pub(crate) fn nonempty_schema(path: &impl fmt::Display, children: usize) -> Self {
        Self::facade(
            ErrorKind::CatalogRestrictViolation,
            format!("cannot drop nonempty schema {path} under RESTRICT: {children} objects"),
        )
    }

    pub(crate) fn referenced_graph_type(path: &impl fmt::Display, references: usize) -> Self {
        Self::facade(
            ErrorKind::CatalogRestrictViolation,
            format!(
                "cannot drop graph type {path} under RESTRICT: referenced by {references} graphs"
            ),
        )
    }

    pub(crate) fn cross_schema_graph_type(path: &impl fmt::Display) -> Self {
        Self::facade(
            ErrorKind::CatalogReferenceViolation,
            format!("graph type {path} is not in the graph's schema"),
        )
    }

    pub(crate) fn catalog_invariant(message: impl Into<String>) -> Self {
        Self::facade(ErrorKind::CatalogInvariant, message)
    }

    pub(crate) fn mutation_canceled() -> Self {
        Self::facade(
            ErrorKind::MutationCanceled,
            "database mutation was canceled before publication and is not visible",
        )
    }

    pub(crate) fn mutation_indeterminate() -> Self {
        Self::facade(
            ErrorKind::MutationIndeterminate,
            "database mutation was published but acknowledgement failed; the complete state is visible and the mutation must not be retried blindly",
        )
    }

    pub(crate) fn active_transaction() -> Self {
        Self::facade(
            ErrorKind::ActiveTransaction,
            "a GQL transaction is already active on this session",
        )
    }

    pub(crate) fn transaction_mixing() -> Self {
        Self::facade(
            ErrorKind::TransactionMixing,
            "catalog and data modifications cannot be mixed in one transaction",
        )
    }

    pub(crate) fn read_only_transaction() -> Self {
        Self::facade(
            ErrorKind::ReadOnlyTransaction,
            "a modifying statement is not allowed in a read-only transaction",
        )
    }

    pub(crate) fn in_failed_transaction() -> Self {
        Self::facade(
            ErrorKind::FailedTransaction,
            "the transaction is failed; ROLLBACK is required",
        )
    }

    pub(crate) fn no_active_transaction() -> Self {
        Self::facade(
            ErrorKind::NoActiveTransaction,
            "COMMIT or ROLLBACK requires an active transaction",
        )
    }

    pub(crate) fn transaction_rollback() -> Self {
        Self::facade(
            ErrorKind::TransactionRollback,
            "the transaction base changed and the detached work was rolled back",
        )
    }

    pub(crate) fn transaction_id_exhausted() -> Self {
        Self::facade(
            ErrorKind::TransactionIdExhausted,
            "the monotonic transaction ID domain is exhausted",
        )
    }

    pub(crate) fn invalid_transaction_transition() -> Self {
        Self::catalog_invariant("invalid facade transaction state transition")
    }

    pub(crate) fn selected_maintenance_unsupported() -> Self {
        Self {
            kind: ErrorKind::FeatureNotSupported,
            status: Some(GqlStatus::FEATURE_NOT_SUPPORTED),
            message: "selected maintenance is outside the deferred detached-maintenance boundary"
                .to_owned(),
            source: None,
            durable: None,
        }
    }

    pub(crate) fn identifier_exhausted(kind: &str) -> Self {
        Self::facade(
            ErrorKind::CatalogInvariant,
            format!("{kind} ID high-water mark is exhausted"),
        )
    }

    #[cfg(test)]
    pub(crate) fn injected_failure(stage: &str) -> Self {
        Self::facade(
            ErrorKind::CatalogInvariant,
            format!("test failure injected {stage}"),
        )
    }

    /// Return the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Return the GQLSTATUS code of this failure.
    ///
    /// Catalog failures carry the code selected by their [`ErrorKind`] whether
    /// the request came from Rust or GQL; engine failures copy the engine's
    /// code; session/authorization-hook, internal invariant, stale-selection,
    /// and graph-type definition failures have none. Indeterminate mutation
    /// failures carry `40003` and mean the complete state may already be visible.
    #[must_use]
    pub const fn gqlstatus(&self) -> Option<GqlStatus> {
        self.status
    }

    /// Return the owned diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}
