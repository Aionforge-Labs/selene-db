//! Stable 2.x embedding facade for Selene DB.
//!
//! Applications should depend on this crate rather than assembling engine
//! layers. Lower crates remain available for advanced engine work, but they do
//! not carry this crate's 2.x stability promise unless a type is intentionally
//! re-exported here.
//!
//! The current facade owns one in-memory catalog with named schemas, graphs, and
//! closed graph types. A [`Session`] holds copied catalog/profile defaults,
//! optional embedder-provided authorization, a controlled typed parameter map,
//! and one active-request slot. [`RequestOutcome`] retains the immutable context
//! used by each explicit [`Request`]. M03-PR04 provides facade-owned detached
//! transaction state, serial multi-request visibility, and one outer in-memory
//! publication for implicit and explicit mutations. An
//! [`ErrorKind::MutationIndeterminate`] result means the complete mutation is
//! already visible and must not be retried blindly. F02-PR04 separately reports
//! [`DurableCommitOutcome`] for the private format-2 commit path; it does not
//! add public durable create/open (owned by F02-PR05).
//!
//! # Quickstart
//!
//! ```
//! use selene_db::{
//!     CreatePolicy, Database, ObjectPath, SchemaPath, WriteSummary,
//! };
//!
//! let database = Database::builder().build();
//! let catalog = database.catalog();
//! let schema = SchemaPath::regular("selene", "memory")?;
//! catalog.create_schema(&schema, CreatePolicy::Strict)?;
//! let graph_path = ObjectPath::regular("selene", "memory", "episodes")?;
//! catalog.create_graph(&graph_path, None, CreatePolicy::Strict)?;
//! let session = database.session(&graph_path)?;
//!
//! let write = session.execute("INSERT (:Person { name: 'Ada' })")?;
//! assert_eq!(write.write_summary(), Some(WriteSummary::new(1, None)));
//!
//! let rows = session.execute("MATCH (n:Person) RETURN n")?;
//! assert_eq!(rows.row_count(), Some(1));
//! # Ok::<(), selene_db::Error>(())
//! ```
//!
//! Removed graph handles and lower engine types are not facade exports:
//!
//! ```compile_fail
//! use selene_db::{GraphHandle, SharedGraph};
//! ```
//!
//! Physical row indices are not facade exports:
//!
//! ```compile_fail
//! use selene_db::RowIndex;
//! ```
//!
//! Lower mutation builders are not facade exports:
//!
//! ```compile_fail
//! use selene_db::Mutator;
//! ```
//!
//! Persistence writers are not facade exports:
//!
//! ```compile_fail
//! use selene_db::WalWriter;
//! ```
//!
//! Lower runtime graph and schema definitions are not facade exports:
//!
//! ```compile_fail
//! use selene_db::{CoreGraphTypeBridge, CoreProvider, GraphTypeDef, SeleneGraph};
//! ```
//!
//! Lower execution contexts and physical binding tables are not facade exports:
//!
//! ```compile_fail
//! use selene_db::{BindingTable, ExecutionContext, ExecutionStack};
//! ```
//!
//! The facade session has no borrowed graph lifetime:
//!
//! ```compile_fail
//! fn borrowed(_: selene_db::Session<'static>) {}
//! ```
//!
//! A session is movable between threads but is intentionally not shareable for
//! concurrent use:
//!
//! ```compile_fail
//! fn require_sync<T: Sync>() {}
//! require_sync::<selene_db::Session>();
//! ```
//!
//! Session context fields cannot be overwritten through the public API:
//!
//! ```compile_fail
//! fn overwrite(
//!     context: &mut selene_db::SessionContext,
//!     graph: selene_db::GraphDescriptor,
//! ) {
//!     context.current_graph = graph;
//! }
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod auth;
#[cfg(feature = "test-harness")]
#[doc(hidden)]
pub mod benchmark;
mod catalog;
mod catalog_snapshot;
mod catalog_stage;
mod config;
mod database;
mod ddl;
mod declarations;
mod diagnostic;
mod error;
mod graph_type;
mod handle;
mod outcome;
mod params;
mod path;
mod registration_stage;
mod request;
mod session;
mod session_context;
mod transaction;
mod value;

pub use auth::{
    AllowAllAuthorizationPolicy, AuthHookError, AuthorizationDecision, AuthorizationId,
    AuthorizationPolicy, AuthorizationRequest, NoPrincipalProvider, Principal, PrincipalId,
    PrincipalProvider, SessionOptions,
};
pub use catalog::{Catalog, CreateOutcome, CreatePolicy, DropOutcome, DropPolicy};
pub use catalog_snapshot::{
    CatalogGeneration, CatalogReadSnapshot, GraphDescriptor, GraphId, GraphTypeDescriptor,
    GraphTypeId, SchemaDescriptor, SchemaId,
};
pub use config::{DatabaseConfig, OpenMode};
pub use database::{Database, DatabaseBuilder};
pub use declarations::*;
pub use diagnostic::{DiagnosticBundle, GqlStatusObject};
pub use error::{
    DurableCommitOutcome, DurableCommitPhase, DurableCommitPosition, DurableCommitState, Error,
    ErrorKind, GqlStatus,
};
pub use graph_type::{GraphTypeBuilder, GraphTypeDefinition, NodeTypeDefinition};
pub use handle::{DatabaseId, EdgeRef, GraphGeneration, GraphRef, NodeRef};
pub use outcome::{
    DeclaredType, ExecutionOutcome, RegularResult, ResultDescriptor, ResultField, ResultRow,
    WriteSummary,
};
pub use params::{GeneralParameter, RequestParams};
pub use path::{CatalogPath, ObjectPath, PathSegment, SchemaPath};
pub use request::{Request, RequestContext, RequestOutcome, RequestTimestamp};
/// Stable lower graph edge identity intentionally exposed for facade references.
pub use selene_core::EdgeId;
/// Stable lower graph node identity intentionally exposed for facade references.
pub use selene_core::NodeId;
/// Stable, owned result-order metadata; it contains no executable AST or arena ID.
pub use selene_core::{NullPlacement, ResultOrderKey, SortDirection};
/// Owned normalized structural types supported by facade parameters and results.
/// These descriptors contain no AST spelling or process-local type identifier.
///
/// ```compile_fail
/// use selene_db::GqlType; // Source AST types are not facade parameter types.
/// ```
pub use selene_core::{ScalarType, StructuralType as Type, StructuralTypeError, TypeKind};
pub use session::Session;
pub use session_context::{
    ProfileIdentity, RequestSlotState, SessionContext, SessionDependencySummary, SessionParameters,
    SessionTerminationState, TimeZoneDisplacement, TransactionSlotState,
};
pub use transaction::{Transaction, TransactionAccessMode, TransactionId, TransactionState};
/// Owned query values with database- and graph-scoped reference carriers.
pub use value::{Path, PathSegment as ValuePathSegment, Record, Value};

/// Result type returned by facade operations.
pub type Result<T> = std::result::Result<T, Error>;
