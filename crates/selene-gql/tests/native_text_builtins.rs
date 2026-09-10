//! End-to-end coverage for native `selene.*` text-search built-ins.

use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value};
use selene_gql::{
    BindingTable, BuiltinProcedureRegistry, ExecutorError, ProcedureError, ProcedureRegistry,
    Session, StatementOutput,
};
use selene_graph::SharedGraph;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn graph(id: u64) -> SharedGraph {
    SharedGraph::new(GraphId::new(id))
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

fn float_column(table: &BindingTable, name: &str) -> Vec<f64> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::Float(value)) => *value,
            other => panic!("expected float in {name}, got {other:?}"),
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

fn string_column(table: &BindingTable, name: &str) -> Vec<String> {
    let index = table
        .column_index(db_string(name))
        .unwrap_or_else(|| panic!("missing column {name}"));
    table
        .rows()
        .iter()
        .map(|row| match row.values().get(index) {
            Some(Value::String(value)) => value.as_str().to_owned(),
            other => panic!("expected string in {name}, got {other:?}"),
        })
        .collect()
}

#[test]
fn text_search_nodes_ranks_string_properties() {
    let graph = graph(431_101);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let doc = db_string("TextDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(
                    &body,
                    Value::String(db_string("graph memory graph retrieval")),
                ),
            )
            .expect("text node inserts");
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(
                    &body,
                    Value::String(db_string("vector retrieval retrieval")),
                ),
            )
            .expect("text node inserts");
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph search"))),
            )
            .expect("text node inserts");
        mutator
            .create_node(LabelSet::single(doc.clone()), props(&body, Value::Int(1)))
            .expect("non-string node inserts");
        txn.commit().expect("seed commits");
    }

    let table = execute_rows(
        &mut session,
        "CALL selene.text_search_nodes('TextDoc', 'body', 'graph retrieval', 3) \
         YIELD node_id, score",
        &registry,
    );

    assert_eq!(
        node_column(&table, "node_id"),
        vec![NodeId::new(1), NodeId::new(2), NodeId::new(3)]
    );
    let scores = float_column(&table, "score");
    assert!(scores[0] > scores[1]);
    assert!(scores[1] > scores[2]);
}

#[test]
fn create_text_index_commits_and_stats_reports_bm25_state() {
    let graph = graph(431_104);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let doc = db_string("TextDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(
                    &body,
                    Value::String(db_string("graph memory graph retrieval")),
                ),
            )
            .expect("text node inserts");
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("vector retrieval"))),
            )
            .expect("text node inserts");
        mutator
            .create_node(LabelSet::single(doc), props(&body, Value::Int(7)))
            .expect("non-string node inserts");
        txn.commit().expect("seed commits");
    }

    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );

    assert_eq!(graph.read().text_index_count(), 1);
    assert_eq!(
        graph
            .read()
            .text_index_for(&db_string("TextDoc"), &body)
            .unwrap()
            .search("graph", 10)
            .into_iter()
            .map(|hit| hit.node_id)
            .collect::<Vec<_>>(),
        vec![NodeId::new(1)]
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.text_index_stats() \
         YIELD name, label, property, indexed_rows, documents, distinct_terms, postings, \
               total_document_len, document_term_bytes, estimated_index_bytes",
        &registry,
    );

    assert_eq!(string_column(&table, "name"), vec!["body_idx".to_owned()]);
    assert_eq!(string_column(&table, "label"), vec!["TextDoc".to_owned()]);
    assert_eq!(string_column(&table, "property"), vec!["body".to_owned()]);
    assert_eq!(uint_column(&table, "indexed_rows"), vec![2]);
    assert_eq!(uint_column(&table, "documents"), vec![2]);
    assert_eq!(uint_column(&table, "distinct_terms"), vec![4]);
    assert_eq!(uint_column(&table, "postings"), vec![5]);
    assert_eq!(uint_column(&table, "total_document_len"), vec![6]);
    assert!(uint_column(&table, "document_term_bytes")[0] > 0);
    assert!(uint_column(&table, "estimated_index_bytes")[0] > 0);
}

#[test]
fn text_score_nodes_reranks_explicit_candidates_with_index() {
    let graph = graph(431_107);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let doc = db_string("TextDoc");
    let body = db_string("body");
    let ids = {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let top_global = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(
                    &body,
                    Value::String(db_string("graph graph graph graph memory")),
                ),
            )
            .expect("text node inserts");
        let weak = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph retrieval"))),
            )
            .expect("text node inserts");
        let strong = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph graph notes"))),
            )
            .expect("text node inserts");
        let non_string = mutator
            .create_node(LabelSet::single(doc.clone()), props(&body, Value::Int(7)))
            .expect("non-string node inserts");
        let no_match = mutator
            .create_node(
                LabelSet::single(doc),
                props(&body, Value::String(db_string("vector retrieval"))),
            )
            .expect("text node inserts");
        txn.commit().expect("seed commits");
        [top_global, weak, strong, non_string, no_match]
    };

    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );
    session.bind_parameter(
        db_string("nodes"),
        Value::List(vec![
            Value::NodeRef(ids[1]),
            Value::NodeRef(ids[2]),
            Value::NodeRef(ids[2]),
            Value::NodeRef(ids[3]),
            Value::NodeRef(ids[4]),
        ]),
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.text_score_nodes('TextDoc', 'body', 'graph', $nodes, 3) \
         YIELD node_id, score",
        &registry,
    );

    assert_eq!(node_column(&table, "node_id"), vec![ids[2], ids[1]]);
    assert!(!node_column(&table, "node_id").contains(&ids[0]));
}

#[test]
fn text_score_nodes_requires_registered_text_index() {
    let graph = graph(431_108);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session
        .execute_source("INSERT (:TextDoc)", &registry)
        .unwrap();
    session.bind_parameter(db_string("nodes"), node_list(&[NodeId::new(1)]));

    let err = session
        .execute_source(
            "CALL selene.text_score_nodes('TextDoc', 'body', 'graph', $nodes, 10)",
            &registry,
        )
        .expect_err("missing text index must fail");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("requires a text index")
    ));
}

#[test]
fn text_score_nodes_uses_registered_text_index() {
    let graph = graph(431_110);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let doc = db_string("TextDoc");
    let body = db_string("body");
    let ids = {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let first = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent graph memory"))),
            )
            .expect("text node inserts");
        let second = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent graph graph memory"))),
            )
            .expect("text node inserts");
        let third = mutator
            .create_node(
                LabelSet::single(doc),
                props(&body, Value::String(db_string("agent vector retrieval"))),
            )
            .expect("text node inserts");
        txn.commit().expect("seed commits");
        [first, second, third]
    };

    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body', 'body_idx')",
        &registry,
    );
    session.bind_parameter(db_string("nodes"), node_list(&[ids[0], ids[1], ids[2]]));

    let table = execute_rows(
        &mut session,
        "CALL selene.text_score_nodes('TextDoc', 'body', 'graph memory', $nodes, 2) \
         YIELD node_id, score",
        &registry,
    );

    assert_eq!(node_column(&table, "node_id"), vec![ids[1], ids[0]]);
    let scores = float_column(&table, "score");
    assert!(scores[0] > scores[1]);
}

#[test]
fn text_score_nodes_batch_reranks_per_query_candidates_with_index() {
    let graph = graph(431_111);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let doc = db_string("TextDoc");
    let body = db_string("body");
    let ids = {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let graph_memory = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph graph memory"))),
            )
            .expect("text node inserts");
        let graph_retrieval = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph retrieval"))),
            )
            .expect("text node inserts");
        let memory_notes = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("memory memory notes"))),
            )
            .expect("text node inserts");
        let no_match = mutator
            .create_node(
                LabelSet::single(doc),
                props(&body, Value::String(db_string("vector only"))),
            )
            .expect("text node inserts");
        txn.commit().expect("seed commits");
        [graph_memory, graph_retrieval, memory_notes, no_match]
    };

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
        db_string("nodes"),
        Value::List(vec![
            node_list(&[ids[0], ids[1], ids[2], ids[3]]),
            node_list(&[ids[0], ids[2], ids[3]]),
        ]),
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.text_score_nodes_batch('TextDoc', 'body', $queries, $nodes, 2) \
         YIELD query_index, node_id, score",
        &registry,
    );

    assert_eq!(uint_column(&table, "query_index"), vec![0, 0, 1, 1]);
    assert_eq!(
        node_column(&table, "node_id"),
        vec![ids[0], ids[1], ids[2], ids[0]]
    );
    let scores = float_column(&table, "score");
    assert!(scores[0] > scores[1]);
    assert!(scores[2] > scores[3]);
}

#[test]
fn text_score_nodes_batch_rejects_mismatched_batch_lengths() {
    let graph = graph(431_112);
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
        db_string("nodes"),
        Value::List(vec![node_list(&[NodeId::new(1)])]),
    );

    let err = session
        .execute_source(
            "CALL selene.text_score_nodes_batch('TextDoc', 'body', $queries, $nodes, 10)",
            &registry,
        )
        .expect_err("mismatched text-score batch lengths must fail");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("queries and nodes must have the same length")
    ));
}

#[test]
fn text_score_nodes_batch_requires_registered_text_index() {
    let graph = graph(431_113);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session
        .execute_source("INSERT (:TextDoc)", &registry)
        .unwrap();
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![Value::String(db_string("graph"))]),
    );
    session.bind_parameter(
        db_string("nodes"),
        Value::List(vec![node_list(&[NodeId::new(1)])]),
    );

    let err = session
        .execute_source(
            "CALL selene.text_score_nodes_batch('TextDoc', 'body', $queries, $nodes, 10)",
            &registry,
        )
        .expect_err("missing text index must fail");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("requires a text index")
    ));
}

#[test]
fn text_score_nodes_batch_rejects_non_node_candidates() {
    let graph = graph(431_114);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![Value::String(db_string("graph"))]),
    );
    session.bind_parameter(
        db_string("nodes"),
        Value::List(vec![Value::List(vec![Value::Int(1)])]),
    );

    let err = session
        .execute_source(
            "CALL selene.text_score_nodes_batch('TextDoc', 'body', $queries, $nodes, 10)",
            &registry,
        )
        .expect_err("non-node candidates must fail");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("nodes[0][0] must be a NODE")
    ));
}

#[test]
fn text_score_nodes_rejects_non_node_candidates() {
    let graph = graph(431_109);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("nodes"), Value::List(vec![Value::Int(1)]));

    let err = session
        .execute_source(
            "CALL selene.text_score_nodes('TextDoc', 'body', 'graph', $nodes, 10)",
            &registry,
        )
        .expect_err("non-node candidates must fail");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("nodes[0] must be a NODE")
    ));
}

#[test]
fn create_text_index_rejects_duplicate_registration() {
    let graph = graph(431_105);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body')",
        &registry,
    );
    let err = session
        .execute_source(
            "CALL selene.create_text_index('TextDoc', 'body')",
            &registry,
        )
        .expect_err("duplicate text index should fail");

    let ExecutorError::Procedure {
        source: ProcedureError::InvalidArgument { detail },
        ..
    } = err
    else {
        panic!("expected invalid procedure argument, got {err:?}");
    };
    assert!(detail.contains("already exists"));
}

#[test]
fn drop_text_index_removes_registration() {
    let graph = graph(431_106);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    execute_ok(
        &mut session,
        "CALL selene.create_text_index('TextDoc', 'body')",
        &registry,
    );
    assert_eq!(graph.read().text_index_count(), 1);

    execute_ok(
        &mut session,
        "CALL selene.drop_text_index('TextDoc', 'body')",
        &registry,
    );

    assert_eq!(graph.read().text_index_count(), 0);
    let table = execute_rows(
        &mut session,
        "CALL selene.text_index_stats() YIELD name",
        &registry,
    );
    assert_eq!(table.row_count(), 0);
}

#[test]
fn text_search_nodes_rejects_negative_k() {
    let graph = graph(431_102);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);

    let err = session
        .execute_source(
            "CALL selene.text_search_nodes('TextDoc', 'body', 'graph', -1)",
            &registry,
        )
        .expect_err("negative k should fail");

    let ExecutorError::Procedure {
        source: ProcedureError::InvalidArgument { detail },
        ..
    } = err
    else {
        panic!("expected invalid procedure argument, got {err:?}");
    };
    assert!(detail.contains("k must be a non-negative INTEGER"));
}

#[test]
fn text_search_nodes_empty_token_query_returns_no_rows() {
    let graph = graph(431_103);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    let doc = db_string("TextDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(doc),
                props(&body, Value::String(db_string("graph memory"))),
            )
            .expect("text node inserts");
        txn.commit().expect("seed commits");
    }

    let table = execute_rows(
        &mut session,
        "CALL selene.text_search_nodes('TextDoc', 'body', '!!!', 3) \
         YIELD node_id, score",
        &registry,
    );

    assert_eq!(table.row_count(), 0);

    let table = execute_rows(
        &mut session,
        "CALL selene.text_search_nodes('TextDoc', 'body', '', 3) \
         YIELD node_id, score",
        &registry,
    );

    assert_eq!(table.row_count(), 0);
}
