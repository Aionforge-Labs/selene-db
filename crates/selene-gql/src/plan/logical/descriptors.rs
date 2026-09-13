//! Logical descriptors built from semantic identities.
//!
//! Every descriptor names semantic bindings, expression identities, types, and
//! source origins without parser syntax nodes, physical row coordinates,
//! storage positions, or execution policy. Physical lowering transports these
//! decisions into batch operators; it never rederives them.

use crate::{
    NullsPolicy, OrderDirection, SourceSpan,
    analyze::{AnalyzedType, BindingId, ExprId, ScopeId},
};

use super::effect::LogicalEffect;

/// Graph-access descriptor built from a semantic binding declaration.
///
/// This names the binding-table input source without parser nodes or physical
/// coordinates. The analyzer's `BindingId`,
/// declaration type, and source origin identify the access; storage positions
/// and runtime addresses never appear here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalScanDescriptor {
    /// Semantic binding produced by this scan.
    pub binding: BindingId,
    /// Lexical scope that declares the binding.
    pub scope: ScopeId,
    /// Analyzer-inferred binding type.
    pub ty: AnalyzedType,
    /// True for node access, false for edge access.
    pub is_node: bool,
    /// Source origin of the declaring pattern.
    pub origin: SourceSpan,
}

/// Ordinary mutation intent staged through the existing detached transaction.
///
/// Mutations describe intent only. Execution stages changes through the
/// existing detached transaction state and the single publication funnel; no
/// independent publication path exists at this layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalMutationDescriptor {
    /// Conservative count of analyzer write-set entries for this statement.
    pub write_entry_count: usize,
    /// True when at least one entry inserts a node.
    pub inserts_node: bool,
    /// True when at least one entry inserts an edge.
    pub inserts_edge: bool,
    /// True when at least one entry sets or removes labels/properties.
    pub updates_graph: bool,
    /// True when at least one entry deletes a target.
    pub deletes_target: bool,
    /// Source origin of the mutation pipeline.
    pub origin: SourceSpan,
}

/// Named-procedure reference resolved from registration metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalCallDescriptor {
    /// Dotted procedure name segments for stable diagnostics.
    pub name: Vec<String>,
    /// Effect resolved from registration metadata at plan time.
    pub effect: LogicalEffect,
    /// Number of evaluated arguments (explicit plus synthesized defaults).
    pub argument_count: usize,
    /// Number of yielded output columns.
    pub yield_count: usize,
    /// Source origin of the call.
    pub origin: SourceSpan,
}

/// One aggregate function application resolved from semantic identities.
///
/// The function name is the analyzer-resolved aggregate name (for example,
/// `count`); argument identities are semantic expression ids. The output
/// column name is the deterministic `agg_<id>` synthesized during lowering,
/// matching the row-plan aggregate rewrite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalAggregate {
    /// Semantic expression identity of the aggregate call itself.
    pub aggregate_id: ExprId,
    /// Lower-cased aggregate function name.
    pub function: selene_core::DbString,
    /// `COUNT(*)` star spelling.
    pub star: bool,
    /// `DISTINCT` duplicate elimination.
    pub distinct: bool,
    /// Semantic identities of the aggregate arguments in source order.
    pub args: Vec<ExprId>,
    /// Deterministic output column name (`agg_<id>`).
    pub output_name: selene_core::DbString,
    /// Analyzer-inferred aggregate result type.
    pub ty: AnalyzedType,
    /// Source origin of the aggregate call.
    pub origin: SourceSpan,
}

/// One ordering key resolved from semantic identities.
///
/// The sorted expression is a semantic identity; direction and null policy
/// are the source-declared ordering contract. No runtime sort algorithm or
/// access path appears here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalOrderKey {
    /// Semantic expression identity of the sort key.
    pub expr: ExprId,
    /// Declared sort direction.
    pub direction: OrderDirection,
    /// Declared null-order policy, when present.
    pub nulls: Option<NullsPolicy>,
    /// Analyzer-inferred key type.
    pub ty: AnalyzedType,
    /// Semantic binding references carried by the key.
    pub binding_refs: Vec<BindingId>,
    /// Source origin of the sort term.
    pub origin: SourceSpan,
}

/// Catalog statement family resolved from the analyzer category and source shape.
///
/// The payload carried here is intentionally coarse: the statement kind plus
/// its source origin and effect. Unchanged DDL payloads (labels, property
/// definitions, endpoints) stay in source syntax and are transported by the
/// physical lowerer; no semantic decision is rederived there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogicalCatalogKind {
    /// Database-catalog command (`CREATE/DROP SCHEMA/GRAPH/GRAPH TYPE`).
    DatabaseCatalog,
    /// `CREATE NODE TYPE`.
    CreateNodeType,
    /// `CREATE EDGE TYPE`.
    CreateEdgeType,
    /// `ALTER NODE TYPE`.
    AlterNodeType,
    /// `ALTER EDGE TYPE`.
    AlterEdgeType,
    /// `DROP NODE TYPE`.
    DropNodeType,
    /// `DROP EDGE TYPE`.
    DropEdgeType,
    /// `TRUNCATE` (data write that keeps the type).
    Truncate,
    /// `CREATE INDEX`.
    CreateIndex,
    /// `DROP INDEX`.
    DropIndex,
    /// Read-only introspection (`SHOW ...`).
    Show,
}

impl LogicalCatalogKind {
    /// Return this catalog family's logical effect.
    ///
    /// `SHOW` is read-only introspection; `TRUNCATE` removes data instances
    /// while keeping the type (a data write); every other DDL family is a
    /// catalog write.
    #[must_use]
    pub const fn effect(self) -> LogicalEffect {
        match self {
            Self::Show => LogicalEffect::Query,
            Self::Truncate => LogicalEffect::Data,
            Self::DatabaseCatalog
            | Self::CreateNodeType
            | Self::CreateEdgeType
            | Self::AlterNodeType
            | Self::AlterEdgeType
            | Self::DropNodeType
            | Self::DropEdgeType
            | Self::CreateIndex
            | Self::DropIndex => LogicalEffect::Catalog,
        }
    }
}

/// Transaction/session control family resolved from the statement category.
///
/// Carries only the control kind plus its source origin. Session parameter
/// values and catalog targets stay in source syntax for the adapter to
/// transport; the logical effect (session vs transaction) is fixed here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogicalControlKind {
    /// `START TRANSACTION`.
    StartTransaction,
    /// `COMMIT`.
    Commit,
    /// `ROLLBACK`.
    Rollback,
    /// `SESSION SET VALUE`.
    SessionSetValue,
    /// `SESSION SET TIME ZONE`.
    SessionSetTimeZone,
    /// `SESSION SET GRAPH` / `SESSION SET SCHEMA`.
    SessionSetGraph,
    /// `SESSION RESET ...`.
    SessionReset,
    /// `SESSION CLOSE`.
    SessionClose,
}

impl LogicalControlKind {
    /// Return this control family's logical effect.
    #[must_use]
    pub const fn effect(self) -> LogicalEffect {
        match self {
            Self::StartTransaction | Self::Commit | Self::Rollback => LogicalEffect::Transaction,
            Self::SessionSetValue
            | Self::SessionSetTimeZone
            | Self::SessionSetGraph
            | Self::SessionReset
            | Self::SessionClose => LogicalEffect::Session,
        }
    }
}
