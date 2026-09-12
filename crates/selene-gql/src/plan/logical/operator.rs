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
    SourceSpan,
    analyze::{AnalyzedType, BindingId, ScopeId},
    plan::{BindingTableColumn, BindingTableSchema, LimitAmount},
};

use super::effect::{EffectSummary, LogicalEffect};

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

/// One logical binding-table operator.
///
/// The slice covers scan, filter, project, page, one ordinary mutation path,
/// and named-procedure calls. Full family coverage (joins, grouping,
/// set operations beyond the carried schemas, path operators) stays with
/// F03-PR04; the contracts here are sufficient for physical batches, path
/// semantic nodes, and native adapters to build upon.
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
}

impl LogicalOp {
    /// Return this operator's logical effect.
    #[must_use]
    pub const fn effect(&self) -> LogicalEffect {
        match self {
            Self::Scan { .. } | Self::Filter { .. } | Self::Project { .. } | Self::Page { .. } => {
                LogicalEffect::Query
            }
            Self::Mutate { .. } => LogicalEffect::Data,
            Self::Call { descriptor, .. } => descriptor.effect,
        }
    }

    /// Return this operator's explicit output schema.
    #[must_use]
    pub fn output_schema(&self) -> &BindingTableSchema {
        match self {
            Self::Scan { output_schema, .. }
            | Self::Filter { output_schema, .. }
            | Self::Project { output_schema, .. }
            | Self::Page { output_schema, .. }
            | Self::Mutate { output_schema, .. }
            | Self::Call { output_schema, .. } => output_schema,
        }
    }

    /// Return this operator's row-order contract.
    #[must_use]
    pub const fn ordering(&self) -> LogicalOrdering {
        match self {
            // Scan order is graph-iteration order: deterministic for a pinned
            // snapshot but not a semantic order promise.
            Self::Scan { .. } => LogicalOrdering::Unordered,
            Self::Filter { .. }
            | Self::Project { .. }
            | Self::Mutate { .. }
            | Self::Call { .. } => LogicalOrdering::Preserved,
            // Page never reorders; it only skips and takes in input order.
            Self::Page { .. } => LogicalOrdering::Preserved,
        }
    }

    /// Return this operator's duplicate-row contract.
    #[must_use]
    pub const fn multiplicity(&self) -> LogicalMultiplicity {
        match self {
            // Filter, project, page, scan, ordinary mutation, and procedure
            // calls in this slice all preserve duplicates. A future distinct
            // operator will carry `Distinct`; nothing here silently
            // deduplicates.
            Self::Scan { .. }
            | Self::Filter { .. }
            | Self::Project { .. }
            | Self::Page { .. }
            | Self::Mutate { .. }
            | Self::Call { .. } => LogicalMultiplicity::PreservesDuplicates,
        }
    }

    /// Return this operator's source origin.
    #[must_use]
    pub const fn origin(&self) -> SourceSpan {
        match self {
            Self::Scan { origin, .. }
            | Self::Filter { origin, .. }
            | Self::Project { origin, .. }
            | Self::Page { origin, .. }
            | Self::Mutate { origin, .. }
            | Self::Call { origin, .. } => *origin,
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
/// final output schema, and the dependency inputs a cache needs to decide
/// reuse. It carries no physical execution policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalPlan {
    /// Logical operators in execution order.
    pub operators: Vec<LogicalOp>,
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
