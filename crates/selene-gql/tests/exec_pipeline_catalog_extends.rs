//! EXTENDS catalog composition coverage.

mod exec_common;

#[path = "../../selene-graph/tests/format2_support/mod.rs"]
mod format2_support;
use selene_core::{Change, GraphId, PropertyValueType, SchemaChange};
use selene_gql::{
    Binding, BindingTable, BindingTableSchema, EmptyProcedureRegistry, ExecutionPlan,
    ExecutorError, TxContext, analyze, execute_pipeline, feature_walk, parse, plan,
};
use selene_graph::{
    CommitOutcome, EdgeEndpointDef, GraphTypeDef, PropertyDefaultValue, SharedGraph,
};
use selene_profile::{CapabilityStatus, FeatureId, capability};

use exec_common::db_string;

fn planned(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn seed_table() -> BindingTable {
    BindingTable::new(
        BindingTableSchema {
            columns: Vec::new(),
        },
        vec![Binding::empty()],
    )
}

fn empty_closed_graph(id: u64) -> SharedGraph {
    SharedGraph::builder(GraphId::new(id))
        .bound_to(empty_graph_type())
        .unwrap()
        .build()
        .unwrap()
}

fn empty_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("catalog.extends.graph"),
        node_types: Vec::new(),
        edge_types: Vec::new(),
    }
}

fn run_write(
    graph: &SharedGraph,
    plan: &ExecutionPlan,
) -> Result<(BindingTable, CommitOutcome), ExecutorError> {
    let snapshot = graph.read();
    let mut txn = graph.begin_write();
    let result = {
        let mut ctx = TxContext::write(
            snapshot,
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            &mut txn,
            graph.index_providers(),
        );
        execute_pipeline(&plan.pipeline, seed_table(), &mut ctx)
    };
    match result {
        Ok(table) => Ok((table, txn.commit().expect("commit succeeds"))),
        Err(error) => {
            txn.rollback();
            Err(error)
        }
    }
}

fn plan_with_prefix(child: &str, prefixes: &[&str]) -> ExecutionPlan {
    let mut plan = planned(child);
    for prefix in prefixes.iter().rev() {
        let op = planned(prefix).pipeline.remove(0);
        plan.pipeline.insert(0, op);
    }
    plan
}

fn node_property_names(graph: &SharedGraph, name: &str) -> Vec<String> {
    let graph_type = graph.graph_type().expect("closed graph type");
    let node_type = graph_type
        .node_types
        .iter()
        .find(|node_type| node_type.name.as_str() == name)
        .expect("node type exists");
    node_type
        .properties
        .iter()
        .map(|property| property.name.as_str().to_owned())
        .collect()
}

fn edge_property_names(graph: &SharedGraph, name: &str) -> Vec<String> {
    let graph_type = graph.graph_type().expect("closed graph type");
    let edge_type = graph_type
        .edge_types
        .iter()
        .find(|edge_type| edge_type.name.as_str() == name)
        .expect("edge type exists");
    edge_type
        .properties
        .iter()
        .map(|property| property.name.as_str().to_owned())
        .collect()
}

fn graph_type_violation(source: &str, prefixes: &[&str]) -> String {
    let graph = empty_closed_graph(13_900);
    let plan = plan_with_prefix(source, prefixes);
    let err = run_write(&graph, &plan).expect_err("statement rejects");
    let ExecutorError::GraphTypeViolation { message, .. } = err else {
        panic!("expected GraphTypeViolation");
    };
    message
}

#[test]
fn node_extends_composes_parent_then_child_properties() {
    let graph = empty_closed_graph(13_901);
    let plan = plan_with_prefix(
        "CREATE NODE TYPE :Child EXTENDS :Parent (c :: STRING)",
        &["CREATE NODE TYPE :Parent (a :: INT, b :: STRING)"],
    );

    let (_table, outcome) = run_write(&graph, &plan).expect("catalog executes");

    assert_eq!(node_property_names(&graph, "Child"), ["a", "b", "c"]);
    assert!(matches!(
        outcome.changes.as_slice(),
        [
            Change::SchemaChanged {
                change: SchemaChange::NodeTypeAddedV2 { label: parent, .. },
                ..
            },
            Change::SchemaChanged {
                change: SchemaChange::NodeTypeAddedV2 { label: child, .. },
                ..
            }
        ] if parent.as_str() == "Parent" && child.as_str() == "Child"
    ));
}

#[test]
fn node_extends_allows_parent_only_child_only_and_multilevel_shapes() {
    let parent_only = empty_closed_graph(13_902);
    let plan = plan_with_prefix(
        "CREATE NODE TYPE :Child EXTENDS :Parent ()",
        &["CREATE NODE TYPE :Parent (a :: INT, b :: STRING)"],
    );
    run_write(&parent_only, &plan).expect("parent-only compose succeeds");
    assert_eq!(node_property_names(&parent_only, "Child"), ["a", "b"]);

    let child_only = empty_closed_graph(13_903);
    let plan = planned("CREATE NODE TYPE :Child (c :: STRING)");
    run_write(&child_only, &plan).expect("child-only create succeeds");
    assert_eq!(node_property_names(&child_only, "Child"), ["c"]);

    let multilevel = empty_closed_graph(13_904);
    let plan = plan_with_prefix(
        "CREATE NODE TYPE :C EXTENDS :B (c :: STRING)",
        &[
            "CREATE NODE TYPE :A (a :: INT)",
            "CREATE NODE TYPE :B EXTENDS :A (b :: STRING)",
        ],
    );
    run_write(&multilevel, &plan).expect("multi-level compose succeeds");
    assert_eq!(node_property_names(&multilevel, "C"), ["a", "b", "c"]);
}

#[test]
fn exact_match_redeclaration_succeeds_without_duplicate_property() {
    let graph = empty_closed_graph(13_905);
    let plan = plan_with_prefix(
        "CREATE NODE TYPE :Child EXTENDS :Parent (a :: INT NOT NULL UNIQUE)",
        &["CREATE NODE TYPE :Parent (a :: INT NOT NULL UNIQUE)"],
    );

    run_write(&graph, &plan).expect("matching redeclaration succeeds");

    assert_eq!(node_property_names(&graph, "Child"), ["a"]);
}

#[test]
fn property_conflicts_name_each_mismatched_field() {
    let cases = [
        (
            "CREATE NODE TYPE :Parent (a :: INT)",
            "CREATE NODE TYPE :Child EXTENDS :Parent (a :: STRING)",
            "property 'a' redeclared with different value type (parent: INTEGER, child: STRING) on child type Child",
        ),
        (
            "CREATE NODE TYPE :Parent (a :: INT NOT NULL)",
            "CREATE NODE TYPE :Child EXTENDS :Parent (a :: INT)",
            "property 'a' redeclared with different NOT NULL constraint (parent: true, child: false) on child type Child",
        ),
        (
            "CREATE NODE TYPE :Parent (a :: INT DEFAULT 0)",
            "CREATE NODE TYPE :Child EXTENDS :Parent (a :: INT DEFAULT 1)",
            "property 'a' redeclared with different DEFAULT value (parent: 0, child: 1) on child type Child",
        ),
        (
            "CREATE NODE TYPE :Parent (a :: INT IMMUTABLE)",
            "CREATE NODE TYPE :Child EXTENDS :Parent (a :: INT)",
            "property 'a' redeclared with different IMMUTABLE constraint (parent: true, child: false) on child type Child",
        ),
        (
            "CREATE NODE TYPE :Parent (a :: INT UNIQUE)",
            "CREATE NODE TYPE :Child EXTENDS :Parent (a :: INT)",
            "property 'a' redeclared with different UNIQUE constraint (parent: true, child: false) on child type Child",
        ),
        (
            "CREATE NODE TYPE :Parent (a :: LIST<INT>)",
            "CREATE NODE TYPE :Child EXTENDS :Parent (a :: LIST<STRING>)",
            "property 'a' redeclared with different list element type (parent: INTEGER, child: STRING) on child type Child",
        ),
        (
            "CREATE NODE TYPE :Parent (a :: STRING(2, 4))",
            "CREATE NODE TYPE :Child EXTENDS :Parent (a :: VARCHAR(4))",
            "property 'a' redeclared with different character-string length (parent: STRING(2, 4), child: STRING(4)) on child type Child",
        ),
        (
            "CREATE NODE TYPE :Parent (a :: BYTES(2))",
            "CREATE NODE TYPE :Child EXTENDS :Parent (a :: BYTES(4))",
            "property 'a' redeclared with different byte-string length (parent: BYTES(2), child: BYTES(4)) on child type Child",
        ),
    ];

    for (parent, child, expected) in cases {
        assert_eq!(graph_type_violation(child, &[parent]), expected);
    }
}

#[test]
fn missing_parent_and_cross_kind_extends_use_graph_type_violation() {
    assert_eq!(
        graph_type_violation("CREATE NODE TYPE :Child EXTENDS :Missing ()", &[]),
        "CREATE NODE TYPE :Child EXTENDS :Missing - parent node type 'Missing' is not declared in this graph"
    );
    assert_eq!(
        graph_type_violation("CREATE EDGE TYPE :Child EXTENDS :Missing ()", &[]),
        "CREATE EDGE TYPE :Child EXTENDS :Missing - parent edge type 'Missing' is not declared in this graph"
    );
    assert_eq!(
        graph_type_violation(
            "CREATE NODE TYPE :Child EXTENDS :Base ()",
            &["CREATE EDGE TYPE :Base ()"],
        ),
        "CREATE NODE TYPE :Child EXTENDS :Base - parent 'Base' is an edge type; node types may only extend node types"
    );
    assert_eq!(
        graph_type_violation(
            "CREATE EDGE TYPE :Child EXTENDS :Base ()",
            &["CREATE NODE TYPE :Base ()"],
        ),
        "CREATE EDGE TYPE :Child EXTENDS :Base - parent 'Base' is a node type; edge types may only extend edge types"
    );
}

#[test]
fn edge_extends_composes_properties_without_inheriting_endpoints() {
    let graph = empty_closed_graph(13_906);
    let plan = plan_with_prefix(
        "CREATE EDGE TYPE :Child EXTENDS :Base (weight :: INT)",
        &[
            "CREATE NODE TYPE :Person ()",
            "CREATE EDGE TYPE :Base (FROM :Person TO :Person, since :: DATE)",
        ],
    );

    run_write(&graph, &plan).expect("edge composition succeeds");

    assert_eq!(edge_property_names(&graph, "Child"), ["since", "weight"]);
    let graph_type = graph.graph_type().unwrap();
    let child = graph_type
        .edge_types
        .iter()
        .find(|edge_type| edge_type.name.as_str() == "Child")
        .unwrap();
    assert_eq!(child.source_node_type, EdgeEndpointDef::Any);
    assert_eq!(child.target_node_type, EdgeEndpointDef::Any);
}

#[test]
fn mnemosyne_memory_base_case_composes_universal_block() {
    let graph = empty_closed_graph(13_907);
    let plan = plan_with_prefix(
        "CREATE NODE TYPE :Episode EXTENDS :Memory (kind :: STRING NOT NULL)",
        &[
            "CREATE NODE TYPE :Memory (id :: STRING, ingested_at :: STRING, namespace :: STRING, importance :: INT, trust :: INT, expired_at :: STRING)",
        ],
    );

    run_write(&graph, &plan).expect("mnemosyne-style composition succeeds");

    assert_eq!(
        node_property_names(&graph, "Episode"),
        [
            "id",
            "ingested_at",
            "namespace",
            "importance",
            "trust",
            "expired_at",
            "kind"
        ]
    );
}

#[test]
fn composed_node_type_survives_format2_snapshot() {
    let graph_id = GraphId::new(13_908);
    let base = empty_graph_type();
    let graph = SharedGraph::builder(graph_id)
        .bound_to(base.clone())
        .unwrap()
        .build()
        .unwrap();
    let plan = plan_with_prefix(
        "CREATE NODE TYPE :Child EXTENDS :Parent (c :: STRING)",
        &["CREATE NODE TYPE :Parent (a :: INT)"],
    );

    run_write(&graph, &plan).expect("catalog executes");
    let recovered = format2_support::snapshot(&graph.read()).unwrap();
    assert_eq!(node_property_names(&recovered, "Child"), ["a", "c"]);
}

#[test]
fn flagger_records_im_extends_and_feature_is_supported() {
    assert!(
        capability(FeatureId::IM_EXTENDS)
            .is_some_and(|record| record.status == CapabilityStatus::Supported)
    );

    for source in [
        "CREATE NODE TYPE :Child EXTENDS :Parent ()",
        "CREATE EDGE TYPE :Child EXTENDS :Parent ()",
    ] {
        let statement = parse(source).expect("statement parses");
        let observed = feature_walk(&statement)
            .into_iter()
            .map(|feature| feature.feature_id)
            .collect::<Vec<_>>();
        assert!(
            observed.contains(&FeatureId::IM_EXTENDS),
            "{source} should record IM_EXTENDS, observed {observed:?}"
        );
    }

    let statement = parse("CREATE NODE TYPE :Child ()").expect("statement parses");
    assert!(
        !feature_walk(&statement)
            .into_iter()
            .any(|feature| feature.feature_id == FeatureId::IM_EXTENDS)
    );
}

#[test]
fn composed_property_fields_preserve_defaults_and_value_types() {
    let graph = empty_closed_graph(13_909);
    let plan = plan_with_prefix(
        "CREATE NODE TYPE :Child EXTENDS :Parent (child :: STRING)",
        &["CREATE NODE TYPE :Parent (a :: INT DEFAULT 7, name :: STRING IMMUTABLE)"],
    );

    run_write(&graph, &plan).expect("composition succeeds");

    let graph_type = graph.graph_type().unwrap();
    let child = graph_type
        .node_types
        .iter()
        .find(|node_type| node_type.name.as_str() == "Child")
        .unwrap();
    let a = &child.properties[0];
    assert_eq!(a.value_type, PropertyValueType::Int);
    assert_eq!(a.default, Some(PropertyDefaultValue::Integer(7)));
    let name = &child.properties[1];
    assert_eq!(name.value_type, PropertyValueType::String);
    assert!(name.immutable);
}
