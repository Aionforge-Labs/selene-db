//! Procedure-call planner IR rows.

use selene_core::DbString;

use crate::{
    ProcedureHandle, ProcedureMutability, ProcedureOutputSchema, ProcedureTier, SourceSpan,
};

use super::{BindingTableColumn, ProjectExpr};

/// Planned procedure call.
#[derive(Clone, Debug)]
pub struct PlannedCall {
    /// Runtime epoch at resolution; drift invalidates this call before dispatch.
    pub registry_version: u64,
    /// Complete resolved signature, effects, and durable declaration identity.
    pub metadata: crate::ProcedureMetadata,
    /// Whether an empty procedure result preserves the input row with null yields.
    pub optional: bool,
    /// Qualified procedure name.
    pub procedure: Box<[DbString]>,
    /// Opaque procedure handle.
    pub handle: ProcedureHandle,
    /// Planned call arguments.
    pub args: Vec<ProjectExpr>,
    /// Requested yield columns.
    pub yield_cols: Vec<PlannedYieldItem>,
    /// Procedure output schema.
    pub output_schema: ProcedureOutputSchema,
    /// Yielded binding-table schema appended by this CALL.
    pub yield_schema: Vec<BindingTableColumn>,
    /// Procedure execution tier.
    pub tier: ProcedureTier,
    /// Procedure mutability class.
    pub mutability: ProcedureMutability,
    /// Source span.
    pub span: SourceSpan,
}

/// Planned yield item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedYieldItem {
    /// Source output column selector.
    pub column: YieldKind,
    /// Output alias, when present.
    pub alias: Option<DbString>,
    /// Source span of this yield item.
    pub span: SourceSpan,
}

/// Planned yield column selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum YieldKind {
    /// `YIELD *`.
    Star,
    /// `YIELD col`.
    Named(DbString),
}
