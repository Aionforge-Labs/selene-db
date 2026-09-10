//! End-to-end coverage for text scoring over maintained candidate state.

use std::sync::Arc;

use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value, VectorValue};
use selene_gql::{
    BindingTable, BuiltinProcedureRegistry, ExecutorError, ProcedureError, ProcedureRegistry,
    Session, StatementOutput,
};
use selene_graph::{
    CandidateStateSpec, IndexProvider, MaintainedCandidateStateProvider, SharedGraph,
};

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn graph(id: u64) -> SharedGraph {
    let doc = db_string("TextDoc");
    let negative = db_string("NEGATIVE");
    let provider = Arc::new(
        MaintainedCandidateStateProvider::new([CandidateStateSpec::new(db_string("current_docs"))
            .require_label(doc)
            .exclude_outgoing(negative)])
        .expect("candidate-state provider is valid"),
    );
    SharedGraph::builder(GraphId::new(id))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .expect("graph builds")
}

fn props(key: &DbString, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(key.clone(), value)]).expect("test property map is valid")
}

fn doc_props(body: &DbString, embedding: &DbString, text: &str, vector: &[f32]) -> PropertyMap {
    PropertyMap::from_pairs([
        (body.clone(), Value::String(db_string(text))),
        (embedding.clone(), Value::Vector(vector_value(vector))),
    ])
    .expect("test document property map is valid")
}

fn vector_value(components: &[f32]) -> VectorValue {
    VectorValue::new(components.to_vec()).expect("test vector is valid")
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

fn execute_ok(session: &mut Session<'_>, source: &str, registry: &dyn ProcedureRegistry) {
    session
        .execute_source(source, registry)
        .expect("statement executes");
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

#[test]
fn text_score_candidate_state_reranks_maintained_set() {
    let graph = graph(431_604);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let seed = seed_state_graph(&graph);
    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.text_score_candidate_state( \
            'TextDoc', 'body', 'graph', 'current_docs', 3) \
         YIELD node_id, score",
        &registry,
    );

    assert_eq!(node_column(&table, "node_id"), vec![seed.current_graph]);
}

#[test]
fn text_score_candidate_state_nodes_intersects_state_with_nodes_by_default() {
    let graph = graph(431_605);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let seed = seed_state_graph(&graph);
    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );
    session.bind_parameter(
        db_string("nodes"),
        node_list(&[seed.current_graph, seed.stale_graph, seed.current_memory]),
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.text_score_candidate_state_nodes( \
            'TextDoc', 'body', 'current', 'current_docs', $nodes, 3) \
         YIELD node_id, score",
        &registry,
    );

    assert_eq!(
        node_column(&table, "node_id"),
        vec![seed.current_graph, seed.current_memory]
    );
}

#[test]
fn text_score_candidate_state_nodes_supports_explicit_set_algebra() {
    let graph = graph(431_606);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let seed = seed_state_graph(&graph);
    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );

    session.bind_parameter(db_string("union_nodes"), node_list(&[seed.stale_graph]));
    let union = execute_rows(
        &mut session,
        "CALL selene.text_score_candidate_state_nodes( \
            'TextDoc', 'body', 'graph', 'current_docs', $union_nodes, 3, 'union') \
         YIELD node_id, score",
        &registry,
    );
    assert_eq!(
        node_column(&union, "node_id"),
        vec![seed.stale_graph, seed.current_graph]
    );

    session.bind_parameter(
        db_string("state_diff_nodes"),
        node_list(&[seed.current_graph]),
    );
    let state_difference = execute_rows(
        &mut session,
        "CALL selene.text_score_candidate_state_nodes( \
            'TextDoc', 'body', 'memory', 'current_docs', $state_diff_nodes, 3, 'state_difference') \
         YIELD node_id, score",
        &registry,
    );
    assert_eq!(
        node_column(&state_difference, "node_id"),
        vec![seed.current_memory]
    );

    session.bind_parameter(
        db_string("nodes_diff_nodes"),
        node_list(&[seed.stale_graph, seed.current_memory]),
    );
    let nodes_difference = execute_rows(
        &mut session,
        "CALL selene.text_score_candidate_state_nodes( \
            'TextDoc', 'body', 'graph', 'current_docs', $nodes_diff_nodes, 3, 'nodes_difference') \
         YIELD node_id, score",
        &registry,
    );
    assert_eq!(
        node_column(&nodes_difference, "node_id"),
        vec![seed.stale_graph]
    );
}

#[test]
fn text_score_candidate_state_nodes_rejects_unknown_operation() {
    let graph = graph(431_607);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let seed = seed_state_graph(&graph);
    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );
    session.bind_parameter(db_string("nodes"), node_list(&[seed.current_graph]));

    let err = session
        .execute_source(
            "CALL selene.text_score_candidate_state_nodes( \
                'TextDoc', 'body', 'graph', 'current_docs', $nodes, 3, 'xor')",
            &registry,
        )
        .expect_err("unknown operation must fail");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("operation must be intersection")
    ));
}

#[test]
fn text_score_candidate_state_requires_registered_text_index() {
    let graph = graph(431_608);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    seed_state_graph(&graph);

    let err = session
        .execute_source(
            "CALL selene.text_score_candidate_state( \
                'TextDoc', 'body', 'graph', 'current_docs', 3)",
            &registry,
        )
        .expect_err("candidate-state text scoring requires text index");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("requires a text index")
    ));
}

#[test]
fn text_score_candidate_state_expanded_batch_filters_stale_candidates() {
    let graph = graph(431_601);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let (graph_root, memory_root, current_graph, current_memory) = seed_graph(&graph);
    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![
            Value::String(db_string("graph")),
            Value::String(db_string("memory")),
        ]),
    );
    session.bind_parameter(
        db_string("roots"),
        Value::List(vec![node_list(&[graph_root]), node_list(&[memory_root])]),
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.text_score_candidate_state_expanded_batch( \
            'TextDoc', 'body', $queries, 'current_docs', $roots, 'SUPPORTS', 2) \
         YIELD query_index, node_id, score",
        &registry,
    );

    assert_eq!(uint_column(&table, "query_index"), vec![0, 1]);
    assert_eq!(
        node_column(&table, "node_id"),
        vec![current_graph, current_memory]
    );
}

#[test]
fn text_score_candidate_state_expanded_batch_rejects_mismatched_roots() {
    let graph = graph(431_602);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session
        .execute_source("INSERT (:TextDoc)", &registry)
        .unwrap();
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![
            Value::String(db_string("graph")),
            Value::String(db_string("memory")),
        ]),
    );
    session.bind_parameter(
        db_string("roots"),
        Value::List(vec![node_list(&[NodeId::new(1)])]),
    );

    let err = session
        .execute_source(
            "CALL selene.text_score_candidate_state_expanded_batch( \
                'TextDoc', 'body', $queries, 'current_docs', $roots, 'SUPPORTS', 2)",
            &registry,
        )
        .expect_err("mismatched text-state batch lengths must fail");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("queries and roots must have the same length")
    ));
}

#[test]
fn text_state_candidates_feed_vector_batch_rerank() {
    let graph = graph(431_603);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let (graph_root, memory_root, graph_target, memory_target) = seed_hybrid_graph(&graph);
    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );
    session.bind_parameter(
        db_string("text_queries"),
        Value::List(vec![
            Value::String(db_string("graph")),
            Value::String(db_string("memory")),
        ]),
    );
    session.bind_parameter(
        db_string("vector_queries"),
        Value::List(vec![
            Value::Vector(vector_value(&[1.0, 0.0])),
            Value::Vector(vector_value(&[0.0, 1.0])),
        ]),
    );
    session.bind_parameter(
        db_string("roots"),
        Value::List(vec![node_list(&[graph_root]), node_list(&[memory_root])]),
    );

    let table = execute_rows(
        &mut session,
        "WITH $text_queries AS text_queries, $vector_queries AS vector_queries, $roots AS roots \
         CALL selene.text_score_candidate_state_expanded_batch( \
            'TextDoc', 'body', text_queries, 'current_docs', roots, 'SUPPORTS', 2) \
         YIELD query_index, node_id, score \
         WITH vector_queries, query_index, collect_list(node_id) AS candidates \
         GROUP BY vector_queries, query_index ORDER BY query_index \
         WITH vector_queries, collect_list(candidates) AS candidate_sets \
         CALL selene.vector_score_nodes_batch('embedding', vector_queries, candidate_sets, 1, 'cosine') \
         YIELD query_index, node_id, distance \
         RETURN query_index, node_id, distance",
        &registry,
    );

    assert_eq!(uint_column(&table, "query_index"), vec![0, 1]);
    assert_eq!(
        node_column(&table, "node_id"),
        vec![graph_target, memory_target]
    );
}

#[derive(Debug)]
struct TextStateSeed {
    current_graph: NodeId,
    stale_graph: NodeId,
    current_memory: NodeId,
}

fn seed_state_graph(graph: &SharedGraph) -> TextStateSeed {
    let doc = db_string("TextDoc");
    let body = db_string("body");
    let negative = db_string("NEGATIVE");
    let mut txn = graph.begin_write();
    let mut mutator = txn.mutator();
    let current_graph = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            props(&body, Value::String(db_string("graph current fact"))),
        )
        .expect("current graph doc inserts");
    let stale_graph = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            props(&body, Value::String(db_string("graph graph stale fact"))),
        )
        .expect("stale graph doc inserts");
    let current_memory = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            props(&body, Value::String(db_string("memory current fact"))),
        )
        .expect("current memory doc inserts");
    let stale_memory = mutator
        .create_node(
            LabelSet::single(doc),
            props(&body, Value::String(db_string("memory memory stale fact"))),
        )
        .expect("stale memory doc inserts");
    for stale in [stale_graph, stale_memory] {
        mutator
            .create_edge(negative.clone(), stale, current_graph, PropertyMap::new())
            .expect("negative evidence edge inserts");
    }
    txn.commit().expect("state seed commits");
    TextStateSeed {
        current_graph,
        stale_graph,
        current_memory,
    }
}

fn seed_graph(graph: &SharedGraph) -> (NodeId, NodeId, NodeId, NodeId) {
    let root = db_string("TextRoot");
    let doc = db_string("TextDoc");
    let body = db_string("body");
    let supports = db_string("SUPPORTS");
    let negative = db_string("NEGATIVE");
    let mut txn = graph.begin_write();
    let mut mutator = txn.mutator();
    let graph_root = mutator
        .create_node(LabelSet::single(root.clone()), PropertyMap::new())
        .expect("graph root inserts");
    let memory_root = mutator
        .create_node(LabelSet::single(root), PropertyMap::new())
        .expect("memory root inserts");
    let current_graph = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            props(&body, Value::String(db_string("graph current fact"))),
        )
        .expect("current graph doc inserts");
    let stale_graph = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            props(&body, Value::String(db_string("graph graph stale fact"))),
        )
        .expect("stale graph doc inserts");
    let current_memory = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            props(&body, Value::String(db_string("memory current fact"))),
        )
        .expect("current memory doc inserts");
    let stale_memory = mutator
        .create_node(
            LabelSet::single(doc),
            props(&body, Value::String(db_string("memory memory stale fact"))),
        )
        .expect("stale memory doc inserts");
    for target in [current_graph, stale_graph] {
        mutator
            .create_edge(supports.clone(), graph_root, target, PropertyMap::new())
            .expect("graph support edge inserts");
    }
    for target in [current_memory, stale_memory] {
        mutator
            .create_edge(supports.clone(), memory_root, target, PropertyMap::new())
            .expect("memory support edge inserts");
    }
    for stale in [stale_graph, stale_memory] {
        mutator
            .create_edge(negative.clone(), stale, current_graph, PropertyMap::new())
            .expect("negative evidence edge inserts");
    }
    txn.commit().expect("seed commits");
    (graph_root, memory_root, current_graph, current_memory)
}

fn seed_hybrid_graph(graph: &SharedGraph) -> (NodeId, NodeId, NodeId, NodeId) {
    let root = db_string("TextRoot");
    let doc = db_string("TextDoc");
    let body = db_string("body");
    let embedding = db_string("embedding");
    let supports = db_string("SUPPORTS");
    let negative = db_string("NEGATIVE");
    let mut txn = graph.begin_write();
    let mut mutator = txn.mutator();
    let graph_root = mutator
        .create_node(LabelSet::single(root.clone()), PropertyMap::new())
        .expect("graph root inserts");
    let memory_root = mutator
        .create_node(LabelSet::single(root), PropertyMap::new())
        .expect("memory root inserts");
    let graph_target = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            doc_props(&body, &embedding, "graph current precise", &[1.0, 0.0]),
        )
        .expect("precise graph doc inserts");
    let graph_other = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            doc_props(&body, &embedding, "graph current broad", &[0.25, 0.75]),
        )
        .expect("broad graph doc inserts");
    let graph_stale = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            doc_props(&body, &embedding, "graph stale", &[1.0, 0.0]),
        )
        .expect("stale graph doc inserts");
    let memory_target = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            doc_props(&body, &embedding, "memory current precise", &[0.0, 1.0]),
        )
        .expect("precise memory doc inserts");
    let memory_other = mutator
        .create_node(
            LabelSet::single(doc.clone()),
            doc_props(&body, &embedding, "memory current broad", &[0.75, 0.25]),
        )
        .expect("broad memory doc inserts");
    let memory_stale = mutator
        .create_node(
            LabelSet::single(doc),
            doc_props(&body, &embedding, "memory stale", &[0.0, 1.0]),
        )
        .expect("stale memory doc inserts");
    for target in [graph_target, graph_other, graph_stale] {
        mutator
            .create_edge(supports.clone(), graph_root, target, PropertyMap::new())
            .expect("graph support edge inserts");
    }
    for target in [memory_target, memory_other, memory_stale] {
        mutator
            .create_edge(supports.clone(), memory_root, target, PropertyMap::new())
            .expect("memory support edge inserts");
    }
    for (stale, replacement) in [(graph_stale, graph_target), (memory_stale, memory_target)] {
        mutator
            .create_edge(negative.clone(), stale, replacement, PropertyMap::new())
            .expect("negative evidence edge inserts");
    }
    txn.commit().expect("hybrid seed commits");
    (graph_root, memory_root, graph_target, memory_target)
}
