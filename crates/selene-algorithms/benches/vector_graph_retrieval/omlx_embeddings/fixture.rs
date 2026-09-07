//! Graph fixture built from local oMLX endpoint embeddings.

use std::collections::HashMap;
use std::sync::Arc;

use selene_core::{
    CancellationChecker, HnswIndexConfig, LabelSet, NodeId, PropertyMap, Value, VectorMetric,
    VectorValue,
};
use selene_graph::{
    ApproximateVectorSearchOptions, CandidateStateSpec, IndexProvider,
    MaintainedCandidateStateProvider, SeleneGraph, SharedGraph, TextIndex, VectorCandidateSet,
    VectorIndexConfig, VectorIndexKind, VectorNeighborDirection, VectorNeighborSearchOptions,
};

use self::build_support::{
    DocumentMeta, QueryAnchor, QueryVector, admits_graph_hint, graph_id_for_model,
    topic_hint_expansion_set_for,
};
use super::super::support::db_string;
use super::{ANN_SEARCH_WIDTH, TOP_K, precision_basis_points};
use selene_testing::local_omlx::{CorpusInput, Topic, topic_label};

mod bm25;
#[path = "fixture/build_support.rs"]
mod build_support;
#[path = "fixture/ivf.rs"]
mod ivf;
#[path = "fixture/target_hit.rs"]
mod target_hit;
#[path = "fixture/turbo_quant.rs"]
mod turbo_quant;

pub(super) struct OmlxVectorFixture {
    shared: SharedGraph,
    graph: SeleneGraph,
    label: selene_core::DbString,
    embedding_key: selene_core::DbString,
    ivf_embedding_key: selene_core::DbString,
    turbo_embedding_key: selene_core::DbString,
    dependency_edge: selene_core::DbString,
    support_edge: selene_core::DbString,
    support_state_name: selene_core::DbString,
    text_index: Arc<TextIndex>,
    pub(super) dimension: usize,
    documents: Vec<DocumentMeta>,
    topics_by_node: HashMap<NodeId, Topic>,
    target_by_node: HashMap<NodeId, &'static str>,
    queries: Vec<QueryVector>,
    expanded_hint_sets: Vec<VectorCandidateSet>,
}

impl OmlxVectorFixture {
    pub(super) fn build(
        model: &str,
        inputs: &[CorpusInput],
        vectors: Vec<VectorValue>,
        graph_hint_docs_per_topic: Option<usize>,
    ) -> Self {
        assert_eq!(
            vectors.len(),
            inputs.len(),
            "oMLX returned one vector per corpus input"
        );
        let dimension = vectors
            .first()
            .map(VectorValue::dimension)
            .expect("corpus has at least one vector");
        assert!(
            vectors.iter().all(|vector| vector.dimension() == dimension),
            "oMLX returned consistent vector dimensions"
        );
        let label = db_string("OmlxEmbeddingDoc");
        let query_label = db_string("OmlxQueryAnchor");
        let support_fact_label = db_string("OmlxSupportFact");
        let dependency_edge = db_string("OmlxDependsOn");
        let support_edge = db_string("OmlxSupports");
        let body_key = db_string("body");
        let embedding_key = db_string("embedding");
        let ivf_embedding_key = db_string("embedding_ivf");
        let turbo_embedding_key = db_string("embedding_turbo");
        let support_state_name = db_string("omlx_support_facts");
        let support_state_provider = Arc::new(
            MaintainedCandidateStateProvider::new([CandidateStateSpec::new(
                support_state_name.clone(),
            )
            .require_label(support_fact_label.clone())])
            .expect("oMLX maintained support state provider is valid"),
        );
        let shared = SharedGraph::builder(graph_id_for_model(model))
            .with_provider(support_state_provider as Arc<dyn IndexProvider>)
            .build()
            .expect("oMLX bench graph builds");
        let mut documents = Vec::new();
        let mut query_anchors = Vec::new();
        let mut graph_hint_counts = HashMap::<Topic, usize>::new();
        {
            let mut txn = shared.begin_write();
            {
                let mut mutator = txn.mutator();
                for (input, vector) in inputs.iter().zip(vectors.iter()) {
                    if !input.is_document {
                        continue;
                    }
                    let props = PropertyMap::from_pairs([
                        (body_key.clone(), Value::String(db_string(input.text()))),
                        (embedding_key.clone(), Value::Vector(vector.clone())),
                        (ivf_embedding_key.clone(), Value::Vector(vector.clone())),
                        (turbo_embedding_key.clone(), Value::Vector(vector.clone())),
                    ])
                    .expect("oMLX bench document properties fit");
                    let graph_hint = admits_graph_hint(
                        &mut graph_hint_counts,
                        input.topic,
                        graph_hint_docs_per_topic,
                    );
                    let mut labels = LabelSet::single(label.clone());
                    if graph_hint {
                        labels.insert(topic_label(input.topic));
                    }
                    if !graph_hint || graph_hint_docs_per_topic.is_none() {
                        labels.insert(support_fact_label.clone());
                    }
                    let node = mutator
                        .create_node(labels, props)
                        .expect("oMLX bench document node inserts");
                    documents.push(DocumentMeta {
                        node,
                        topic: input.topic,
                        graph_hint,
                        target_key: input.target_key,
                    });
                }
                for source in documents.iter().filter(|document| document.graph_hint) {
                    for target in documents
                        .iter()
                        .filter(|document| document.topic == source.topic)
                    {
                        if target.node == source.node {
                            continue;
                        }
                        mutator
                            .create_edge(
                                support_edge.clone(),
                                source.node,
                                target.node,
                                PropertyMap::new(),
                            )
                            .expect("oMLX bench support edge inserts");
                    }
                }
                for input in inputs.iter().filter(|input| !input.is_document) {
                    let anchor = mutator
                        .create_node(
                            LabelSet::from_iter([query_label.clone()]),
                            PropertyMap::new(),
                        )
                        .expect("oMLX bench query anchor inserts");
                    for document in documents
                        .iter()
                        .filter(|document| document.topic == input.topic && document.graph_hint)
                    {
                        mutator
                            .create_edge(
                                dependency_edge.clone(),
                                anchor,
                                document.node,
                                PropertyMap::new(),
                            )
                            .expect("oMLX bench query dependency edge inserts");
                    }
                    query_anchors.push(QueryAnchor {
                        node: anchor,
                        topic: input.topic,
                    });
                }
                mutator
                    .create_vector_index_named_with_configs(
                        label.clone(),
                        embedding_key.clone(),
                        VectorIndexKind::HnswCosine,
                        dimension as u32,
                        None,
                        VectorIndexConfig::new(Some(HnswIndexConfig::new(16, 64)), None),
                    )
                    .expect("oMLX bench HNSW index builds");
                mutator
                    .create_vector_index_named_with_configs(
                        label.clone(),
                        ivf_embedding_key.clone(),
                        VectorIndexKind::IvfCosine,
                        dimension as u32,
                        None,
                        VectorIndexConfig::default(),
                    )
                    .expect("oMLX bench IVF index builds");
                mutator
                    .create_vector_index_named_with_configs(
                        label.clone(),
                        turbo_embedding_key.clone(),
                        VectorIndexKind::TurboQuantCosine,
                        dimension as u32,
                        None,
                        VectorIndexConfig::default(),
                    )
                    .expect("oMLX bench TurboQuant index builds");
            }
            txn.commit().expect("oMLX bench graph commits");
        }
        shared
            .create_text_index(label.clone(), body_key.clone())
            .expect("oMLX bench text index registers");
        let mut query_anchors = query_anchors.into_iter();
        let queries: Vec<QueryVector> = inputs
            .iter()
            .zip(vectors)
            .filter_map(|(input, vector)| {
                (!input.is_document).then(|| {
                    let anchor = query_anchors
                        .next()
                        .expect("oMLX bench query anchor count matches queries");
                    debug_assert_eq!(anchor.topic, input.topic);
                    QueryVector {
                        anchor: anchor.node,
                        topic: input.topic,
                        text: input.text.clone(),
                        vector,
                        target_key: input.target_key,
                    }
                })
            })
            .collect();
        debug_assert!(
            query_anchors.next().is_none(),
            "oMLX bench consumed every query anchor"
        );
        let graph = shared.read().as_ref().clone();
        let text_index = graph
            .text_index_for(&label, &body_key)
            .expect("oMLX bench text index exists");
        let expanded_hint_sets = queries
            .iter()
            .map(|query| {
                topic_hint_expansion_set_for(&graph, &dependency_edge, &support_edge, query.anchor)
            })
            .collect();
        let topics_by_node = documents
            .iter()
            .map(|document| (document.node, document.topic))
            .collect();
        let target_by_node = documents
            .iter()
            .filter_map(|document| document.target_key.map(|target| (document.node, target)))
            .collect();
        Self {
            shared,
            graph,
            label,
            embedding_key,
            ivf_embedding_key,
            turbo_embedding_key,
            dependency_edge,
            support_edge,
            support_state_name,
            text_index,
            dimension,
            documents,
            topics_by_node,
            target_by_node,
            queries,
            expanded_hint_sets,
        }
    }

    pub(super) fn document_count(&self) -> usize {
        self.documents.len()
    }

    pub(super) fn query_count(&self) -> usize {
        self.queries.len()
    }

    pub(super) fn exact_total_precision(&self) -> usize {
        self.queries
            .iter()
            .map(|query| {
                let hits = self
                    .graph
                    .exact_vector_search_nodes_checked(
                        &self.label,
                        &self.embedding_key,
                        &query.vector,
                        VectorMetric::Cosine,
                        TOP_K,
                        CancellationChecker::disabled(),
                    )
                    .expect("oMLX exact vector search succeeds");
                self.precision(query.topic, hits.into_iter().map(|hit| hit.node_id))
            })
            .sum()
    }

    pub(super) fn ann_total_precision(&self) -> usize {
        self.queries
            .iter()
            .map(|query| {
                let hits = self
                    .graph
                    .approximate_vector_search_nodes_checked(
                        &self.label,
                        &self.embedding_key,
                        &query.vector,
                        ApproximateVectorSearchOptions::new(
                            VectorMetric::Cosine,
                            TOP_K,
                            ANN_SEARCH_WIDTH,
                        ),
                        CancellationChecker::disabled(),
                    )
                    .expect("oMLX ANN vector search succeeds");
                self.precision(query.topic, hits.into_iter().map(|hit| hit.node_id))
            })
            .sum()
    }

    pub(super) fn exact_precision_basis_points(&self) -> usize {
        precision_basis_points(self.exact_total_precision(), self.query_count() * TOP_K)
    }

    pub(super) fn ann_precision_basis_points(&self) -> usize {
        precision_basis_points(self.ann_total_precision(), self.query_count() * TOP_K)
    }

    pub(super) fn topic_candidate_total_precision(&self) -> usize {
        let queries = self
            .queries
            .iter()
            .map(|query| query.vector.clone())
            .collect::<Vec<_>>();
        let candidate_sets = self
            .queries
            .iter()
            .map(|query| self.topic_candidate_set(query.topic))
            .collect::<Vec<_>>();
        let hits = self
            .graph
            .score_vector_candidate_sets_batch_checked(
                &self.embedding_key,
                &queries,
                &candidate_sets,
                VectorMetric::Cosine,
                TOP_K,
                CancellationChecker::disabled(),
            )
            .expect("oMLX topic candidate scoring succeeds");
        self.queries
            .iter()
            .zip(hits)
            .map(|(query, hits)| {
                self.precision(query.topic, hits.into_iter().map(|hit| hit.node_id))
            })
            .sum()
    }

    pub(super) fn topic_neighbor_total_precision(&self) -> usize {
        let options = self.neighbor_options(TOP_K);
        self.queries
            .iter()
            .map(|query| {
                let hits = self
                    .graph
                    .score_vector_neighbors_checked(
                        &self.embedding_key,
                        &query.vector,
                        query.anchor,
                        options,
                        CancellationChecker::disabled(),
                    )
                    .expect("oMLX topic neighbor scoring succeeds");
                self.precision(query.topic, hits.into_iter().map(|hit| hit.node_id))
            })
            .sum()
    }

    pub(super) fn topic_neighbor_batch_total_precision(&self) -> usize {
        let queries = self
            .queries
            .iter()
            .map(|query| query.vector.clone())
            .collect::<Vec<_>>();
        let anchors = self
            .queries
            .iter()
            .map(|query| query.anchor)
            .collect::<Vec<_>>();
        let hits = self
            .graph
            .score_vector_neighbors_batch_checked(
                &self.embedding_key,
                &queries,
                &anchors,
                self.neighbor_options(TOP_K),
                CancellationChecker::disabled(),
            )
            .expect("oMLX topic neighbor batch scoring succeeds");
        self.queries
            .iter()
            .zip(hits)
            .map(|(query, hits)| {
                self.precision(query.topic, hits.into_iter().map(|hit| hit.node_id))
            })
            .sum()
    }

    pub(super) fn topic_label_ann_union_total_precision(&self) -> usize {
        self.candidate_sets_total_precision(|fixture, query| {
            fixture
                .topic_candidate_set(query.topic)
                .union(&fixture.ann_hit_set(query, super::ANN_UNION_SEED_K))
        })
    }

    pub(super) fn topic_neighbor_ann_union_total_precision(&self) -> usize {
        self.candidate_sets_total_precision(|fixture, query| {
            fixture
                .topic_neighbor_set(query)
                .union(&fixture.ann_hit_set(query, super::ANN_UNION_SEED_K))
        })
    }

    pub(super) fn topic_hint_expansion_total_precision(&self) -> usize {
        self.candidate_sets_total_precision(|fixture, query| {
            fixture.topic_hint_expansion_set(query)
        })
    }

    pub(super) fn topic_hint_expansion_ann_union_total_precision(&self) -> usize {
        self.candidate_sets_total_precision(|fixture, query| {
            fixture
                .topic_hint_expansion_set(query)
                .union(&fixture.ann_hit_set(query, super::ANN_UNION_SEED_K))
        })
    }

    pub(super) fn topic_hint_expansion_cached_total_precision(&self) -> usize {
        self.candidate_sets_total_precision_from_sets(&self.expanded_hint_sets)
    }

    pub(super) fn topic_hint_expansion_state_total_precision(&self) -> usize {
        let state = self.maintained_support_state();
        self.candidate_sets_total_precision(|fixture, query| {
            state.intersection(&fixture.topic_hint_expansion_set(query))
        })
    }

    pub(super) fn ann_hint_expansion_state_total_precision(&self) -> usize {
        let state = self.maintained_support_state();
        self.candidate_sets_total_precision(|fixture, query| {
            let roots = fixture.ann_hit_set(query, super::ANN_UNION_SEED_K);
            let expanded = fixture.graph.expand_vector_candidate_set(
                &roots,
                &fixture.support_edge,
                VectorNeighborDirection::Outgoing,
            );
            state.intersection(&expanded)
        })
    }

    pub(super) fn topic_hint_expansion_cached_mixed_read_refresh_work(
        &self,
        rounds: usize,
        reads_per_round: usize,
        refreshes_per_round: usize,
    ) -> usize {
        let mut total = 0usize;
        for _ in 0..rounds {
            for _ in 0..reads_per_round {
                total = total.wrapping_add(self.topic_hint_expansion_cached_total_precision());
            }
            for _ in 0..refreshes_per_round {
                total = total.wrapping_add(self.topic_hint_expansion_refresh_total_candidates());
            }
        }
        total
    }

    pub(super) fn topic_hint_expansion_refresh_total_candidates(&self) -> usize {
        let refreshed = self.refresh_expanded_hint_sets();
        assert_eq!(
            refreshed, self.expanded_hint_sets,
            "oMLX refreshed support candidates match cached candidates"
        );
        refreshed.iter().map(VectorCandidateSet::len).sum()
    }

    pub(super) fn topic_candidate_count(&self) -> usize {
        self.topic_candidate_set(Topic::Gql).len()
    }

    pub(super) fn topic_neighbor_count(&self) -> usize {
        self.queries
            .first()
            .map_or(0, |query| self.topic_neighbor_set(query).len())
    }

    pub(super) fn topic_label_ann_union_count(&self) -> usize {
        self.queries.first().map_or(0, |query| {
            self.topic_candidate_set(query.topic)
                .union(&self.ann_hit_set(query, super::ANN_UNION_SEED_K))
                .len()
        })
    }

    pub(super) fn topic_neighbor_ann_union_count(&self) -> usize {
        self.queries.first().map_or(0, |query| {
            self.topic_neighbor_set(query)
                .union(&self.ann_hit_set(query, super::ANN_UNION_SEED_K))
                .len()
        })
    }

    pub(super) fn topic_hint_expansion_count(&self) -> usize {
        self.queries
            .first()
            .map_or(0, |query| self.topic_hint_expansion_set(query).len())
    }

    pub(super) fn topic_hint_expansion_ann_union_count(&self) -> usize {
        self.queries.first().map_or(0, |query| {
            self.topic_hint_expansion_set(query)
                .union(&self.ann_hit_set(query, super::ANN_UNION_SEED_K))
                .len()
        })
    }

    pub(super) fn topic_hint_expansion_cached_count(&self) -> usize {
        self.expanded_hint_sets
            .first()
            .map_or(0, VectorCandidateSet::len)
    }

    pub(super) fn topic_hint_expansion_state_count(&self) -> usize {
        self.queries.first().map_or(0, |query| {
            self.maintained_support_state()
                .intersection(&self.topic_hint_expansion_set(query))
                .len()
        })
    }

    pub(super) fn ann_hint_expansion_state_count(&self) -> usize {
        self.queries.first().map_or(0, |query| {
            let roots = self.ann_hit_set(query, super::ANN_UNION_SEED_K);
            let expanded = self.graph.expand_vector_candidate_set(
                &roots,
                &self.support_edge,
                VectorNeighborDirection::Outgoing,
            );
            self.maintained_support_state()
                .intersection(&expanded)
                .len()
        })
    }

    pub(super) fn topic_hint_expansion_refresh_count(&self) -> usize {
        self.refresh_expanded_hint_sets()
            .first()
            .map_or(0, VectorCandidateSet::len)
    }

    fn candidate_sets_total_precision<F>(&self, candidate_set_for: F) -> usize
    where
        F: Fn(&Self, &QueryVector) -> VectorCandidateSet,
    {
        let candidate_sets = self
            .queries
            .iter()
            .map(|query| candidate_set_for(self, query))
            .collect::<Vec<_>>();
        self.candidate_sets_total_precision_from_sets(&candidate_sets)
    }

    fn candidate_sets_total_precision_from_sets(
        &self,
        candidate_sets: &[VectorCandidateSet],
    ) -> usize {
        let queries = self
            .queries
            .iter()
            .map(|query| query.vector.clone())
            .collect::<Vec<_>>();
        let hits = self
            .graph
            .score_vector_candidate_sets_batch_checked(
                &self.embedding_key,
                &queries,
                candidate_sets,
                VectorMetric::Cosine,
                TOP_K,
                CancellationChecker::disabled(),
            )
            .expect("oMLX ANN-union candidate scoring succeeds");
        self.queries
            .iter()
            .zip(hits)
            .map(|(query, hits)| {
                self.precision(query.topic, hits.into_iter().map(|hit| hit.node_id))
            })
            .sum()
    }

    fn topic_candidate_set(&self, topic: Topic) -> VectorCandidateSet {
        let topic_label = topic_label(topic);
        let Ok(candidates) = self.graph.node_candidates_with_label(&topic_label) else {
            return VectorCandidateSet::default();
        };
        VectorCandidateSet::from_nodes(candidates.iter())
    }

    fn topic_neighbor_set(&self, query: &QueryVector) -> VectorCandidateSet {
        self.graph.vector_neighbor_candidates(
            query.anchor,
            &self.dependency_edge,
            VectorNeighborDirection::Outgoing,
        )
    }

    fn topic_hint_expansion_set(&self, query: &QueryVector) -> VectorCandidateSet {
        topic_hint_expansion_set_for(
            &self.graph,
            &self.dependency_edge,
            &self.support_edge,
            query.anchor,
        )
    }

    fn refresh_expanded_hint_sets(&self) -> Vec<VectorCandidateSet> {
        self.queries
            .iter()
            .map(|query| self.topic_hint_expansion_set(query))
            .collect()
    }

    fn ann_hit_set(&self, query: &QueryVector, k: usize) -> VectorCandidateSet {
        let hits = self
            .graph
            .approximate_vector_search_nodes_checked(
                &self.label,
                &self.embedding_key,
                &query.vector,
                ApproximateVectorSearchOptions::new(VectorMetric::Cosine, k, ANN_SEARCH_WIDTH),
                CancellationChecker::disabled(),
            )
            .expect("oMLX ANN hit-set search succeeds");
        VectorCandidateSet::from_search_hits(hits)
    }

    fn maintained_support_state(&self) -> VectorCandidateSet {
        self.shared
            .vector_candidate_set(&self.support_state_name)
            .expect("oMLX maintained support state is generation current")
            .expect("oMLX maintained support state provider is configured")
    }

    fn neighbor_options(&self, k: usize) -> VectorNeighborSearchOptions<'_> {
        VectorNeighborSearchOptions::new(
            &self.dependency_edge,
            VectorNeighborDirection::Outgoing,
            VectorMetric::Cosine,
            k,
        )
    }

    fn precision<I>(&self, topic: Topic, hits: I) -> usize
    where
        I: IntoIterator<Item = NodeId>,
    {
        hits.into_iter()
            .filter(|node| {
                self.topics_by_node
                    .get(node)
                    .is_some_and(|hit_topic| *hit_topic == topic)
            })
            .count()
    }
}
