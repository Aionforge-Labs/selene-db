//! Logical binding-table operators built from semantic descriptors.
//!
//! Every operator carries an explicit output schema, its logical effect, its
//! invalidation dependencies, and the semantic identities it was built from.
//! Operators never carry parser syntax nodes, physical row coordinates,
//! storage positions, or execution policy (access paths, join order, batch
//! sizes, parallelism). Ordering, multiplicity, variable scope, and type
//! metadata are part of each operator contract so physical batches (F04),
//! path semantic nodes (F05-PR01), and native adapters can consume this layer
//! without re-deriving semantics.

use crate::{
    SetOp, SourceSpan,
    analyze::{BindingId, ExprId, ScopeId},
    plan::{BindingTableColumn, BindingTableSchema, LimitAmount},
};

use super::{
    descriptors::{
        LogicalAggregate, LogicalCallDescriptor, LogicalCatalogKind, LogicalControlKind,
        LogicalMutationDescriptor, LogicalOrderKey, LogicalScanDescriptor,
    },
    effect::{EffectSummary, LogicalEffect},
    path::lowering::LoweredPathSet,
};

/// Row-order contract for one logical operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogicalOrdering {
    /// Operator preserves its input row order.
    Preserved,
    /// Operator defines a new row order (for example, an explicit sort).
    /// Page operators in this slice never reorder; they only skip/take.
    Defined,
    /// Operator makes no order promise; input order flows through unchanged
    /// but must not be relied upon.
    Unordered,
}

/// Duplicate-row contract for one logical operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogicalMultiplicity {
    /// Operator preserves duplicates exactly (binding tables may contain
    /// duplicates per ISO/IEC 39075:2024 §4.3.6).
    PreservesDuplicates,
    /// Operator removes duplicate rows.
    Distinct,
}

/// One logical binding-table operator.
///
/// Every supported statement/expression family lowers through these operators
/// from semantic descriptors. Source syntax supplies only statement ordering
/// and source spans; every identity, type, and effect comes from the frozen
/// semantic tree. Operators never carry parser syntax nodes, physical row
/// coordinates, storage positions, or execution policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalOp {
    /// Graph access producing its declared binding column.
    Scan {
        /// Semantic access descriptor.
        descriptor: LogicalScanDescriptor,
        /// Explicit output schema after this operator.
        output_schema: BindingTableSchema,
        /// Lexical scope visible after this operator.
        scope: ScopeId,
        /// Source origin.
        origin: SourceSpan,
    },
    /// Retain rows satisfying one semantic predicate.
    Filter {
        /// Semantic expression identity of the predicate.
        predicate: crate::analyze::ExprId,
        /// Lexical scope used to resolve the predicate.
        scope: ScopeId,
        /// Explicit output schema (identical to the input schema).
        output_schema: BindingTableSchema,
        /// Source origin.
        origin: SourceSpan,
    },
    /// Project semantic expressions into output columns.
    Project {
        /// Semantic expression identities in projection order.
        expressions: Vec<crate::analyze::ExprId>,
        /// Lexical scope that declares the projection aliases.
        scope: ScopeId,
        /// Explicit output schema.
        output_schema: BindingTableSchema,
        /// Source origin.
        origin: SourceSpan,
    },
    /// Skip/take rows without reordering or deduplication.
    Page {
        /// Rows to skip.
        offset: LogicalPageAmount,
        /// Rows to retain after the offset.
        count: LogicalPageAmount,
        /// Explicit output schema (identical to the input schema).
        output_schema: BindingTableSchema,
        /// Source origin.
        origin: SourceSpan,
    },
    /// One ordinary mutation path (insert/update/delete intent).
    Mutate {
        /// Conservative mutation descriptor.
        descriptor: LogicalMutationDescriptor,
        /// Explicit output schema after the mutation boundary.
        output_schema: BindingTableSchema,
        /// Lexical scope visible after the mutation.
        scope: ScopeId,
        /// Source origin.
        origin: SourceSpan,
    },
    /// Named-procedure call with metadata-resolved effects.
    Call {
        /// Procedure descriptor from registration metadata.
        descriptor: LogicalCallDescriptor,
        /// Explicit output schema (input columns plus yielded columns).
        output_schema: BindingTableSchema,
        /// Lexical scope visible after the call.
        scope: ScopeId,
        /// Source origin.
        origin: SourceSpan,
    },
    /// Extend the row with new aliases without dropping prior columns (`LET`).
    Extend {
        /// Semantic expression identities in binding order.
        expressions: Vec<ExprId>,
        /// Lexical scope that declares the new aliases.
        scope: ScopeId,
        /// Explicit output schema (input columns plus new aliases).
        output_schema: BindingTableSchema,
        /// Source origin.
        origin: SourceSpan,
    },
    /// Expand one list expression into one row per element (`FOR`/`UNWIND`).
    Unwind {
        /// Semantic identity of the source list expression.
        source: ExprId,
        /// Semantic binding produced for each element.
        alias: BindingId,
        /// Semantic binding for the position output, when present.
        position_alias: Option<BindingId>,
        /// Explicit output schema after expansion.
        output_schema: BindingTableSchema,
        /// Lexical scope visible after expansion.
        scope: ScopeId,
        /// Source origin.
        origin: SourceSpan,
    },
    /// Graph-pattern step carrying one `MATCH` clause's semantic coverage.
    ///
    /// The full element tests live in the plan-level [`LoweredPathSet`];
    /// this operator records the clause boundary, its optionality, and the
    /// schema it contributes so joins preserve ordering without rederiving
    /// pattern semantics.
    Match {
        /// True for `OPTIONAL MATCH` (left-outer); false for inner.
        optional: bool,
        /// Explicit output schema after this pattern step.
        output_schema: BindingTableSchema,
        /// Lexical scope visible after this pattern step.
        scope: ScopeId,
        /// Source origin of the clause.
        origin: SourceSpan,
    },
    /// Binary join between two pattern fragments on shared bindings.
    Join {
        /// Shared semantic bindings used as the join key.
        keys: Vec<BindingId>,
        /// True for left-outer (`OPTIONAL MATCH`); false for inner.
        optional: bool,
        /// Explicit output schema after the join.
        output_schema: BindingTableSchema,
        /// Lexical scope visible after the join.
        scope: ScopeId,
        /// Source origin of the right fragment.
        origin: SourceSpan,
    },
    /// Group rows and compute aggregates (`GROUP BY` / implicit grouping).
    Aggregate {
        /// Semantic identities of the grouping keys in source order.
        keys: Vec<ExprId>,
        /// Aggregate applications in discovery order.
        aggregates: Vec<LogicalAggregate>,
        /// Explicit output schema after grouping.
        output_schema: BindingTableSchema,
        /// Lexical scope visible after grouping.
        scope: ScopeId,
        /// Source origin of the `RETURN`/`WITH` clause.
        origin: SourceSpan,
    },
    /// Sort rows by semantic keys (`ORDER BY`).
    Order {
        /// Sort keys in source order.
        keys: Vec<LogicalOrderKey>,
        /// Explicit output schema (identical to the input schema).
        output_schema: BindingTableSchema,
        /// Source origin of the `ORDER BY` clause.
        origin: SourceSpan,
    },
    /// Deduplicate rows (`DISTINCT`).
    Distinct {
        /// Explicit output schema (identical to the input schema).
        output_schema: BindingTableSchema,
        /// Source origin.
        origin: SourceSpan,
    },
    /// Set-composition boundary between two query arms (`UNION`/`INTERSECT`/etc).
    Union {
        /// Parser set operator, preserved exactly for the row adapter.
        op: SetOp,
        /// Explicit output schema (left-arm schema; arms are column name-equal).
        output_schema: BindingTableSchema,
        /// Source origin of the right arm.
        origin: SourceSpan,
    },
    /// `NEXT`-chain boundary between two query blocks.
    Chain {
        /// True when the right block references prior bindings (per-row).
        correlated: bool,
        /// Explicit output schema (final-block schema).
        output_schema: BindingTableSchema,
        /// Source origin of the right block.
        origin: SourceSpan,
    },
    /// Inline `CALL { ... }` table subquery with explicit variable scope.
    Subquery {
        /// Imported outer bindings (`CALL (a, b) { ... }`); empty is isolated.
        imports: Vec<BindingId>,
        /// True for `OPTIONAL CALL`.
        optional: bool,
        /// Lowered body plan executed per input row.
        body: Box<LogicalPlan>,
        /// Explicit output schema (input columns plus yielded columns).
        output_schema: BindingTableSchema,
        /// Lexical scope visible after the subquery.
        scope: ScopeId,
        /// Source origin of the subquery.
        origin: SourceSpan,
    },
    /// Catalog DDL with its logical kind and effect fixed here.
    Catalog {
        /// Statement family; payloads ride source syntax in the row adapter.
        kind: LogicalCatalogKind,
        /// Explicit output schema (`SHOW` columns, else empty).
        output_schema: BindingTableSchema,
        /// Source origin of the DDL statement.
        origin: SourceSpan,
    },
    /// Transaction or session control.
    Control {
        /// Control family; payloads ride source syntax in the row adapter.
        kind: LogicalControlKind,
        /// Source origin.
        origin: SourceSpan,
    },
    /// `EXPLAIN` wrapper over one lowered inner plan (never executed).
    Explain {
        /// Lowered inner plan.
        inner: Box<LogicalPlan>,
        /// Explicit output schema (single `plan` string column).
        output_schema: BindingTableSchema,
        /// Source origin of the `EXPLAIN` statement.
        origin: SourceSpan,
    },
}

impl LogicalOp {
    /// Return this operator's logical effect.
    #[must_use]
    pub const fn effect(&self) -> LogicalEffect {
        match self {
            Self::Scan { .. }
            | Self::Filter { .. }
            | Self::Project { .. }
            | Self::Extend { .. }
            | Self::Unwind { .. }
            | Self::Match { .. }
            | Self::Join { .. }
            | Self::Page { .. } => LogicalEffect::Query,
            Self::Aggregate { .. } => LogicalEffect::Query,
            Self::Order { .. } => LogicalEffect::Query,
            Self::Distinct { .. } => LogicalEffect::Query,
            Self::Union { op, .. } => match op {
                SetOp::Union
                | SetOp::UnionAll
                | SetOp::Intersect
                | SetOp::IntersectAll
                | SetOp::Except
                | SetOp::ExceptAll
                | SetOp::Otherwise => LogicalEffect::Query,
            },
            Self::Chain { .. } => LogicalEffect::Query,
            Self::Subquery { body, .. } => body.effects.effect,
            Self::Mutate { .. } => LogicalEffect::Data,
            Self::Call { descriptor, .. } => descriptor.effect,
            Self::Catalog { kind, .. } => kind.effect(),
            Self::Control { kind, .. } => kind.effect(),
            Self::Explain { .. } => LogicalEffect::Query,
        }
    }

    /// Return this operator's explicit output schema.
    #[must_use]
    pub fn output_schema(&self) -> &BindingTableSchema {
        match self {
            Self::Scan { output_schema, .. }
            | Self::Filter { output_schema, .. }
            | Self::Project { output_schema, .. }
            | Self::Extend { output_schema, .. }
            | Self::Unwind { output_schema, .. }
            | Self::Match { output_schema, .. }
            | Self::Join { output_schema, .. }
            | Self::Aggregate { output_schema, .. }
            | Self::Order { output_schema, .. }
            | Self::Distinct { output_schema, .. }
            | Self::Union { output_schema, .. }
            | Self::Chain { output_schema, .. }
            | Self::Subquery { output_schema, .. }
            | Self::Page { output_schema, .. }
            | Self::Mutate { output_schema, .. }
            | Self::Call { output_schema, .. }
            | Self::Catalog { output_schema, .. }
            | Self::Explain { output_schema, .. } => output_schema,
            Self::Control { .. } => {
                static EMPTY: BindingTableSchema = BindingTableSchema {
                    columns: Vec::new(),
                };
                &EMPTY
            }
        }
    }

    /// Return this operator's row-order contract.
    #[must_use]
    pub const fn ordering(&self) -> LogicalOrdering {
        match self {
            // Scan order is graph-iteration order: deterministic for a pinned
            // snapshot but not a semantic order promise.
            Self::Scan { .. } => LogicalOrdering::Unordered,
            Self::Order { .. } | Self::Union { .. } => LogicalOrdering::Defined,
            Self::Filter { .. }
            | Self::Project { .. }
            | Self::Extend { .. }
            | Self::Unwind { .. }
            | Self::Match { .. }
            | Self::Join { .. }
            | Self::Aggregate { .. }
            | Self::Distinct { .. }
            | Self::Chain { .. }
            | Self::Subquery { .. }
            | Self::Mutate { .. }
            | Self::Call { .. }
            | Self::Catalog { .. }
            | Self::Control { .. }
            | Self::Explain { .. } => LogicalOrdering::Preserved,
            // Page never reorders; it only skips and takes in input order.
            Self::Page { .. } => LogicalOrdering::Preserved,
        }
    }

    /// Return this operator's duplicate-row contract.
    #[must_use]
    pub const fn multiplicity(&self) -> LogicalMultiplicity {
        match self {
            Self::Distinct { .. } | Self::Aggregate { .. } => LogicalMultiplicity::Distinct,
            Self::Union { op, .. } => match op {
                SetOp::Union | SetOp::Intersect | SetOp::Except | SetOp::Otherwise => {
                    LogicalMultiplicity::Distinct
                }
                SetOp::UnionAll | SetOp::IntersectAll | SetOp::ExceptAll => {
                    LogicalMultiplicity::PreservesDuplicates
                }
            },
            Self::Scan { .. }
            | Self::Filter { .. }
            | Self::Project { .. }
            | Self::Extend { .. }
            | Self::Unwind { .. }
            | Self::Match { .. }
            | Self::Join { .. }
            | Self::Order { .. }
            | Self::Chain { .. }
            | Self::Subquery { .. }
            | Self::Page { .. }
            | Self::Mutate { .. }
            | Self::Call { .. }
            | Self::Catalog { .. }
            | Self::Control { .. }
            | Self::Explain { .. } => LogicalMultiplicity::PreservesDuplicates,
        }
    }

    /// Return this operator's source origin.
    #[must_use]
    pub const fn origin(&self) -> SourceSpan {
        match self {
            Self::Scan { origin, .. }
            | Self::Filter { origin, .. }
            | Self::Project { origin, .. }
            | Self::Extend { origin, .. }
            | Self::Unwind { origin, .. }
            | Self::Match { origin, .. }
            | Self::Join { origin, .. }
            | Self::Aggregate { origin, .. }
            | Self::Order { origin, .. }
            | Self::Distinct { origin, .. }
            | Self::Union { origin, .. }
            | Self::Chain { origin, .. }
            | Self::Subquery { origin, .. }
            | Self::Page { origin, .. }
            | Self::Mutate { origin, .. }
            | Self::Call { origin, .. }
            | Self::Catalog { origin, .. }
            | Self::Control { origin, .. }
            | Self::Explain { origin, .. } => *origin,
        }
    }
}

/// Logical page amount without physical coordinates.
///
/// Literal counts lower directly; parameters resolve through the request's
/// parameter contract at execution time. No batch size, cursor position, or
/// storage offset appears here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalPageAmount {
    /// Literal row count.
    Literal(u64),
    /// Request parameter supplying the count.
    Parameter {
        /// Exact decoded parameter name without `$`.
        name: selene_core::DbString,
    },
}

impl LogicalPageAmount {
    /// Build a logical page amount from a lowered limit amount.
    ///
    /// Parameter declarations come from the semantic parameter contract, not
    /// from physical row coordinates.
    #[must_use]
    pub fn from_limit_amount(amount: &LimitAmount) -> Self {
        match amount {
            LimitAmount::Literal(value) => Self::Literal(*value),
            LimitAmount::Parameter { name, .. } => Self::Parameter { name: name.clone() },
        }
    }
}

/// One lowered logical plan over binding tables.
///
/// Operators execute in order against an initial unit table (one empty row)
/// or a graph-access seed. The plan carries its overall effect summary, its
/// final output schema, its lowered path automata, and the dependency inputs
/// a cache needs to decide reuse. It carries no physical execution policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalPlan {
    /// Logical operators in execution order.
    pub operators: Vec<LogicalOp>,
    /// Lowered path automata in clause/pattern order (possibly empty).
    ///
    /// Every top-level `MATCH` pattern plus every `EXISTS`/`CALL`-subquery
    /// graph pattern lowers here from semantic descriptors; `Match`
    /// operators record the clause boundaries that consume them.
    pub paths: LoweredPathSet,
    /// Conservative effect summary for the whole statement.
    pub effects: EffectSummary,
    /// Final output schema.
    pub output_schema: BindingTableSchema,
    /// Registry epoch observed during lowering.
    pub registry_version: u64,
    /// Number of binding-table columns at the input boundary.
    pub input_width: usize,
}

impl LogicalPlan {
    /// Return the plan's overall logical effect.
    #[must_use]
    pub const fn effect(&self) -> LogicalEffect {
        self.effects.effect
    }

    /// Return true when a read-only transaction must reject this plan before
    /// publication.
    #[must_use]
    pub const fn rejects_in_read_only(&self) -> bool {
        self.effects.rejects_in_read_only()
    }

    /// Return the output column for `name`, when present.
    #[must_use]
    pub fn output_column(&self, name: &selene_core::DbString) -> Option<&BindingTableColumn> {
        self.output_schema
            .columns
            .iter()
            .find(|column| column.name.as_ref() == Some(name))
    }

    /// Return the row-order contract of the whole plan (the last operator's
    /// contract, or unordered for an empty plan).
    #[must_use]
    pub fn ordering(&self) -> LogicalOrdering {
        self.operators
            .last()
            .map_or(LogicalOrdering::Unordered, LogicalOp::ordering)
    }

    /// Return true when every operator preserves duplicates.
    #[must_use]
    pub fn preserves_duplicates(&self) -> bool {
        self.operators
            .iter()
            .all(|op| op.multiplicity() == LogicalMultiplicity::PreservesDuplicates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_and_filter_preserve_order_and_duplicates() {
        let schema = BindingTableSchema {
            columns: Vec::new(),
        };
        let origin = SourceSpan::default();
        let filter = LogicalOp::Filter {
            predicate: crate::analyze::ExprId::new(0),
            scope: crate::analyze::ScopeId::new(0),
            output_schema: schema.clone(),
            origin,
        };
        assert_eq!(filter.ordering(), LogicalOrdering::Preserved);
        assert_eq!(
            filter.multiplicity(),
            LogicalMultiplicity::PreservesDuplicates
        );
        let page = LogicalOp::Page {
            offset: LogicalPageAmount::Literal(0),
            count: LogicalPageAmount::Literal(10),
            output_schema: schema,
            origin,
        };
        assert_eq!(page.ordering(), LogicalOrdering::Preserved);
        assert_eq!(
            page.multiplicity(),
            LogicalMultiplicity::PreservesDuplicates
        );
    }
}
