use selene_core::{
    CancellationChecker, CancellationToken, GraphId, LabelDiff, LabelSet, NodeId, PropertyDiff,
    PropertyMap, Value,
};

use super::*;
use crate::SharedGraph;
use crate::text_search::TextSearchError;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn props(key: &DbString, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(key.clone(), value)]).expect("test property map is valid")
}

#[test]
fn text_index_matches_exact_bm25_ranking() {
    let graph = SharedGraph::new(GraphId::new(433_001));
    let doc = db_string("TextIndexedDoc");
    let other = db_string("OtherDoc");
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
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(
                    &body,
                    Value::String(db_string("vector retrieval retrieval")),
                ),
            )
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph search"))),
            )
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("unmatched corpus document"))),
            )
            .unwrap();
        mutator
            .create_node(LabelSet::single(doc.clone()), props(&body, Value::Int(7)))
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(other),
                props(&body, Value::String(db_string("graph retrieval"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let snapshot = graph.read();
    let exact = snapshot
        .exact_text_search_nodes(&doc, &body, "Graph retrieval", 10)
        .unwrap();
    let index = snapshot.build_text_index(&doc, &body).unwrap();
    let indexed = index.search("Graph retrieval", 10);

    assert_eq!(indexed, exact);
    assert_eq!(
        indexed.iter().map(|hit| hit.node_id).collect::<Vec<_>>(),
        vec![NodeId::new(1), NodeId::new(2), NodeId::new(3)]
    );
}

#[test]
fn text_index_preserves_lowercase_terms_after_punctuation_skip_run() {
    let graph = SharedGraph::new(GraphId::new(433_900));
    let doc = db_string("TextIndexedPunctuationDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("alpha, graph beta"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let snapshot = graph.read();
    let index = snapshot.build_text_index(&doc, &body).unwrap();
    assert_eq!(
        index
            .search("graph", 10)
            .iter()
            .map(|hit| hit.node_id)
            .collect::<Vec<_>>(),
        vec![NodeId::new(1)]
    );
    assert_eq!(
        index
            .search("beta", 10)
            .iter()
            .map(|hit| hit.node_id)
            .collect::<Vec<_>>(),
        vec![NodeId::new(1)]
    );
}

#[test]
fn text_index_rebuild_observes_update_and_delete_visibility() {
    let graph = SharedGraph::new(GraphId::new(433_002));
    let doc = db_string("TextIndexedMutableDoc");
    let body = db_string("body");
    let stale;
    let fresh;
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        stale = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("stale memory"))),
            )
            .unwrap();
        fresh = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("fresh memory"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let before = graph.build_text_index(&doc, &body).unwrap();
    assert_eq!(
        before
            .search("fresh", 10)
            .iter()
            .map(|hit| hit.node_id)
            .collect::<Vec<_>>(),
        vec![fresh]
    );

    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .update_node(
                stale,
                LabelDiff::new([], []).unwrap(),
                PropertyDiff::new(
                    [(body.clone(), Value::String(db_string("fresh updated")))],
                    [],
                )
                .unwrap(),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let after_update = graph.build_text_index(&doc, &body).unwrap();
    assert_eq!(
        after_update
            .search("updated", 10)
            .iter()
            .map(|hit| hit.node_id)
            .collect::<Vec<_>>(),
        vec![stale]
    );

    {
        let mut txn = graph.begin_write();
        txn.mutator().delete_node(stale).unwrap();
        txn.commit().unwrap();
    }
    let after_delete = graph.build_text_index(&doc, &body).unwrap();
    assert_eq!(
        after_delete
            .search("fresh", 10)
            .iter()
            .map(|hit| hit.node_id)
            .collect::<Vec<_>>(),
        vec![fresh]
    );
}

#[test]
fn text_index_reports_stats_and_memory() {
    let graph = SharedGraph::new(GraphId::new(433_003));
    let doc = db_string("TextStatsDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent graph graph"))),
            )
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent vector memory"))),
            )
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("!!!"))),
            )
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::Bool(true)),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let index = graph.build_text_index(&doc, &body).unwrap();
    let stats = index.stats();
    let usage = index.memory_usage();

    assert_eq!(stats.indexed_rows, 2);
    assert_eq!(stats.documents, 2);
    assert_eq!(stats.distinct_terms, 4);
    assert_eq!(stats.postings, 5);
    assert_eq!(stats.total_document_len, 6);
    assert_eq!(usage.documents, stats.documents);
    assert_eq!(usage.distinct_terms, stats.distinct_terms);
    assert!(usage.document_term_bytes > 0);
    assert!(usage.estimated_index_bytes >= usage.posting_bytes);
}

#[test]
fn text_index_interns_document_terms_with_posting_keys() {
    let graph = SharedGraph::new(GraphId::new(433_902));
    let doc = db_string("TextInternedTermsDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent graph graph"))),
            )
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent graph retrieval"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let index = graph.build_text_index(&doc, &body).unwrap();

    assert_eq!(index.term_count(), 3);
    for terms in index.document_terms.values() {
        for term in terms.iter() {
            let (posting_term, _) = index
                .postings
                .get_key_value(term.as_ref())
                .expect("document term has postings key");
            assert!(std::sync::Arc::ptr_eq(term, posting_term));
        }
    }
}

#[test]
fn text_index_replacement_updates_shared_terms_in_place() {
    let graph = SharedGraph::new(GraphId::new(433_904));
    let doc = db_string("TextReplacementDoc");
    let body = db_string("body");
    let target;
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        target = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent graph graph stale"))),
            )
            .unwrap();
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent memory current"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let snapshot = graph.read();
    let row = snapshot
        .node_row_for_id(target)
        .expect("target row remains live")
        .get();
    let mut index = snapshot.build_text_index(&doc, &body).unwrap();
    index.insert_document(row, target, "agent graph current current");

    assert_eq!(index.document_count(), 2);
    assert_eq!(index.stats().total_document_len, 7);
    assert_eq!(index.search("stale", 10), Vec::new());
    assert_eq!(
        index
            .search("graph", 10)
            .iter()
            .map(|hit| hit.node_id)
            .collect::<Vec<_>>(),
        vec![target]
    );
    let current_postings = index
        .postings
        .get("current")
        .expect("current postings exist");
    let target_posting = current_postings
        .iter()
        .find(|posting| posting.node_id == target)
        .expect("target has current posting");
    assert_eq!(target_posting.term_count, 2);
}

#[test]
fn text_index_counts_terms_after_inline_accumulator_spill() {
    let graph = SharedGraph::new(GraphId::new(433_903));
    let doc = db_string("TextSpilledTermsDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(doc.clone()),
                props(
                    &body,
                    Value::String(db_string(
                        "alpha beta gamma delta epsilon zeta eta theta iota kappa alpha kappa",
                    )),
                ),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let index = graph.build_text_index(&doc, &body).unwrap();

    assert_eq!(index.term_count(), 10);
    assert_eq!(index.posting_count(), 10);
    assert_eq!(index.stats().total_document_len, 12);
    assert_eq!(index.postings["alpha"][0].term_count, 2);
    assert_eq!(index.postings["kappa"][0].term_count, 2);
}

#[test]
fn text_index_sparse_label_build_does_not_keep_label_row_capacity() {
    let graph = SharedGraph::new(GraphId::new(433_901));
    let doc = db_string("TextSparseDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        for row in 0..512 {
            mutator
                .create_node(
                    LabelSet::single(doc.clone()),
                    props(&body, Value::Int(i64::from(row))),
                )
                .unwrap();
        }
        mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agent memory"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let index = graph.build_text_index(&doc, &body).unwrap();
    let usage = index.memory_usage();

    assert_eq!(index.document_count(), 1);
    assert_eq!(index.posting_count(), 2);
    assert!(
        usage.document_length_bytes < 4096,
        "sparse text index retained {} bytes of document-length capacity",
        usage.document_length_bytes
    );
    assert!(
        usage.document_term_bytes < 4096,
        "sparse text index retained {} bytes of document-term capacity",
        usage.document_term_bytes
    );
}

#[test]
fn text_index_empty_query_and_zero_k_are_empty() {
    let graph = SharedGraph::new(GraphId::new(433_004));
    let doc = db_string("TextIndexedEmptyDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph memory"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let index = graph.build_text_index(&doc, &body).unwrap();

    assert!(index.search("!!!", 10).is_empty());
    assert!(index.search("graph", 0).is_empty());
    assert!(
        index
            .search_candidates("graph", &[NodeId::new(1)], 0)
            .is_empty()
    );
    assert!(
        index
            .search_candidates("!!!", &[NodeId::new(1)], 10)
            .is_empty()
    );
    assert!(index.search_candidates("graph", &[], 10).is_empty());
}

#[test]
fn shared_graph_indexed_text_search_matches_exact() {
    let graph = SharedGraph::new(GraphId::new(433_005));
    let doc = db_string("TextSharedIndexedDoc");
    let body = db_string("body");
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("agentic graph retrieval"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    assert_eq!(
        graph
            .indexed_text_search_nodes(&doc, &body, "graph retrieval", 10)
            .unwrap(),
        graph
            .exact_text_search_nodes(&doc, &body, "graph retrieval", 10)
            .unwrap()
    );
}

#[test]
fn text_index_candidate_search_matches_global_filter() {
    let graph = SharedGraph::new(GraphId::new(433_006));
    let doc = db_string("TextCandidateDoc");
    let body = db_string("body");
    let keep_a;
    let keep_b;
    let reject;
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        keep_a = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph memory graph"))),
            )
            .unwrap();
        reject = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph memory memory"))),
            )
            .unwrap();
        keep_b = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph retrieval"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let index = graph.build_text_index(&doc, &body).unwrap();
    let candidates = [keep_a, keep_b];
    let candidate_set = std::collections::HashSet::<_>::from(candidates);
    let filtered_global = index
        .search("graph memory", 10)
        .into_iter()
        .filter(|hit| candidate_set.contains(&hit.node_id))
        .collect::<Vec<_>>();

    let scoped = index.search_candidates("graph memory", &candidates, 10);

    assert_eq!(scoped, filtered_global);
    assert_eq!(
        scoped.iter().map(|hit| hit.node_id).collect::<Vec<_>>(),
        vec![keep_a, keep_b]
    );
    assert!(!scoped.iter().any(|hit| hit.node_id == reject));
}

#[test]
fn text_index_candidate_search_full_cover_matches_global() {
    let graph = SharedGraph::new(GraphId::new(433_017));
    let doc = db_string("TextCandidateFullCoverDoc");
    let body = db_string("body");
    let node_a;
    let node_b;
    let node_c;
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        node_a = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph memory graph"))),
            )
            .unwrap();
        node_b = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph retrieval"))),
            )
            .unwrap();
        node_c = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("memory retrieval graph"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let index = graph.build_text_index(&doc, &body).unwrap();
    let global = index.search("graph memory", 10);

    assert_eq!(
        index.search_candidates("graph memory", &[node_a, node_b, node_c], 10),
        global
    );
    assert_eq!(
        index.search_candidates("graph memory", &[node_c, node_b, node_a], 10),
        global
    );
    assert_eq!(
        index.search_candidates(
            "graph memory",
            &[node_c, node_b, node_a, node_a, NodeId::new(999_999)],
            10,
        ),
        global
    );
}

#[test]
fn text_index_candidate_search_dedups_and_ignores_unindexed_nodes() {
    let graph = SharedGraph::new(GraphId::new(433_007));
    let doc = db_string("TextCandidateDedupDoc");
    let body = db_string("body");
    let indexed;
    let non_string;
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        indexed = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("needle current memory"))),
            )
            .unwrap();
        non_string = mutator
            .create_node(LabelSet::single(doc.clone()), props(&body, Value::Int(7)))
            .unwrap();
        txn.commit().unwrap();
    }
    let index = graph.build_text_index(&doc, &body).unwrap();

    let hits = index.search_candidates(
        "needle",
        &[indexed, indexed, non_string, NodeId::new(999_999)],
        10,
    );

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, indexed);

    let sorted_hits =
        index.search_candidates("needle", &[indexed, non_string, NodeId::new(999_999)], 10);

    assert_eq!(sorted_hits.len(), 1);
    assert_eq!(sorted_hits[0].node_id, indexed);
}

#[test]
fn text_index_candidate_search_checked_observes_cancelled_token() {
    let graph = SharedGraph::new(GraphId::new(433_008));
    let doc = db_string("TextCandidateCancelDoc");
    let body = db_string("body");
    let indexed;
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        indexed = mutator
            .create_node(
                LabelSet::single(doc.clone()),
                props(&body, Value::String(db_string("graph memory"))),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let index = graph.build_text_index(&doc, &body).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let checker = CancellationChecker::new(Some(&token), None);

    let err = index
        .search_candidates_checked("graph", &[indexed], 10, checker)
        .expect_err("cancelled token should stop candidate search");

    assert!(matches!(err, TextSearchError::Cancelled));
}
