//! BRIEF-29 range-index optimizer tests.

use selene_core::DbString;
use selene_gql::{
    EmptyProcedureRegistry, IndexKey, IndexKind, JoinTree, NodeOrEdgeScan, ScanAccess,
    TypedIndexBounds, analyze, optimize, parse, plan,
};
use selene_testing::MockIndexCatalog;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn optimized_one(source: &str, catalog: &MockIndexCatalog) -> selene_gql::ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans");
    let ctx = selene_gql::OptimizeContext::default().with_index_catalog(catalog);
    optimize(plan, &ctx)
}

fn person_catalog() -> MockIndexCatalog {
    MockIndexCatalog::new()
        .with_node_typed_index(db_string("Person"), db_string("age"), IndexKind::Integer)
        .with_node_typed_index(db_string("Person"), db_string("name"), IndexKind::String)
}

fn event_catalog() -> MockIndexCatalog {
    MockIndexCatalog::new()
        .with_node_typed_index(db_string("Event"), db_string("event_date"), IndexKind::Date)
        .with_node_typed_index(
            db_string("Event"),
            db_string("started_at"),
            IndexKind::LocalDateTime,
        )
        .with_node_typed_index(
            db_string("Event"),
            db_string("occurred_at"),
            IndexKind::ZonedDateTime,
        )
        .with_node_typed_index(
            db_string("Event"),
            db_string("wall_time"),
            IndexKind::LocalTime,
        )
        .with_node_typed_index(
            db_string("Event"),
            db_string("clock_time"),
            IndexKind::ZonedTime,
        )
        .with_node_typed_index(
            db_string("Event"),
            db_string("elapsed"),
            IndexKind::Duration,
        )
}

fn first_scan(tree: &JoinTree) -> Option<&NodeOrEdgeScan> {
    match tree {
        JoinTree::Scan(scan) => Some(scan),
        JoinTree::Expand { child, .. } => first_scan(child),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            first_scan(left).or_else(|| first_scan(right))
        }
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => None,
        _ => None,
    }
}

#[path = "optimize_range_index_scan/basics.rs"]
mod basics;
#[path = "optimize_range_index_scan/parameters.rs"]
mod parameters;
