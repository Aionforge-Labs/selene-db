//! Planner IR definitions.

mod access;
mod call;
mod caps;
mod catalog;
mod execution;
mod filter;
mod mutation;
mod path;
mod session;
mod subquery;
mod tx;

use crate::{
    EdgeDirection, LabelExpr, SetOp, SourceSpan,
    analyze::{AnalyzedType, BindingId},
};

pub use access::{IndexKey, NodeIdOrdering, OrderAccess, ScanAccess, TypedIndexBounds};
pub use call::{PlannedCall, PlannedYieldItem, YieldKind};
pub use caps::ImplDefinedCaps;
pub use catalog::{CatalogOp, PlannedTypePropertyConstraint, PlannedTypePropertyDef};
pub use execution::{ExecutionPlan, PipelineOpId};
pub use filter::{
    Aggregate, AggregateArg, FilterPredicate, FilterPredicateKind, LimitAmount, OrderKey,
    ProjectExpr,
};
pub use mutation::{DeleteTargetPlan, InsertEndpointRef, InsertSiteId, MutationOp, PropertyInit};
pub use path::{PathConditions, PathProgram};
pub use session::SessionOp;
pub use subquery::{
    OuterBindingRef, PlannedSubquery, PlannedTableSubquery, PlannedTableSubqueryYield,
    SubqueryBody, SubqueryKind, SubqueryRegistry,
};
pub use tx::TxOp;

/// Pattern-matching subplan for the leading MATCH prefix.
#[derive(Clone, Debug)]
pub struct PatternPlan {
    /// Named pattern bindings visible to downstream pipeline operations.
    pub bindings: Vec<BindingDef>,
    /// Unoptimized join tree.
    pub join_tree: JoinTree,
    /// Inline and clause-level predicates attached to the pattern phase.
    pub filters: Vec<FilterPredicate>,
}

/// Named binding defined by pattern analysis.
#[derive(Clone, Debug, PartialEq)]
pub struct BindingDef {
    /// Analyzer-stable binding ID.
    pub binding: BindingId,
    /// Database-string binding name.
    pub name: selene_core::DbString,
    /// Element kind represented by the binding.
    pub element: BindingElement,
    /// Analyzer-inferred binding type.
    pub ty: AnalyzedType,
    /// Static label predicate from the declaring pattern, when present.
    pub label_predicate: Option<LabelExpr>,
    /// Source span of the declaration.
    pub span: SourceSpan,
}

/// Binding element category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingElement {
    /// Node binding.
    Node,
    /// Edge binding.
    Edge,
    /// Path binding.
    Path,
}

/// Pattern join tree.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum JoinTree {
    /// A complete logical path clause executed by the product-path batch operator.
    Paths(Box<PathProgram>),
    /// One-row, all-null anchor used to model leading optional graph patterns.
    Unit,
    /// Scan nodes or edges.
    Scan(NodeOrEdgeScan),
    /// Expand from a child tree across one edge pattern.
    Expand {
        /// Input side of the expansion.
        child: Box<JoinTree>,
        /// Edge pattern to traverse.
        edge: EdgeMatch,
        /// Direction requested by the source pattern.
        direction: EdgeDirection,
    },
    /// Binary join between two pattern fragments.
    HashJoin {
        /// Left input.
        left: Box<JoinTree>,
        /// Right input.
        right: Box<JoinTree>,
        /// Shared binding names used as the join key.
        key: Vec<selene_core::DbString>,
        /// Planner-selected build input.
        build_side: BuildSide,
    },
    /// Left-outer join used for OPTIONAL MATCH.
    Outer {
        /// Preserved left input.
        left: Box<JoinTree>,
        /// Optional right input.
        right: Box<JoinTree>,
        /// Shared binding names used as the join key.
        key: Vec<selene_core::DbString>,
        /// Predicates scoped to the optional right side.
        right_filters: Vec<FilterPredicate>,
    },
    /// Marker for future WCO rewrites.
    WorstCaseOptimal {
        /// Intersected subplans.
        intersection: Vec<JoinTree>,
        /// Node-id orderings used to break symmetric WCO traversals.
        node_id_ordering: Vec<NodeIdOrdering>,
    },
    /// A fully nested [`ExecutionPlan`] executed as a single join-tree node.
    ///
    /// Reserved for correlated-`CALL` subquery lowering: a `CALL { ... }` whose
    /// body imports outer bindings will lower to this variant so the inner
    /// pipeline runs per outer row. No production lowering rule constructs it
    /// yet (correlated-`CALL` is not lowered at HEAD; only tests build it), but
    /// the runtime path is complete — the sole executor is the runtime subplan
    /// executor reached from the `JoinTree::Subplan` arm of pattern walking. It
    /// is kept (not removed) as the working substrate for that near-term
    /// direction.
    Subplan(Box<ExecutionPlan>),
    /// Per-label sub-scans wrapping a flat-disjunctive-label pattern.
    ///
    /// Emitted by the `disjunctive_label_expansion` optimizer rule when a node
    /// scan carries a flat `LabelExpr::Disjunction([Single, Single, …])` label
    /// expression and at least one per-label branch has an applicable typed,
    /// composite, or in-list index. Each branch is a clone of the original
    /// scan with `label_predicate = Some(LabelExpr::Single(L_i))`, allowing the
    /// downstream index-selection rules (`composite_index_lookup`,
    /// `in_list_optimization`, `range_index_scan`) to set per-branch
    /// `ScanAccess` independently.
    ///
    /// Runtime executes each branch via the standard `scan_pattern` entry and
    /// concatenates the per-branch `Binding` rows with `UNION ALL` semantics
    /// (no dedup; a node carrying labels A AND B appears in both branches'
    /// candidate sets, matching the manual `MATCH (n:A) UNION ALL MATCH (n:B)`
    /// behaviour). Per-branch label filtering applies via the existing
    /// `label_matches_scan` machinery against each branch's single-label
    /// predicate.
    DisjunctiveScan {
        /// Per-label sub-scans, each with `label_predicate =
        /// Some(LabelExpr::Single(L_i))` and a clone of the original scan's
        /// property predicates + bindings.
        ///
        /// `Vec`, not `Vec2OrMore`, because the source
        /// `LabelExpr::Disjunction(Vec2OrMore<LabelExpr>)` already guarantees
        /// `≥ 2` branches at construction time.
        branches: Vec<NodeOrEdgeScan>,
        /// The original scan, retained for EXPLAIN diagnostics and to preserve
        /// the original disjunctive `label_predicate` for post-commit walks.
        /// Carries the same `binding` / `hidden_binding` IDs that the branches
        /// inherit, so downstream pipeline ops resolve `(n)` against the
        /// unioned binding table consistently.
        scan_anchor: NodeOrEdgeScan,
    },
}

/// Planner-selected hash-join build side.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildSide {
    /// Build the hash table from the left input.
    Left,
    /// Build the hash table from the right input.
    Right,
}

/// Executor-private binding slot for anonymous pattern elements.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HiddenBindingId(u32);

impl HiddenBindingId {
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Return this hidden slot's zero-based numeric index.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Node or edge scan.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeOrEdgeScan {
    /// Named binding, or `None` for anonymous pattern elements.
    pub binding: Option<BindingId>,
    /// Executor-private slot for anonymous scan elements.
    pub hidden_binding: Option<HiddenBindingId>,
    /// Scan kind.
    pub kind: ScanKind,
    /// Label predicate attached to the scanned element.
    pub label_predicate: Option<LabelExpr>,
    /// Inline property predicates from the pattern.
    pub property_predicates: Vec<FilterPredicate>,
    /// Optimizer-selected access path.
    pub access: ScanAccess,
    /// Source span.
    pub span: SourceSpan,
}

/// Scan element kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanKind {
    /// Node scan.
    Node,
    /// Edge scan.
    Edge,
}

/// Edge pattern in an expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeMatch {
    /// Named edge binding, or `None` for anonymous edge patterns.
    pub binding: Option<BindingId>,
    /// Executor-private slot for anonymous edge patterns.
    pub hidden_binding: Option<HiddenBindingId>,
    /// Label predicate attached to the edge.
    pub label_predicate: Option<LabelExpr>,
    /// Inline property predicates from the edge pattern.
    pub property_predicates: Vec<FilterPredicate>,
    /// Binding on the syntactic left side of the edge, if named.
    pub left_binding: Option<BindingId>,
    /// Executor-private slot on the syntactic left side of the edge.
    pub left_hidden_binding: Option<HiddenBindingId>,
    /// Binding on the syntactic right side of the edge, if named.
    pub right_binding: Option<BindingId>,
    /// Executor-private slot on the syntactic right side of the edge.
    pub right_hidden_binding: Option<HiddenBindingId>,
    /// Label predicate on the syntactic right-side node, if any.
    pub right_label_predicate: Option<LabelExpr>,
    /// Property-map equality predicates on the syntactic right-side node.
    pub right_property_predicates: Vec<FilterPredicate>,
    /// Optimizer-selected access path.
    pub access: ScanAccess,
    /// Source span.
    pub span: SourceSpan,
}

/// Pipeline operation over binding tables.
///
/// `#[non_exhaustive]` so future planner work (e.g., MERGE lowering, CALL
/// subquery form, INDEX DDL) can add variants without breaking downstream
/// pattern matches.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
pub enum PipelineOp {
    /// Retain rows satisfying a predicate.
    Filter(FilterPredicate),
    /// Project expressions into output columns.
    Project(Vec<ProjectExpr>),
    /// Extend the binding table with new aliases without dropping prior columns.
    Let(Vec<ProjectExpr>),
    /// Expand a list expression to one row per element.
    Unwind {
        /// Source list expression.
        source: ProjectExpr,
        /// Alias bound to each list element.
        alias: selene_core::DbString,
        /// Optional position output for ISO `FOR`.
        position: Option<crate::RowExpansionPosition>,
        /// Source span.
        span: SourceSpan,
    },
    /// Sort rows.
    OrderBy(Vec<OrderKey>),
    /// Drop the carrier columns appended so `ORDER BY` could reach a binding
    /// the `RETURN` discards.
    ///
    /// ISO/IEC 39075:2024 §14.10 SR 4)c)i)2)A)VIII appends `REF AS REF` to a
    /// copy of the return item list for every sort-key reference that is not
    /// already a return alias, and GR 1)b)ii sets the working table to a copy
    /// without exactly those columns once the ordering and page statement has
    /// run. Carriers are appended after the projected columns, so dropping them
    /// is a truncation to `projected_width`.
    ///
    /// Positional rather than by name because a return item need not have an
    /// alias: `RETURN d.tag` produces a column whose name is `None`, which no
    /// name-keyed trim could reproduce.
    TrimOrderCarriers {
        /// Number of leading columns the `RETURN` actually projects.
        projected_width: usize,
    },
    /// Offset and limit rows.
    Limit {
        /// Rows to skip.
        offset: LimitAmount,
        /// Rows to retain after offset.
        count: LimitAmount,
    },
    /// Sort rows while retaining only the bounded top range.
    TopK {
        /// Sort keys preserved from the fused `OrderBy`.
        keys: Vec<OrderKey>,
        /// Rows to skip before yielding.
        offset: LimitAmount,
        /// Rows to retain after offset.
        count: LimitAmount,
    },
    /// Group and aggregate rows.
    GroupBy {
        /// Grouping keys.
        keys: Vec<ProjectExpr>,
        /// Aggregate expressions.
        aggregates: Vec<Aggregate>,
    },
    /// Deduplicate rows.
    Distinct,
    /// Apply a set-composition operation with another plan.
    Union {
        /// Parser set operator, preserved exactly.
        op: SetOp,
        /// Right-hand plan.
        rhs: Box<ExecutionPlan>,
    },
    /// Evaluate a NEXT block after the current plan.
    Chain(Box<ExecutionPlan>),
    /// Evaluate a NEXT block once per input row because it references prior bindings.
    CorrelatedChain(Box<ExecutionPlan>),
    /// Match a graph pattern against each incoming row.
    Match(PatternPlan),
    /// Optionally match a graph pattern against each incoming row.
    OptionalMatch(PatternPlan),
    /// Planned procedure call.
    Call(PlannedCall),
    /// Inline `CALL { ... }` table subquery.
    CallSubquery(Box<PlannedTableSubquery>),
    /// Mutation operation.
    Mutation(MutationOp),
    /// Catalog operation.
    Catalog(CatalogOp),
    /// Return a textual dump of an inner execution plan.
    ExplainPlan {
        /// Planned inner statement. It is never executed by this operation.
        inner: Box<ExecutionPlan>,
        /// Source span.
        span: SourceSpan,
    },
    /// Transaction-control operation.
    Tx(TxOp),
    /// Session-control operation (ISO/IEC 39075:2024 section 7).
    Session(SessionOp),
}

/// Binding-table output schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingTableSchema {
    /// Output columns in order.
    pub columns: Vec<BindingTableColumn>,
}

/// One binding-table output column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingTableColumn {
    /// Stable column name for aliases and bare variable projections.
    pub name: Option<selene_core::DbString>,
    /// Executor-private anonymous pattern slot.
    pub hidden: Option<HiddenBindingId>,
    /// Analyzer-inferred column type.
    pub ty: AnalyzedType,
}
