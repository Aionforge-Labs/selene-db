//! 813 — Feature GG21 "Explicit element type key label sets" (ISO/IEC
//! 39075:2024 §18.2/18.3).
//!
//! The generated Flagger disposition accepts GG21's directly selected
//! *singleton* explicit-key-label-set surface (Option A), independently of its
//! unsupported complete runtime status. The type-DDL grammar parses the
//! explicit `<...type key label set>` (`[ <label set phrase> ] <implies>`, the
//! symbolic `=>` marker), the feature walk stamps GG21, and the IL003
//! key-label-set cardinality cap is fixed at 1 (singleton). A cardinality other than 1 is
//! rejected with the spec-defined GQLSTATUS (42012/42013 for nodes, 42014/42015
//! for edges); the separate-implied-label-set shape (`:Key => :Implied`) is an
//! honest `FEATURE_NOT_SUPPORTED` (42N01), not a silent mis-identification.
//!
//! Because the singleton key label set equals the type-name label set, the
//! explicit form is *observationally identical* to the implied (bare `:Name`)
//! form: same `GraphTypeDef`, same WAL `SchemaChange`, same exact-equality
//! element-type identification — so the GTYP snapshot + WAL replay round-trip is
//! byte-identical (no format change).

#[path = "../../selene-graph/tests/format2_support/mod.rs"]
mod format2_support;
use selene_core::{Change, GraphId, LabelSet, SchemaChange};
use selene_gql::{
    Binding, BindingTable, BindingTableSchema, EmptyProcedureRegistry, ExecutionPlan,
    ExecutorError, GqlStatus, TxContext, analyze, execute_pipeline, parse, plan,
};
use selene_graph::{GraphError, GraphTypeDef, SharedGraph};

fn planned(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

/// Plan `source` and return the planner error (used for the IL003 / deferred
/// rejects which fire at plan time, before any catalog write).
fn plan_err(source: &str) -> GqlStatus {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry)
        .map(|_| ())
        .expect_err("expected a plan-time rejection")
        .gqlstatus()
}

fn base_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: selene_core::db_string("gg21.test.graph").unwrap(),
        node_types: Vec::new(),
        edge_types: Vec::new(),
    }
}

fn empty_closed_graph(id: u64) -> SharedGraph {
    SharedGraph::builder(GraphId::new(id))
        .bound_to(base_graph_type())
        .unwrap()
        .build()
        .unwrap()
}

fn person_closed_graph(id: u64) -> SharedGraph {
    let person = selene_core::db_string("Person").unwrap();
    SharedGraph::builder(GraphId::new(id))
        .bound_to(GraphTypeDef {
            name: selene_core::db_string("gg21.person.graph").unwrap(),
            node_types: vec![selene_graph::NodeTypeDef {
                name: person.clone(),
                key_labels: LabelSet::single(person),
                properties: Vec::new(),
                validation_mode: selene_graph::ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        })
        .unwrap()
        .build()
        .unwrap()
}

fn run_write(
    graph: &SharedGraph,
    plan: &ExecutionPlan,
) -> Result<
    (
        BindingTable,
        Result<selene_graph::CommitOutcome, GraphError>,
    ),
    ExecutorError,
> {
    let snapshot = graph.read();
    let mut txn = graph.begin_write();
    let seed = BindingTable::new(
        BindingTableSchema {
            columns: Vec::new(),
        },
        vec![Binding::empty()],
    );
    let result = {
        let mut ctx = TxContext::write(
            snapshot,
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            &mut txn,
            graph.index_providers(),
        );
        execute_pipeline(&plan.pipeline, seed, &mut ctx)
    };
    match result {
        Ok(table) => Ok((table, txn.commit())),
        Err(error) => {
            txn.rollback();
            Err(error)
        }
    }
}

// --- parse + plan acceptance -------------------------------------------------

#[test]
fn explicit_singleton_key_label_set_parses_and_plans() {
    // `:Person => (...)` is the accepted singleton explicit key label set; it
    // must parse and plan exactly like the bare `:Person (...)` form.
    planned("CREATE NODE TYPE :Person => (name :: STRING)");
    planned("CREATE EDGE TYPE :KNOWS => (since :: INTEGER)");
    // Property-types content under `=>` (no separate implied label) is supported.
    planned("CREATE NODE TYPE :Account => ()");
}

#[test]
fn implies_keyword_is_accepted_like_the_symbolic_form() {
    // ISO §21.3: `<implies> ::= <right double arrow> | IMPLIES`. Both spellings
    // must parse, plan, and commit identically.
    let (keyword_type, keyword_changes) =
        commit_node_type(3820, "CREATE NODE TYPE :Person IMPLIES (name :: STRING)");
    let (symbol_type, symbol_changes) =
        commit_node_type(3821, "CREATE NODE TYPE :Person => (name :: STRING)");
    assert_eq!(
        keyword_type, symbol_type,
        "`IMPLIES` keyword and `=>` must yield the same bound type"
    );
    assert_eq!(keyword_changes, symbol_changes);

    // Case-insensitive in key-label-set position. A type/label literally named
    // `implies` must be delimited because IMPLIES is globally reserved by
    // ISO §21.3.
    planned("CREATE EDGE TYPE :KNOWS implies (since :: INTEGER)");
    planned("CREATE NODE TYPE :\"implies\" (x :: INT)");

    let err =
        parse("CREATE NODE TYPE :implies (x :: INT)").expect_err("bare IMPLIES label must reject");
    assert!(
        matches!(err, selene_gql::ParserError::SyntaxError { .. }),
        "expected SyntaxError for bare IMPLIES label, got {err:?}"
    );
}

// --- IL003 cardinality rejects (the spec-defined GQLSTATUS) ------------------

#[test]
fn multi_label_node_key_label_set_rejects_42013() {
    // Cardinality > 1 (`:A & :B =>`) exceeds the IL003 node maximum (1) — ISO
    // §18.2 SR11 raises 42013.
    assert_eq!(
        plan_err("CREATE NODE TYPE :A & :B => (name :: STRING)"),
        GqlStatus::NODE_TYPE_KEY_LABELS_EXCEED_MAXIMUM
    );
    assert_eq!(
        plan_err("CREATE NODE TYPE :A & :B => (name :: STRING)").as_str(),
        "42013"
    );
}

#[test]
fn multi_label_edge_key_label_set_rejects_42015() {
    // Cardinality > 1 exceeds the IL003 edge maximum (1) — ISO §18.3 SR12 raises
    // 42015.
    assert_eq!(
        plan_err("CREATE EDGE TYPE :A & :B => (since :: INTEGER)"),
        GqlStatus::EDGE_TYPE_KEY_LABELS_EXCEED_MAXIMUM
    );
    assert_eq!(
        plan_err("CREATE EDGE TYPE :A & :B => (since :: INTEGER)").as_str(),
        "42015"
    );
}

#[test]
fn empty_node_key_label_set_rejects_42012() {
    // The bare `<implies>` with no `<label set phrase>` is the empty key label
    // set (§18.2 SR5b, cardinality 0), below the IL003 minimum (1) — ISO §18.2
    // SR10 raises 42012.
    assert_eq!(
        plan_err("CREATE NODE TYPE => (name :: STRING)"),
        GqlStatus::NODE_TYPE_KEY_LABELS_BELOW_MINIMUM
    );
    assert_eq!(
        plan_err("CREATE NODE TYPE => (name :: STRING)").as_str(),
        "42012"
    );
}

#[test]
fn empty_edge_key_label_set_rejects_42014() {
    // Empty edge key label set (cardinality 0) below the IL003 minimum (1) — ISO
    // §18.3 SR11 raises 42014.
    assert_eq!(
        plan_err("CREATE EDGE TYPE => (since :: INTEGER)"),
        GqlStatus::EDGE_TYPE_KEY_LABELS_BELOW_MINIMUM
    );
    assert_eq!(
        plan_err("CREATE EDGE TYPE => (since :: INTEGER)").as_str(),
        "42014"
    );
}

// --- deferred separate-implied-label-set (honest reject) ---------------------

#[test]
fn separate_implied_label_set_is_feature_not_supported() {
    // `:Person => :Employee` is a key label set followed by a SEPARATE implied
    // label set (§18.2 SR7/SR8 — the element-type label set is the union,
    // requiring containment identification). selene-db defers this; it must be
    // an honest FEATURE_NOT_SUPPORTED (42N01), NOT a silent mis-identification.
    assert_eq!(
        plan_err("CREATE NODE TYPE :Person => :Employee (name :: STRING)"),
        GqlStatus::FEATURE_NOT_SUPPORTED
    );
    assert_eq!(
        plan_err("CREATE EDGE TYPE :KNOWS => :LINKS (since :: INTEGER)"),
        GqlStatus::FEATURE_NOT_SUPPORTED
    );
}

// --- exact-equality identification + round-trip equivalence ------------------

/// Commit `source` against a freshly-bound empty closed graph and return the
/// committed `GraphTypeDef` plus the emitted `SchemaChange`s.
///
/// The graph-id-carrying `Change` envelope is unwrapped to its inner
/// `SchemaChange` so two commits against distinct test graphs can be compared
/// for payload (GTYP/WAL content) equality.
fn commit_node_type(id: u64, source: &str) -> (GraphTypeDef, Vec<SchemaChange>) {
    let graph = empty_closed_graph(id);
    let plan = planned(source);
    let (_table, outcome) = run_write(&graph, &plan).expect("catalog executes");
    let outcome = outcome.expect("commit succeeds");
    let schema_changes = outcome.changes.iter().map(schema_change).collect();
    ((*graph.graph_type().unwrap()).clone(), schema_changes)
}

/// Unwrap a `Change` to its `SchemaChange` payload (the WAL/GTYP content,
/// excluding the graph-id envelope).
fn schema_change(change: &Change) -> SchemaChange {
    match change {
        Change::SchemaChanged { change, .. } => change.clone(),
        other => panic!("expected a SchemaChanged change, got {other:?}"),
    }
}

#[test]
fn explicit_singleton_is_identical_to_implied_form() {
    // The accepted explicit-singleton form and the implied (bare `:Name`) form
    // must produce a byte-identical bound graph type AND a byte-identical WAL
    // `SchemaChange` — proving the GTYP snapshot + WAL replay round-trip is
    // identical and the exact-equality element-type identification is preserved.
    let (explicit_type, explicit_changes) =
        commit_node_type(3811, "CREATE NODE TYPE :Person => (name :: STRING)");
    let (implied_type, implied_changes) =
        commit_node_type(3812, "CREATE NODE TYPE :Person (name :: STRING)");

    assert_eq!(
        explicit_type, implied_type,
        "explicit-singleton GG21 bound type must equal the implied GG20 form"
    );
    assert_eq!(
        explicit_changes, implied_changes,
        "the WAL SchemaChange must be identical for the explicit and implied forms"
    );

    // The committed key label set is the singleton `:Person`, so exact-equality
    // identification resolves it (G6 unchanged).
    let person = LabelSet::single(selene_core::db_string("Person").unwrap());
    assert_eq!(explicit_type.node_types[0].key_labels, person);
    assert!(explicit_type.find_node_type(&person).is_some());
    assert!(matches!(
        explicit_changes.as_slice(),
        [SchemaChange::NodeTypeAddedV2 { label, .. }] if label.as_str() == "Person"
    ));
}

#[test]
fn explicit_singleton_edge_is_identical_to_implied_form() {
    let explicit = person_closed_graph(3813);
    let implied = person_closed_graph(3814);

    let (_t, explicit_outcome) = run_write(
        &explicit,
        &planned("CREATE EDGE TYPE :KNOWS => (FROM :Person TO :Person, since :: INTEGER)"),
    )
    .expect("explicit edge executes");
    let explicit_changes: Vec<SchemaChange> = explicit_outcome
        .expect("commit succeeds")
        .changes
        .iter()
        .map(schema_change)
        .collect();

    let (_t, implied_outcome) = run_write(
        &implied,
        &planned("CREATE EDGE TYPE :KNOWS (FROM :Person TO :Person, since :: INTEGER)"),
    )
    .expect("implied edge executes");
    let implied_changes: Vec<SchemaChange> = implied_outcome
        .expect("commit succeeds")
        .changes
        .iter()
        .map(schema_change)
        .collect();

    assert_eq!(
        explicit.graph_type().unwrap(),
        implied.graph_type().unwrap(),
        "explicit-singleton edge bound type must equal the implied form"
    );
    assert_eq!(
        explicit_changes, implied_changes,
        "the WAL SchemaChange must be identical for the explicit and implied edge forms"
    );
}

#[test]
fn property_types_content_under_implies_is_supported() {
    // `:Person => (name :: STRING)` — property types after `=>` with no extra
    // labels: key_labels == label set, supported, and the bound type carries the
    // declared property.
    let (graph_type, _changes) = commit_node_type(
        3815,
        "CREATE NODE TYPE :Person => (name :: STRING NOT NULL)",
    );
    assert_eq!(graph_type.node_types[0].name.as_str(), "Person");
    assert_eq!(graph_type.node_types[0].properties[0].name.as_str(), "name");
    assert!(graph_type.node_types[0].properties[0].required);
}

// --- snapshot + WAL recovery round-trip --------------------------------------

#[test]
fn explicit_singleton_survives_format2_snapshot() {
    // The `..._is_identical_to_implied_form` tests above prove the explicit
    // `=>` form's GTYP/WAL *payload* is byte-identical to the implied form; this
    // test proves that payload actually REPLAYS. It drives the real GG21 GQL
    // surface (`CREATE NODE TYPE :Person => (...)`) through execute → WAL →
    // `recover_closed`, then confirms the recovered closed graph carries the
    // singleton key label set that exact-equality identification resolves. (Without
    // this, the module-doc "survives the GTYP snapshot + WAL replay round-trip"
    // claim was inferred from byte-identity, never demonstrated end-to-end.)
    let graph_id = GraphId::new(3816);
    let base = base_graph_type();
    let graph = SharedGraph::builder(graph_id)
        .bound_to(base.clone())
        .unwrap()
        .build()
        .unwrap();

    let plan = planned("CREATE NODE TYPE :Person => (name :: STRING)");
    let (_table, outcome) = run_write(&graph, &plan).expect("catalog executes");
    outcome.expect("commit succeeds");
    let recovered = format2_support::snapshot(&graph.read()).unwrap();
    let graph_type = recovered.graph_type().expect("recovered closed graph type");
    let person = LabelSet::single(selene_core::db_string("Person").unwrap());

    assert_eq!(
        graph_type.node_types.len(),
        1,
        "the explicit GG21 type must survive WAL replay"
    );
    assert_eq!(
        graph_type.node_types[0].key_labels, person,
        "the recovered singleton key label set must equal the implied `:Person` form"
    );
    assert!(
        graph_type.find_node_type(&person).is_some(),
        "exact-equality identification (G6 unchanged) must resolve the recovered key label set"
    );
    assert_eq!(graph_type.node_types[0].properties[0].name.as_str(), "name");
}
