//! Deterministic graph fixtures for benchmark binaries.

use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value};
use selene_graph::{SeleneGraph, SharedGraph, TypedIndexKind};

/// Canonical publish-quality graph-size scales for release-prep benchmarks.
pub const BENCHMARK_SCALES: &[usize] = &[10_000, 50_000, 100_000];

/// Deterministic in-memory graph fixture used by benchmark binaries.
///
/// The graph contains `scale` nodes, roughly `3 * scale` directed edges, a
/// small fixed label vocabulary, and built-in typed indexes for hot property
/// lookups. The generator avoids filesystem state and caches so repeated
/// calls with the same scale produce the same graph shape.
#[derive(Clone, Debug)]
pub struct BenchFixture {
    scale: usize,
    graph: SeleneGraph,
    sample_node_id: NodeId,
    person_label: DbString,
    sensor_label: DbString,
    edge_label: DbString,
    age_key: DbString,
    bench_id_key: DbString,
    name_key: DbString,
    score_key: DbString,
    sample_age: i64,
    sample_name: DbString,
}

impl BenchFixture {
    /// Build the canonical benchmark graph at `scale` nodes.
    #[must_use]
    pub fn build(scale: usize) -> Self {
        let scale = scale.max(1);
        let person_label = db_string("Person");
        let sensor_label = db_string("Sensor");
        let device_label = db_string("Device");
        let edge_label = db_string("KNOWS");
        let age_key = db_string("age");
        let bench_id_key = db_string("bench_id");
        let name_key = db_string("name");
        let score_key = db_string("score");
        let shared = SharedGraph::new(GraphId::new(1));
        {
            let mut txn = shared.begin_write();
            let mut mutator = txn.mutator();
            for idx in 0..scale {
                let label = match idx % 3 {
                    0 => person_label.clone(),
                    1 => sensor_label.clone(),
                    _ => device_label.clone(),
                };
                let props = PropertyMap::from_pairs([
                    (age_key.clone(), Value::Int(sample_age(idx))),
                    (bench_id_key.clone(), Value::Int(idx as i64)),
                    (name_key.clone(), Value::String(sample_name(idx))),
                    (score_key.clone(), Value::Int((idx % 1_024) as i64)),
                ])
                .expect("fixture properties fit core caps");
                mutator
                    .create_node(LabelSet::single(label), props)
                    .expect("fixture node insert succeeds");
            }
            for idx in 0..scale {
                let source = NodeId::new(idx as u64 + 1);
                for offset in [1_usize, 7, 31] {
                    let target = NodeId::new(((idx + offset) % scale) as u64 + 1);
                    mutator
                        .create_edge(edge_label.clone(), source, target, PropertyMap::new())
                        .expect("fixture edge insert succeeds");
                }
            }
            txn.commit().expect("fixture commit succeeds");
        }
        shared
            .create_property_index(person_label.clone(), age_key.clone(), TypedIndexKind::I64)
            .expect("age index builds");
        shared
            .create_property_index(
                person_label.clone(),
                name_key.clone(),
                TypedIndexKind::String,
            )
            .expect("name index builds");
        shared
            .create_property_index(
                person_label.clone(),
                bench_id_key.clone(),
                TypedIndexKind::I64,
            )
            .expect("bench_id index builds");
        let graph = shared.read().as_ref().clone();
        Self {
            scale,
            graph,
            sample_node_id: NodeId::new(1),
            person_label,
            sensor_label,
            edge_label,
            age_key,
            bench_id_key,
            name_key,
            score_key,
            sample_age: sample_age(0),
            sample_name: sample_name(0),
        }
    }

    /// Borrow the immutable graph snapshot.
    #[must_use]
    pub const fn graph(&self) -> &SeleneGraph {
        &self.graph
    }

    /// Return the requested node scale.
    #[must_use]
    pub const fn scale(&self) -> usize {
        self.scale
    }

    /// Return a node ID known to exist.
    #[must_use]
    pub const fn sample_node_id(&self) -> NodeId {
        self.sample_node_id
    }

    /// Return the primary node label used by index benchmarks.
    #[must_use]
    pub fn person_label(&self) -> DbString {
        self.person_label.clone()
    }

    /// Return the secondary node label used by label-scan benchmarks.
    #[must_use]
    pub fn sensor_label(&self) -> DbString {
        self.sensor_label.clone()
    }

    /// Return the canonical edge label.
    #[must_use]
    pub fn edge_label(&self) -> DbString {
        self.edge_label.clone()
    }

    /// Return the indexed integer property key.
    #[must_use]
    pub fn age_key(&self) -> DbString {
        self.age_key.clone()
    }

    /// Return the unique integer lookup key used by write benchmarks.
    #[must_use]
    pub fn bench_id_key(&self) -> DbString {
        self.bench_id_key.clone()
    }

    /// Return the indexed string property key.
    #[must_use]
    pub fn name_key(&self) -> DbString {
        self.name_key.clone()
    }

    /// Return a non-indexed integer property key.
    #[must_use]
    pub fn score_key(&self) -> DbString {
        self.score_key.clone()
    }

    /// Return an age value known to be present for [`Self::person_label`].
    #[must_use]
    pub const fn sample_age_value(&self) -> i64 {
        self.sample_age
    }

    /// Return a name value known to be present for [`Self::person_label`].
    #[must_use]
    pub fn sample_name_value(&self) -> DbString {
        self.sample_name.clone()
    }

    /// Return one registered `(label, property)` pair for index lookup.
    #[must_use]
    pub fn sample_label_property(&self) -> (DbString, DbString) {
        (self.person_label.clone(), self.age_key.clone())
    }
}

/// Build a deterministic **star / hub** graph: one center node with `degree`
/// outgoing edges to `degree` distinct leaf nodes.
///
/// Returned as an immutable snapshot so a bench can `SharedGraph::from_graph`
/// it per iteration. The center is `NodeId(1)` (the first node created), so a
/// bench deletes `NodeId::new(1)` to detach all `degree` incident edges at once
/// — the high-degree hub the uniform ~degree-6 [`BenchFixture`] never builds,
/// and the O(D²) adjacency-removal vector (GRAPH-05) that `delete_only` (which
/// deletes edgeless nodes) cannot exercise.
#[must_use]
pub fn star_graph(degree: usize) -> SeleneGraph {
    let center_label = db_string("Hub");
    let leaf_label = db_string("Leaf");
    let edge_label = db_string("LINK");
    let shared = SharedGraph::new(GraphId::new(1));
    {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let center = mutator
            .create_node(LabelSet::single(center_label), PropertyMap::new())
            .expect("hub center node create succeeds");
        for _ in 0..degree {
            let leaf = mutator
                .create_node(LabelSet::single(leaf_label.clone()), PropertyMap::new())
                .expect("hub leaf node create succeeds");
            mutator
                .create_edge(edge_label.clone(), center, leaf, PropertyMap::new())
                .expect("hub edge create succeeds");
        }
        txn.commit().expect("hub fixture commit succeeds");
    }
    shared.read().as_ref().clone()
}

/// Canonical write-side GQL corpus used by benchmark binaries.
pub struct WriteCorpus;

impl WriteCorpus {
    /// Single-node insert with scalar properties.
    #[must_use]
    pub const fn insert_single_node() -> &'static str {
        "INSERT (:Person { name: 'x', score: 42 })"
    }

    /// Indexed lookup plus node/edge insert.
    #[must_use]
    pub const fn insert_node_with_edge() -> &'static str {
        "MATCH (a:Person) FILTER a.bench_id = 42 INSERT (a)-[:KNOWS]->(:Person { name: 'x' })"
    }

    /// Indexed lookup plus scalar property update.
    #[must_use]
    pub const fn match_set() -> &'static str {
        "MATCH (n:Person) FILTER n.bench_id = 42 SET n.score = 17"
    }

    /// Indexed lookup plus detach-delete.
    #[must_use]
    pub const fn match_delete() -> &'static str {
        "MATCH (n:Person) FILTER n.bench_id = 42 DETACH DELETE n"
    }

    /// Explicit transaction statement sequence for multi-statement benches.
    #[must_use]
    pub const fn multi_statement_txn() -> &'static [&'static str] {
        &[
            "START TRANSACTION",
            "INSERT (:Person { name: 'a' })",
            "INSERT (:Person { name: 'b' })",
            "INSERT (:Person { name: 'c' })",
            "COMMIT",
        ]
    }
}

fn sample_age(idx: usize) -> i64 {
    20 + (idx % 80) as i64
}

fn sample_name(idx: usize) -> DbString {
    selene_core::db_string(&format!("bench-name-{}", idx % 256))
        .expect("fixture string fits DB string cap")
}

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("fixture string fits DB string cap")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use selene_gql::{EmptyProcedureRegistry, StatementCategory, analyze, parse};

    #[test]
    fn benchmark_scales_constant_is_pinned() {
        assert_eq!(BENCHMARK_SCALES, &[10_000, 50_000, 100_000]);
    }

    #[test]
    fn fixture_is_deterministic() {
        let a = BenchFixture::build(1_000);
        let b = BenchFixture::build(1_000);
        assert_eq!(a.graph().node_count(), b.graph().node_count());
        assert_eq!(a.graph().edge_count(), b.graph().edge_count());
        assert_eq!(a.graph().label_count(), b.graph().label_count());
        assert_eq!(
            a.graph().property_index_count(),
            b.graph().property_index_count()
        );
        assert_eq!(
            a.graph().node_property_eq_cardinality(
                &a.person_label(),
                &a.age_key(),
                &Value::Int(a.sample_age_value())
            ),
            b.graph().node_property_eq_cardinality(
                &b.person_label(),
                &b.age_key(),
                &Value::Int(b.sample_age_value())
            )
        );
    }

    #[test]
    fn fixture_scales_correctly() {
        for scale in [100, 1_000, 5_000] {
            let fixture = BenchFixture::build(scale);
            assert_eq!(fixture.graph().node_count(), scale);
            assert_eq!(fixture.graph().edge_count(), scale * 3);
        }
    }

    #[test]
    fn bench_fixture_assigns_unique_bench_id() {
        let fixture = BenchFixture::build(100);
        let mut ids = BTreeSet::new();
        for raw in 1..=100 {
            let node = NodeId::new(raw);
            let value = fixture
                .graph()
                .node_properties(node)
                .and_then(|properties| properties.get(&fixture.bench_id_key()))
                .expect("bench_id property exists");
            let Value::Int(bench_id) = value else {
                panic!("bench_id should be an integer, got {value:?}");
            };
            assert!(ids.insert(*bench_id));
        }
        assert_eq!(ids.len(), 100);
    }

    #[test]
    fn write_corpus_statements_parse() {
        for source in write_corpus_sources() {
            parse(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
        }
    }

    #[test]
    fn write_corpus_categories_are_writes() {
        for source in write_corpus_sources() {
            let statement =
                parse(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
            let analyzed = analyze(statement, &EmptyProcedureRegistry, None)
                .unwrap_or_else(|error| panic!("{source} should analyze: {error}"));
            assert!(
                matches!(
                    analyzed.category,
                    StatementCategory::DataModifying | StatementCategory::TransactionControl
                ),
                "{source} should be data-modifying or transaction-control"
            );
        }
    }

    fn write_corpus_sources() -> Vec<&'static str> {
        let mut sources = vec![
            WriteCorpus::insert_single_node(),
            WriteCorpus::insert_node_with_edge(),
            WriteCorpus::match_set(),
            WriteCorpus::match_delete(),
        ];
        sources.extend_from_slice(WriteCorpus::multi_statement_txn());
        sources
    }
}
