//! BRIEF-27 mutation planner lowering tests.

use selene_gql::{
    AnalyzedStatement, BindingTableColumn, EmptyProcedureRegistry, ExecutionPlan,
    InsertEndpointRef, MutationOp, PipelineOp, analyze, parse, plan,
};

fn analyzed(source: &str) -> AnalyzedStatement {
    let statement = parse(source).expect("test input parses");
    analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes")
}

fn plan_one(source: &str) -> ExecutionPlan {
    let analyzed = analyzed(source);
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn mutation_ops(plan: &ExecutionPlan) -> Vec<&MutationOp> {
    plan.pipeline
        .iter()
        .filter_map(|op| match op {
            PipelineOp::Mutation(op) => Some(op),
            _ => None,
        })
        .collect()
}

fn column_names(columns: &[BindingTableColumn]) -> Vec<&str> {
    columns
        .iter()
        .filter_map(|column| column.name.as_ref().map(|name| name.as_str()))
        .collect()
}

#[test]
fn insert_anonymous_edge_endpoints_use_insert_site_refs() {
    let plan = plan_one("INSERT (:A)-[:E]->(:B)");
    let ops = mutation_ops(&plan);
    assert_eq!(ops.len(), 3);
    let MutationOp::InsertNode {
        site_id: left_id,
        binding: None,
        ..
    } = ops[0]
    else {
        panic!("expected anonymous left node");
    };
    let MutationOp::InsertNode {
        site_id: right_id,
        binding: None,
        ..
    } = ops[1]
    else {
        panic!("expected anonymous right node");
    };
    let MutationOp::InsertEdge {
        left,
        right,
        site_id: edge_id,
        ..
    } = ops[2]
    else {
        panic!("expected inserted edge");
    };
    assert_eq!(*left, InsertEndpointRef::InsertedNode(*left_id));
    assert_eq!(*right, InsertEndpointRef::InsertedNode(*right_id));
    assert_eq!(left_id.raw(), 0);
    assert_eq!(edge_id.raw(), 1);
    assert_eq!(right_id.raw(), 2);
}

#[test]
fn insert_return_projects_named_binding() {
    let plan = plan_one("INSERT (n:Person {name: 'Alice'}) RETURN n");
    let ops = mutation_ops(&plan);
    assert_eq!(ops.len(), 1);
    let MutationOp::InsertNode {
        binding,
        property_inits,
        ..
    } = ops[0]
    else {
        panic!("expected insert node");
    };
    assert!(binding.is_some());
    assert_eq!(property_inits.len(), 1);
    assert!(matches!(plan.pipeline.last(), Some(PipelineOp::Project(_))));
    assert_eq!(column_names(&plan.output_schema.columns), ["n"]);
}

#[test]
fn reused_insert_node_is_skipped_but_remains_endpoint_binding() {
    let plan = plan_one("INSERT (a) INSERT (a)-[:K]->(b) RETURN *");
    let ops = mutation_ops(&plan);
    assert_eq!(ops.len(), 3);
    let MutationOp::InsertEdge { left, right, .. } = ops[2] else {
        panic!("expected middle edge");
    };
    assert!(matches!(left, InsertEndpointRef::Binding { .. }));
    assert!(matches!(right, InsertEndpointRef::Binding { .. }));
    assert_eq!(column_names(&plan.output_schema.columns), ["a", "b"]);
}

#[test]
fn set_remove_and_delete_lower_to_mutation_ops() {
    let plan = plan_one("MATCH (n) SET n.age = 30, n :Active REMOVE n.old DELETE n");
    let names = mutation_ops(&plan)
        .iter()
        .map(|op| match op {
            MutationOp::SetProperty { .. } => "SetProperty",
            MutationOp::SetLabel { .. } => "SetLabel",
            MutationOp::RemoveProperty { .. } => "RemoveProperty",
            MutationOp::DeleteTargets { .. } => "DeleteTargets",
            other => panic!("unexpected op {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["SetProperty", "SetLabel", "RemoveProperty", "DeleteTargets"]
    );
}

#[test]
fn property_merge_emits_one_set_property_per_key() {
    let plan = plan_one("MATCH (n) SET n = {age: 30, active: true}");
    let ops = mutation_ops(&plan);
    assert_eq!(ops.len(), 2);
    assert!(
        ops.iter()
            .all(|op| matches!(op, MutationOp::SetProperty { .. }))
    );
}

#[test]
fn detach_delete_edge_binding_preserves_mode_and_element() {
    let plan = plan_one("MATCH (a)-[e]->(b) DETACH DELETE e");
    let ops = mutation_ops(&plan);
    let MutationOp::DeleteTargets { mode, targets, .. } = ops[0] else {
        panic!("expected delete targets");
    };
    assert_eq!(*mode, selene_gql::DeleteMode::Detach);
    assert_eq!(targets.len(), 1);
}

#[test]
fn delete_item_list_lowers_to_one_set_oriented_op() {
    let plan = plan_one("MATCH (a)-[e]->(b) DELETE a, e");
    let ops = mutation_ops(&plan);
    let MutationOp::DeleteTargets { targets, mode, .. } = ops[0] else {
        panic!("expected delete targets");
    };
    assert_eq!(*mode, selene_gql::DeleteMode::Bare);
    assert_eq!(targets.len(), 2);
}

#[test]
fn finish_and_missing_terminator_have_empty_output_schema() {
    for source in ["INSERT (n:Person) FINISH", "INSERT (n:Person)"] {
        let plan = plan_one(source);
        assert!(plan.output_schema.columns.is_empty(), "{source}");
        assert!(matches!(
            plan.pipeline.first(),
            Some(PipelineOp::Mutation(MutationOp::InsertNode { .. }))
        ));
    }
}

// Immutable analyzer output cannot be corrupted by external callers. The
// missing-write-set defensive case is exercised by the adapter's unit test
// `missing_write_set_reports_planner_error` instead.

#[test]
fn sentinel_mutation_plan_shape_snapshot() {
    let plan = plan_one("INSERT (:A)-[:E]->(:B)");
    let summary = mutation_ops(&plan)
        .iter()
        .map(|op| match op {
            MutationOp::InsertNode {
                site_id, binding, ..
            } => format!("node:{}:binding={}", site_id.raw(), binding.is_some()),
            MutationOp::InsertEdge {
                site_id,
                left,
                right,
                ..
            } => format!("edge:{}:{left:?}:{right:?}", site_id.raw()),
            _ => "other".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(summary, @r###"
node:0:binding=false
node:2:binding=false
edge:1:InsertedNode(InsertSiteId(0)):InsertedNode(InsertSiteId(2))
"###);
}

proptest::proptest! {
    #[test]
    fn accepted_mutation_corpus_plans(source in proptest::sample::select(vec![
        "INSERT (n:Person)",
        "INSERT (:Person)",
        "INSERT (:A)-[:E]->(:B)",
        "MATCH (n) SET n.age = 30",
        "MATCH (n) REMOVE n.age",
        "MATCH (n) DELETE n",
        "MATCH (n) DETACH DELETE n",
    ])) {
        let plan = plan_one(source);
        proptest::prop_assert!(
            plan.pipeline.iter().any(|op| matches!(op, PipelineOp::Mutation(_)))
        );
    }
}
