#![allow(missing_docs)]
#![allow(dead_code)]

use std::time::Duration;

use criterion::Criterion;
use selene_gql::{
    EmptyProcedureRegistry, ExecutionPlan, ImplDefinedCaps, OptimizeContext, ProcedureRegistry,
    Session, StatementOutput, analyze, execute_statement, optimize, parse, plan,
};
use selene_graph::SharedGraph;
use selene_testing::{BenchFixture, BenchProfile, PlanCorpusCategory};
use selene_testing::{MockIndexCatalog, MockProcedureRegistry};
use selene_testing::{PlanCorpus, PlanCorpusEntry, PlanCorpusRegistry};

pub(crate) fn criterion_config() -> Criterion {
    let profile = BenchProfile::from_env();
    Criterion::default()
        .sample_size(profile.sample_size())
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(match profile {
            BenchProfile::Quick => 500,
            BenchProfile::Full | BenchProfile::Stress => 1_500,
            _ => 500,
        }))
}

pub(crate) fn corpus_entries() -> Vec<PlanCorpusEntry> {
    PlanCorpus::m5c().entries().cloned().collect()
}

pub(crate) fn registry_for<'a>(
    entry: &PlanCorpusEntry,
    empty: &'a EmptyProcedureRegistry,
    mock: &'a MockProcedureRegistry,
) -> &'a dyn ProcedureRegistry {
    match entry.registry {
        PlanCorpusRegistry::Empty => empty,
        PlanCorpusRegistry::StandardMock => mock,
        _ => empty,
    }
}

pub(crate) fn context_for<'a>(
    entry: &PlanCorpusEntry,
    caps: &'a ImplDefinedCaps,
    catalog: &'a MockIndexCatalog,
) -> OptimizeContext<'a> {
    let ctx = OptimizeContext::new(caps);
    if entry.uses_index_catalog {
        ctx.with_index_catalog(catalog)
    } else {
        ctx
    }
}

pub(crate) fn plan_and_optimize_entry(
    entry: &PlanCorpusEntry,
    empty: &EmptyProcedureRegistry,
    mock: &MockProcedureRegistry,
    catalog: &MockIndexCatalog,
) -> ExecutionPlan {
    let registry = registry_for(entry, empty, mock);
    let statement = parse(entry.source).expect("corpus source parses");
    let analyzed = analyze(statement, registry, None).expect("corpus source analyzes");
    let planned = plan(&analyzed, registry).expect("corpus source plans");
    let caps = ImplDefinedCaps::default();
    let ctx = context_for(entry, &caps, catalog);
    optimize(planned, &ctx)
}

pub(crate) fn representative_plan() -> ExecutionPlan {
    let entries = corpus_entries();
    let entry = entries
        .iter()
        .find(|entry| entry.category == PlanCorpusCategory::Read && entry.uses_index_catalog)
        .expect("corpus has an index-aware read entry");
    let empty = EmptyProcedureRegistry;
    let mock = PlanCorpus::standard_mock_registry();
    let catalog = PlanCorpus::standard_mock_catalog();
    plan_and_optimize_entry(entry, &empty, &mock, &catalog)
}

pub(crate) struct GqlWriteState {
    pub(crate) graph: SharedGraph,
}

/// In-memory (no-WAL) GQL write state — isolates parse/plan/execute + commit
/// CPU from durability.
///
/// Durable commit baselines belong to the `selene-db` facade benches
/// (`durable_commit`, `durable_checkpoint`); GQL write-path arms stay
/// in-memory so a durability cost cannot swamp the GQL CPU deltas
/// (GQLRT-05 / CORE-06) the write-path arms exist to measure.
pub(crate) fn gql_write_state_in_memory(scale: usize) -> GqlWriteState {
    let fixture = BenchFixture::build(scale);
    GqlWriteState {
        graph: SharedGraph::from_graph(fixture.graph().clone()),
    }
}

pub(crate) fn plan_write(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("write source parses");
    let analyzed =
        analyze(statement, &EmptyProcedureRegistry, None).expect("write source analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("write source plans")
}

pub(crate) fn execute_preplanned(plan: &ExecutionPlan, session: &mut Session<'_>) -> usize {
    match execute_statement(plan, session, &EmptyProcedureRegistry)
        .expect("write statement executes")
    {
        StatementOutput::Rows(table) => table.row_count(),
        StatementOutput::Written(outcome) => {
            let rows = outcome.rows.as_ref().map_or(0, |table| table.row_count());
            rows + outcome.changes.len()
        }
        StatementOutput::Empty => 0,
        _ => panic!("unexpected statement output"),
    }
}
