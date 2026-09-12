//! Logical effect classification from semantic descriptors.
//!
//! Effects resolve from the frozen semantic tree — the analyzer's
//! `StatementCategory`, the analyzer's `MutationWriteSet`, and each resolved
//! procedure application's registration metadata — never from whether a
//! procedure implementation happens to write, and never from operation names
//! alone. A parser-only classifier that inspects only the top-level statement
//! shape cannot pass the indirect-write regression: it would miss an effectful
//! nested `CALL` inside an otherwise read-only query pipeline.
//!
//! Write-set computation stays conservative and cheap: it counts analyzer
//! entries and procedure effects in one linear pass, with no per-row,
//! per-property, or graph-content inspection.

use crate::{
    ProcedureMutability, SourceSpan,
    analyze::{AnalyzedStatement, StatementCategory},
    plan::{ExecutionPlan, PipelineOp, PlannerError},
};

/// Logical side-effect class for one binding-table operator or statement.
///
/// This mirrors `StatementCategory` at logical
/// granularity so the facade can enforce transaction policy before
/// publication without re-deriving semantics from parser syntax.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LogicalEffect {
    /// Query-only operator; preserves snapshots and publishes nothing.
    Query,
    /// Operator may modify graph data (nodes, edges, properties).
    Data,
    /// Operator may modify graph/catalog metadata (types, indexes, graphs).
    Catalog,
    /// Operator rebuilds derived engine state outside the writer funnel.
    Maintenance,
    /// Session-control operator (ISO/IEC 39075:2024 section 7).
    Session,
    /// Transaction-control operator (`START`/`COMMIT`/`ROLLBACK`).
    Transaction,
}

impl LogicalEffect {
    /// Return true when this effect stages or publishes shared state.
    #[must_use]
    pub const fn is_write(self) -> bool {
        match self {
            Self::Query | Self::Session | Self::Transaction => false,
            Self::Data | Self::Catalog | Self::Maintenance => true,
        }
    }

    /// Return true when this effect is rejected in a read-only transaction.
    #[must_use]
    pub const fn rejects_in_read_only(self) -> bool {
        match self {
            Self::Data | Self::Catalog | Self::Maintenance => true,
            Self::Query | Self::Session | Self::Transaction => false,
        }
    }

    /// Map this logical effect to its statement category.
    #[must_use]
    pub const fn category(self) -> StatementCategory {
        match self {
            Self::Query => StatementCategory::ReadOnly,
            Self::Data => StatementCategory::DataModifying,
            Self::Catalog => StatementCategory::CatalogModifying,
            Self::Maintenance => StatementCategory::Maintenance,
            Self::Session => StatementCategory::SessionControl,
            Self::Transaction => StatementCategory::TransactionControl,
        }
    }

    /// Map a statement category to its logical effect.
    #[must_use]
    pub const fn from_category(category: StatementCategory) -> Self {
        match category {
            StatementCategory::ReadOnly => Self::Query,
            StatementCategory::DataModifying => Self::Data,
            StatementCategory::CatalogModifying => Self::Catalog,
            StatementCategory::Maintenance => Self::Maintenance,
            StatementCategory::SessionControl => Self::Session,
            StatementCategory::TransactionControl => Self::Transaction,
        }
    }

    /// Map procedure registration mutability to its logical effect.
    ///
    /// This is the single metadata-authoritative mapping. Callers must not
    /// infer effects from procedure names or from observed implementation
    /// behavior.
    #[must_use]
    pub const fn from_mutability(mutability: ProcedureMutability) -> Self {
        match mutability {
            ProcedureMutability::Read => Self::Query,
            ProcedureMutability::SchemaWrite => Self::Catalog,
            ProcedureMutability::MaintenanceWrite => Self::Maintenance,
        }
    }
}

/// Conservative effect summary for one statement.
///
/// `has_data` and `has_catalog` together mean the statement mixes catalog and
/// data effects. The selected GP18 policy forbids that mix: it must surface as
/// an error, never as a silent split into separate commits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectSummary {
    /// Strongest single effect in precedence order.
    pub effect: LogicalEffect,
    /// Analyzer write-set entries were present (data intent).
    pub has_data_write: bool,
    /// A catalog-modifying procedure or DDL effect was present.
    pub has_catalog_write: bool,
    /// A maintenance procedure effect was present.
    pub has_maintenance_write: bool,
    /// Number of analyzer write-set entries (conservative, cheap).
    pub write_entry_count: usize,
    /// Number of resolved procedure applications.
    pub call_count: usize,
    /// Registry epoch observed during analysis.
    pub registry_version: u64,
    /// Catalog descriptor dependency count (cheap invalidation input).
    pub catalog_dependency_count: usize,
    /// Source origin of the classified statement.
    pub origin: SourceSpan,
}

impl EffectSummary {
    /// Return true when this statement mixes data and catalog effects.
    #[must_use]
    pub const fn is_mixed_data_catalog(&self) -> bool {
        self.has_data_write && self.has_catalog_write
    }

    /// Return true when a read-only transaction must reject this statement
    /// before publication, publishing nothing.
    #[must_use]
    pub const fn rejects_in_read_only(&self) -> bool {
        self.effect.rejects_in_read_only()
    }

    /// Return the facade-routing category for this summary.
    #[must_use]
    pub const fn category(&self) -> StatementCategory {
        self.effect.category()
    }
}

/// Classify one analyzed statement from its semantic descriptors.
///
/// Reads [`AnalyzedStatement::category`](crate::AnalyzedStatement),
/// [`write_set`](crate::analyze::ast::SemanticTree), and every
/// resolved procedure application's `ProcedureMutability`. The top-level
/// parser shape alone is insufficient: a query pipeline whose source is a
/// read-only query may still carry catalog or
/// maintenance effects through nested `CALL` applications, and those resolve
/// here.
#[must_use]
pub fn classify_analyzed(analyzed: &AnalyzedStatement) -> EffectSummary {
    let mut has_data = analyzed.write_set.is_some();
    let mut has_catalog = false;
    let mut has_maintenance = false;
    for call in &analyzed.calls {
        match LogicalEffect::from_mutability(call.metadata().mutability) {
            LogicalEffect::Catalog => has_catalog = true,
            LogicalEffect::Maintenance => has_maintenance = true,
            LogicalEffect::Data => has_data = true,
            LogicalEffect::Query | LogicalEffect::Session | LogicalEffect::Transaction => {}
        }
    }
    // The analyzer's own category contributes DDL/catalog intent that is not
    // always represented as a write-set entry or procedure call (for example,
    // `CREATE SCHEMA` lowers to a catalog op with no mutation write set).
    match analyzed.category {
        StatementCategory::DataModifying => has_data = true,
        StatementCategory::CatalogModifying => has_catalog = true,
        StatementCategory::Maintenance => has_maintenance = true,
        StatementCategory::ReadOnly
        | StatementCategory::SessionControl
        | StatementCategory::TransactionControl => {}
    }
    let effect = if has_maintenance && (has_data || has_catalog) {
        // Maintenance never composes with data/catalog in one statement; keep
        // the maintenance label so the facade rejects it at the maintenance
        // boundary rather than misrouting it as an ordinary write.
        LogicalEffect::Maintenance
    } else if has_catalog {
        // Covers catalog-only and the GP18-mixed (data+catalog) case; the mix
        // keeps the catalog label while `check_gp18` reports the error. The
        // label itself never authorizes a split commit.
        LogicalEffect::Catalog
    } else if has_data {
        LogicalEffect::Data
    } else if has_maintenance {
        LogicalEffect::Maintenance
    } else {
        LogicalEffect::from_category(analyzed.category)
    };
    EffectSummary {
        effect,
        has_data_write: has_data,
        has_catalog_write: has_catalog,
        has_maintenance_write: has_maintenance,
        write_entry_count: analyzed
            .write_set
            .as_ref()
            .map_or(0, |set| set.entries.len()),
        call_count: analyzed.calls.len(),
        registry_version: analyzed.procedure_registry_version,
        catalog_dependency_count: analyzed.catalog.as_ref().map_or(0, |resolution| {
            resolution.objects().len() + resolution.sites().len()
        }),
        origin: analyzed.span,
    }
}

/// Classify one lowered execution plan by walking its operators.
///
/// This is the plan-level counterpart to [`classify_analyzed`]. It resolves
/// named-procedure effects from each [`PlannedCall`](crate::PlannedCall)'s
/// stored registration metadata (`mutability`/`tier`), never from the
/// procedure name or from observed implementation behavior. Nested bodies
/// (`UNION` right-hand sides, `NEXT` chains, `CALL { ... }` subqueries, and
/// `EXPLAIN` inner plans) are visited so an effectful nested call cannot hide
/// behind a read-only top-level category.
#[must_use]
pub fn classify_plan(plan: &ExecutionPlan) -> EffectSummary {
    let mut accumulator = PlanEffectAccumulator::default();
    accumulator.visit_pipeline(&plan.pipeline);
    // The plan category contributes the same DDL/catalog intent as the
    // analyzer category above.
    match plan.category {
        StatementCategory::DataModifying => accumulator.has_data = true,
        StatementCategory::CatalogModifying => accumulator.has_catalog = true,
        StatementCategory::Maintenance => accumulator.has_maintenance = true,
        StatementCategory::ReadOnly
        | StatementCategory::SessionControl
        | StatementCategory::TransactionControl => {}
    }
    let effect = if accumulator.has_maintenance && (accumulator.has_data || accumulator.has_catalog)
    {
        LogicalEffect::Maintenance
    } else if accumulator.has_catalog {
        // Covers both catalog-only and GP18-mixed (data+catalog) cases; the
        // mix keeps the catalog label while `check_gp18` reports the error.
        LogicalEffect::Catalog
    } else if accumulator.has_data {
        LogicalEffect::Data
    } else if accumulator.has_maintenance {
        LogicalEffect::Maintenance
    } else {
        LogicalEffect::from_category(plan.category)
    };
    EffectSummary {
        effect,
        has_data_write: accumulator.has_data,
        has_catalog_write: accumulator.has_catalog,
        has_maintenance_write: accumulator.has_maintenance,
        write_entry_count: accumulator.mutation_ops,
        call_count: accumulator.calls,
        registry_version: 0,
        catalog_dependency_count: 0,
        origin: SourceSpan::default(),
    }
}

/// Enforce the selected GP18 policy for one statement summary.
///
/// Catalog/data mixing remains forbidden. An unsupported mix is reported here
/// so it can never be silently split into separate commits by the facade's
/// implicit-transaction path.
///
/// # Errors
///
/// Returns [`PlannerError::EffectMixing`] when one statement carries both
/// data and catalog effects.
pub fn check_gp18(summary: &EffectSummary) -> Result<(), PlannerError> {
    if summary.is_mixed_data_catalog() {
        return Err(PlannerError::EffectMixing {
            detail: "catalog and data effects cannot be mixed in one statement",
            span: summary.origin,
        });
    }
    Ok(())
}

/// Verify that a lowered plan's effects agree with its semantic summary.
///
/// A parser-only check that compares only top-level statement shapes would
/// miss an effectful nested `CALL` whose outer query still reports
/// [`StatementCategory::ReadOnly`]. This check compares the metadata-resolved
/// plan summary against the semantic summary and the declared plan category.
///
/// # Errors
///
/// Returns [`PlannerError::EffectMismatch`] when the plan carries data,
/// catalog, or maintenance effects that its category does not admit, or
/// [`PlannerError::EffectMixing`] for a GP18-violating mix.
pub fn verify_plan_effects(
    analyzed_summary: &EffectSummary,
    plan: &ExecutionPlan,
) -> Result<EffectSummary, PlannerError> {
    let plan_summary = classify_plan(plan);
    check_gp18(&plan_summary)?;
    check_gp18(analyzed_summary)?;
    // The plan must not be weaker than the semantics it claims to implement.
    // A read-only plan carrying catalog/maintenance/data effects is either a
    // registry drift between analysis and planning or a lowering bug; in both
    // cases execution must not proceed with query authority.
    let plan_admits_write = match plan.category {
        StatementCategory::DataModifying
        | StatementCategory::CatalogModifying
        | StatementCategory::Maintenance => true,
        StatementCategory::ReadOnly
        | StatementCategory::SessionControl
        | StatementCategory::TransactionControl => false,
    };
    if !plan_admits_write
        && (plan_summary.has_data_write
            || plan_summary.has_catalog_write
            || plan_summary.has_maintenance_write)
    {
        return Err(PlannerError::EffectMismatch {
            detail: "lowered plan carries write effects under a read-only category",
            span: plan_summary.origin,
        });
    }
    // The semantic summary must not be weaker than the plan either: planning
    // against a newer registry must not silently acquire write authority the
    // analysis did not see.
    if analyzed_summary.effect == LogicalEffect::Query && plan_summary.effect.rejects_in_read_only()
    {
        return Err(PlannerError::EffectMismatch {
            detail: "procedure metadata changed between analyze and plan",
            span: plan_summary.origin,
        });
    }
    Ok(plan_summary)
}

#[derive(Default)]
struct PlanEffectAccumulator {
    has_data: bool,
    has_catalog: bool,
    has_maintenance: bool,
    mutation_ops: usize,
    calls: usize,
}

impl PlanEffectAccumulator {
    fn visit_pipeline(&mut self, pipeline: &[PipelineOp]) {
        for op in pipeline {
            match op {
                PipelineOp::Mutation(_) => {
                    self.has_data = true;
                    self.mutation_ops = self.mutation_ops.saturating_add(1);
                }
                PipelineOp::Catalog(catalog) => {
                    // `SHOW` catalog ops are read-only introspection; they
                    // carry no write effects despite sharing the catalog
                    // pipeline shape. `TRUNCATE` removes data instances while
                    // keeping the type, so it counts as a data write
                    // (matching `StatementCategory::DataModifying`).
                    match catalog {
                        crate::CatalogOp::ShowNodeTypes(_)
                        | crate::CatalogOp::ShowEdgeTypes(_)
                        | crate::CatalogOp::ShowIndexes(_)
                        | crate::CatalogOp::ShowProcedures(_) => {}
                        crate::CatalogOp::TruncateNodeType { .. }
                        | crate::CatalogOp::TruncateEdgeType { .. } => {
                            self.has_data = true;
                            self.mutation_ops = self.mutation_ops.saturating_add(1);
                        }
                        _ => {
                            self.has_catalog = true;
                        }
                    }
                }
                PipelineOp::Call(call) => {
                    self.calls = self.calls.saturating_add(1);
                    match LogicalEffect::from_mutability(call.mutability) {
                        LogicalEffect::Catalog => self.has_catalog = true,
                        LogicalEffect::Maintenance => self.has_maintenance = true,
                        LogicalEffect::Data => self.has_data = true,
                        LogicalEffect::Query
                        | LogicalEffect::Session
                        | LogicalEffect::Transaction => {}
                    }
                }
                PipelineOp::Union { rhs, .. }
                | PipelineOp::Chain(rhs)
                | PipelineOp::CorrelatedChain(rhs) => {
                    self.visit_pipeline(&rhs.pipeline);
                    // Nested categories contribute the same way the top level
                    // does; a nested data-modifying body under a read-only
                    // wrapper is still a write.
                    match rhs.category {
                        StatementCategory::DataModifying => self.has_data = true,
                        StatementCategory::CatalogModifying => self.has_catalog = true,
                        StatementCategory::Maintenance => self.has_maintenance = true,
                        StatementCategory::ReadOnly
                        | StatementCategory::SessionControl
                        | StatementCategory::TransactionControl => {}
                    }
                }
                PipelineOp::CallSubquery(subquery) => {
                    self.visit_pipeline(&subquery.body.pipeline);
                }
                PipelineOp::ExplainPlan { .. } => {
                    // EXPLAIN never executes its inner plan, so its effects do
                    // not authorize writes. Visit nothing here by design; a
                    // future physical planner must not mistake an explained
                    // write for a statement write.
                }
                PipelineOp::Filter(_)
                | PipelineOp::Project(_)
                | PipelineOp::Let(_)
                | PipelineOp::Unwind { .. }
                | PipelineOp::OrderBy(_)
                | PipelineOp::TrimOrderCarriers { .. }
                | PipelineOp::Limit { .. }
                | PipelineOp::TopK { .. }
                | PipelineOp::GroupBy { .. }
                | PipelineOp::Distinct
                | PipelineOp::Match(_)
                | PipelineOp::OptionalMatch(_)
                | PipelineOp::Tx(_)
                | PipelineOp::Session(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_effect_is_not_a_write() {
        assert!(!LogicalEffect::Query.is_write());
        assert!(!LogicalEffect::Query.rejects_in_read_only());
        assert!(LogicalEffect::Data.rejects_in_read_only());
        assert!(LogicalEffect::Catalog.rejects_in_read_only());
        assert!(LogicalEffect::Maintenance.rejects_in_read_only());
    }

    #[test]
    fn mutability_mapping_never_uses_names() {
        assert_eq!(
            LogicalEffect::from_mutability(ProcedureMutability::Read),
            LogicalEffect::Query
        );
        assert_eq!(
            LogicalEffect::from_mutability(ProcedureMutability::SchemaWrite),
            LogicalEffect::Catalog
        );
        assert_eq!(
            LogicalEffect::from_mutability(ProcedureMutability::MaintenanceWrite),
            LogicalEffect::Maintenance
        );
    }
}
