//! End-to-end coverage for maintained candidate-state vector built-ins.

use std::sync::Arc;

use selene_core::{Change, DbString, GraphId, LabelSet, NodeId, PropertyMap, Value, VectorValue};
use selene_gql::{
    BindingTable, BuiltinProcedureRegistry, ExecutorError, ProcedureError, ProcedureRegistry,
    Session, StatementOutput,
};
use selene_graph::{
    CANDIDATE_STATE_PROVIDER_TAG, CandidateStateSpec, IndexProvider,
    MaintainedCandidateStateProvider, ProviderError, ProviderTag, SharedGraph, SubTag,
    VectorCandidateStateInfo,
};

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn vector(components: &[f32]) -> VectorValue {
    VectorValue::new(components.to_vec()).expect("test vector is valid")
}

fn props(key: &DbString, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(key.clone(), value)]).expect("test property map is valid")
}

fn node_list(nodes: &[NodeId]) -> Value {
    Value::List(nodes.iter().copied().map(Value::NodeRef).collect())
}

fn rows(output: StatementOutput) -> BindingTable {
    match output {
        StatementOutput::Rows(table) => table,
        StatementOutput::Written(outcome) => outcome.rows.expect("written statement returned rows"),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn execute_rows(
    session: &mut Session<'_>,
    source: &str,
    registry: &dyn ProcedureRegistry,
) -> BindingTable {
    rows(
        session
            .execute_source(source, registry)
            .expect("statement executes"),
    )
}

fn node_column(table: &BindingTable, name: &str) -> Vec<NodeId> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::NodeRef(value)) => *value,
            other => panic!("expected node ref in {name}, got {other:?}"),
        })
        .collect()
}

fn string_column(table: &BindingTable, name: &str) -> Vec<String> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::String(value)) => value.as_str().to_owned(),
            Some(Value::Null) => "NULL".to_owned(),
            other => panic!("expected string in {name}, got {other:?}"),
        })
        .collect()
}

fn uint_column(table: &BindingTable, name: &str) -> Vec<u64> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::Uint(value)) => *value,
            other => panic!("expected uint in {name}, got {other:?}"),
        })
        .collect()
}

fn string_list_column(table: &BindingTable, name: &str) -> Vec<Vec<String>> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::List(values)) => values
                .iter()
                .map(|value| match value {
                    Value::String(value) => value.as_str().to_owned(),
                    other => panic!("expected string list item in {name}, got {other:?}"),
                })
                .collect(),
            other => panic!("expected list in {name}, got {other:?}"),
        })
        .collect()
}

fn candidate_graph() -> (SharedGraph, Vec<NodeId>) {
    let state_name = db_string("active_docs");
    let doc = db_string("VectorDoc");
    let superseded = db_string("SUPERSEDED_BY");
    let supports = db_string("SUPPORTS");
    let provider = Arc::new(
        MaintainedCandidateStateProvider::new([CandidateStateSpec::new(state_name)
            .require_label(doc.clone())
            .exclude_outgoing(superseded.clone())])
        .expect("provider config is valid"),
    );
    let graph = SharedGraph::builder(GraphId::new(330_401))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .expect("graph builds");
    let embedding = db_string("embedding");
    let mut txn = graph.begin_write();
    let mut mutator = txn.mutator();
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            mutator
                .create_node(
                    LabelSet::single(doc.clone()),
                    props(&embedding, Value::Vector(vector(&[i as f32, 0.0]))),
                )
                .expect("vector node inserts"),
        );
    }
    mutator
        .create_edge(superseded, ids[4], ids[0], PropertyMap::new())
        .expect("stale edge inserts");
    mutator
        .create_edge(supports.clone(), ids[0], ids[2], PropertyMap::new())
        .expect("support edge inserts");
    mutator
        .create_edge(supports, ids[1], ids[4], PropertyMap::new())
        .expect("stale support edge inserts");
    txn.commit().expect("seed commits");
    (graph, ids)
}

#[test]
fn vector_score_candidate_state_reranks_maintained_set() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[2.2, 0.0])));

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state('embedding', $query, 'active_docs', 3) \
         YIELD node_id, distance",
        &registry,
    );

    assert_eq!(node_column(&table, "node_id"), vec![ids[2], ids[3], ids[1]]);
}

#[test]
fn vector_score_candidate_state_nodes_intersects_state_with_nodes_by_default() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[2.2, 0.0])));
    session.bind_parameter(db_string("nodes"), node_list(&[ids[0], ids[2], ids[4]]));

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state_nodes('embedding', $query, \
         'active_docs', $nodes, 3) YIELD node_id, distance",
        &registry,
    );

    assert_eq!(node_column(&table, "node_id"), vec![ids[2], ids[0]]);
}

#[test]
fn vector_score_candidate_state_nodes_supports_explicit_set_algebra() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    session.bind_parameter(db_string("union_query"), Value::Vector(vector(&[4.2, 0.0])));
    session.bind_parameter(db_string("union_nodes"), node_list(&[ids[4]]));
    let union = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state_nodes('embedding', $union_query, \
         'active_docs', $union_nodes, 5, 'union') YIELD node_id, distance",
        &registry,
    );
    assert_eq!(
        node_column(&union, "node_id"),
        vec![ids[4], ids[3], ids[2], ids[1], ids[0]]
    );

    session.bind_parameter(
        db_string("state_diff_query"),
        Value::Vector(vector(&[0.2, 0.0])),
    );
    session.bind_parameter(db_string("state_diff_nodes"), node_list(&[ids[2], ids[3]]));
    let state_difference = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state_nodes('embedding', $state_diff_query, \
         'active_docs', $state_diff_nodes, 5, 'state_difference') YIELD node_id, distance",
        &registry,
    );
    assert_eq!(
        node_column(&state_difference, "node_id"),
        vec![ids[0], ids[1]]
    );

    session.bind_parameter(
        db_string("nodes_diff_query"),
        Value::Vector(vector(&[4.2, 0.0])),
    );
    session.bind_parameter(db_string("nodes_diff_nodes"), node_list(&[ids[2], ids[4]]));
    let nodes_difference = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state_nodes('embedding', $nodes_diff_query, \
         'active_docs', $nodes_diff_nodes, 5, 'nodes_difference') YIELD node_id, distance",
        &registry,
    );
    assert_eq!(node_column(&nodes_difference, "node_id"), vec![ids[4]]);
}

#[test]
fn vector_score_candidate_state_nodes_rejects_unknown_operation() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[0.0, 0.0])));
    session.bind_parameter(db_string("nodes"), node_list(&[ids[0]]));

    let err = session
        .execute_source(
            "CALL selene.vector_score_candidate_state_nodes('embedding', $query, \
             'active_docs', $nodes, 3, 'xor')",
            &registry,
        )
        .expect_err("unknown operation must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("operation must be intersection")
    ));
}

#[test]
fn vector_score_candidate_state_expanded_intersects_state_with_expanded_roots_by_default() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[2.2, 0.0])));
    session.bind_parameter(db_string("roots"), node_list(&[ids[0], ids[1]]));

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state_expanded('embedding', $query, \
         'active_docs', $roots, 'SUPPORTS', 4) YIELD node_id, distance",
        &registry,
    );

    assert_eq!(node_column(&table, "node_id"), vec![ids[2], ids[1], ids[0]]);
}

#[test]
fn vector_score_candidate_state_expanded_batch_intersects_state_per_query() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![
            Value::Vector(vector(&[2.2, 0.0])),
            Value::Vector(vector(&[0.2, 0.0])),
        ]),
    );
    session.bind_parameter(
        db_string("roots"),
        Value::List(vec![node_list(&[ids[0], ids[1]]), node_list(&[ids[0]])]),
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state_expanded_batch('embedding', $queries, \
         'active_docs', $roots, 'SUPPORTS', 4) YIELD query_index, node_id, distance",
        &registry,
    );

    assert_eq!(uint_column(&table, "query_index"), vec![0, 0, 0, 1, 1]);
    assert_eq!(
        node_column(&table, "node_id"),
        vec![ids[2], ids[1], ids[0], ids[0], ids[2]]
    );
}

#[test]
fn vector_score_candidate_state_expanded_supports_explicit_set_algebra() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("roots"), node_list(&[ids[0], ids[1]]));

    session.bind_parameter(db_string("union_query"), Value::Vector(vector(&[4.2, 0.0])));
    let union = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state_expanded('embedding', $union_query, \
         'active_docs', $roots, 'SUPPORTS', 5, 'union') YIELD node_id, distance",
        &registry,
    );
    assert_eq!(
        node_column(&union, "node_id"),
        vec![ids[4], ids[3], ids[2], ids[1], ids[0]]
    );

    session.bind_parameter(db_string("diff_query"), Value::Vector(vector(&[4.2, 0.0])));
    let expanded_difference = execute_rows(
        &mut session,
        "CALL selene.vector_score_candidate_state_expanded('embedding', $diff_query, \
         'active_docs', $roots, 'SUPPORTS', 5, 'expanded_difference') \
         YIELD node_id, distance",
        &registry,
    );
    assert_eq!(node_column(&expanded_difference, "node_id"), vec![ids[4]]);
}

#[test]
fn vector_score_candidate_state_expanded_batch_rejects_mismatched_roots() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![
            Value::Vector(vector(&[0.0, 0.0])),
            Value::Vector(vector(&[1.0, 0.0])),
        ]),
    );
    session.bind_parameter(db_string("roots"), Value::List(vec![node_list(&[ids[0]])]));

    let err = session
        .execute_source(
            "CALL selene.vector_score_candidate_state_expanded_batch('embedding', $queries, \
             'active_docs', $roots, 'SUPPORTS', 3)",
            &registry,
        )
        .expect_err("mismatched query/root batches must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("queries and roots must have the same length")
    ));
}

#[test]
fn vector_score_candidate_state_expanded_rejects_unknown_operation() {
    let (graph, ids) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[0.0, 0.0])));
    session.bind_parameter(db_string("roots"), node_list(&[ids[0]]));

    let err = session
        .execute_source(
            "CALL selene.vector_score_candidate_state_expanded('embedding', $query, \
             'active_docs', $roots, 'SUPPORTS', 3, 'xor')",
            &registry,
        )
        .expect_err("unknown operation must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("operation must be intersection")
    ));
}

#[test]
fn vector_candidate_states_lists_maintained_state_metadata() {
    let (graph, _) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_candidate_states() \
         YIELD state_name, generation, candidate_count, required_label, \
               require_outgoing, require_incoming, exclude_outgoing, exclude_incoming",
        &registry,
    );

    assert_eq!(table.row_count(), 1);
    assert_eq!(string_column(&table, "state_name"), vec!["active_docs"]);
    assert_eq!(
        uint_column(&table, "generation"),
        vec![graph.read().meta.generation]
    );
    assert_eq!(uint_column(&table, "candidate_count"), vec![4]);
    assert_eq!(string_column(&table, "required_label"), vec!["VectorDoc"]);
    assert_eq!(
        string_list_column(&table, "require_outgoing"),
        vec![Vec::<String>::new()]
    );
    assert_eq!(
        string_list_column(&table, "require_incoming"),
        vec![Vec::<String>::new()]
    );
    assert_eq!(
        string_list_column(&table, "exclude_outgoing"),
        vec![vec!["SUPERSEDED_BY".to_owned()]]
    );
    assert_eq!(
        string_list_column(&table, "exclude_incoming"),
        vec![Vec::<String>::new()]
    );
}

#[test]
fn vector_candidate_states_returns_no_rows_without_provider() {
    let graph = SharedGraph::new(GraphId::new(330_403));
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_candidate_states() YIELD state_name",
        &registry,
    );

    assert_eq!(table.row_count(), 0);
}

#[test]
fn vector_score_candidate_state_rejects_unknown_set() {
    let (graph, _) = candidate_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[0.0, 0.0])));

    let err = session
        .execute_source(
            "CALL selene.vector_score_candidate_state('embedding', $query, 'missing', 3)",
            &registry,
        )
        .expect_err("unknown state must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("unknown maintained candidate-state set 'missing'")
    ));
}

#[test]
fn vector_score_candidate_state_surfaces_stale_provider_generation() {
    let provider = Arc::new(StaleCandidateProvider);
    let graph = SharedGraph::builder(GraphId::new(330_402))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .expect("graph builds");
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[0.0, 0.0])));

    let err = session
        .execute_source(
            "CALL selene.vector_score_candidate_state('embedding', $query, 'active_docs', 3)",
            &registry,
        )
        .expect_err("stale provider must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::Internal { ref detail },
            ..
        } if detail.contains("candidate-state provider error")
            && detail.contains("stale candidate state")
    ));
}

#[test]
fn vector_score_candidate_state_nodes_surfaces_stale_provider_generation() {
    let provider = Arc::new(StaleCandidateProvider);
    let graph = SharedGraph::builder(GraphId::new(330_405))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .expect("graph builds");
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.execute_source("INSERT (:Doc)", &registry).unwrap();
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[0.0, 0.0])));
    session.bind_parameter(db_string("nodes"), node_list(&[NodeId::new(1)]));

    let err = session
        .execute_source(
            "CALL selene.vector_score_candidate_state_nodes('embedding', $query, \
             'active_docs', $nodes, 3)",
            &registry,
        )
        .expect_err("stale provider must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::Internal { ref detail },
            ..
        } if detail.contains("candidate-state provider error")
            && detail.contains("stale candidate state")
    ));
}

#[test]
fn vector_score_candidate_state_expanded_surfaces_stale_provider_generation() {
    let provider = Arc::new(StaleCandidateProvider);
    let graph = SharedGraph::builder(GraphId::new(330_406))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .expect("graph builds");
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.execute_source("INSERT (:Doc)", &registry).unwrap();
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[0.0, 0.0])));
    session.bind_parameter(db_string("roots"), node_list(&[NodeId::new(1)]));

    let err = session
        .execute_source(
            "CALL selene.vector_score_candidate_state_expanded('embedding', $query, \
             'active_docs', $roots, 'SUPPORTS', 3)",
            &registry,
        )
        .expect_err("stale provider must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::Internal { ref detail },
            ..
        } if detail.contains("candidate-state provider error")
            && detail.contains("stale candidate state")
    ));
}

#[test]
fn vector_score_candidate_state_expanded_batch_surfaces_stale_provider_generation() {
    let provider = Arc::new(StaleCandidateProvider);
    let graph = SharedGraph::builder(GraphId::new(330_407))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .expect("graph builds");
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.execute_source("INSERT (:Doc)", &registry).unwrap();
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![Value::Vector(vector(&[0.0, 0.0]))]),
    );
    session.bind_parameter(
        db_string("roots"),
        Value::List(vec![node_list(&[NodeId::new(1)])]),
    );

    let err = session
        .execute_source(
            "CALL selene.vector_score_candidate_state_expanded_batch('embedding', $queries, \
             'active_docs', $roots, 'SUPPORTS', 3)",
            &registry,
        )
        .expect_err("stale provider must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::Internal { ref detail },
            ..
        } if detail.contains("candidate-state provider error")
            && detail.contains("stale candidate state")
    ));
}

#[test]
fn vector_candidate_states_surfaces_stale_provider_generation() {
    let provider = Arc::new(StaleCandidateProvider);
    let graph = SharedGraph::builder(GraphId::new(330_404))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .expect("graph builds");
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let err = session
        .execute_source("CALL selene.vector_candidate_states()", &registry)
        .expect_err("stale provider must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::Internal { ref detail },
            ..
        } if detail.contains("candidate-state provider error")
            && detail.contains("stale candidate state")
    ));
}

struct StaleCandidateProvider;

impl IndexProvider for StaleCandidateProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(CANDIDATE_STATE_PROVIDER_TAG)
    }

    fn read_section(&self, _sub_tag: SubTag, _bytes: &[u8]) -> Result<(), ProviderError> {
        Ok(())
    }

    fn write_section(&self, _sub_tag: SubTag) -> Result<Vec<u8>, ProviderError> {
        Ok(Vec::new())
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        Ok(())
    }

    fn vector_candidate_set(
        &self,
        _name: &DbString,
        _generation: u64,
    ) -> Result<Option<selene_graph::VectorCandidateSet>, ProviderError> {
        Err(ProviderError::Inconsistent {
            reason: "stale candidate state".to_owned(),
        })
    }

    fn vector_candidate_state_infos(
        &self,
        _generation: u64,
    ) -> Result<Vec<VectorCandidateStateInfo>, ProviderError> {
        Err(ProviderError::Inconsistent {
            reason: "stale candidate state".to_owned(),
        })
    }

    fn declared_sub_tags(&self) -> &[SubTag] {
        &[]
    }
}
