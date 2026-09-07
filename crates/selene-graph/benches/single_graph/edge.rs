use criterion::{BenchmarkId, Criterion, Throughput};
use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value, db_string};
use selene_graph::{SeleneGraph, SharedGraph, TypedIndexKind};
use selene_testing::BenchProfile;

const POINTS_PER_BLOCK: usize = 4;

pub(super) fn bench_edge_property_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_edge_property_scan");
    for fixture in edge_fixtures(false) {
        group.throughput(Throughput::Elements(fixture.connected_edge_count() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(fixture.scale()),
            &fixture,
            |b, fixture| {
                b.iter(|| std::hint::black_box(fixture.edge_property_scan_count()));
            },
        );
    }
    group.finish();
}

pub(super) fn bench_edge_property_index_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_edge_property_index_lookup");
    for fixture in edge_fixtures(true) {
        group.throughput(Throughput::Elements(fixture.connected_edge_count() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(fixture.scale()),
            &fixture,
            |b, fixture| {
                b.iter(|| std::hint::black_box(fixture.edge_property_index_lookup_count()));
            },
        );
    }
    group.finish();
}

pub(super) fn bench_point_connected_traversal(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_point_connected_traversal");
    for fixture in edge_fixtures(false) {
        group.throughput(Throughput::Elements(fixture.point_count() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(fixture.scale()),
            &fixture,
            |b, fixture| {
                b.iter(|| std::hint::black_box(fixture.point_connected_traversal_count()));
            },
        );
    }
    group.finish();
}

fn edge_fixtures(register_edge_index: bool) -> Vec<EdgeControlFixture> {
    BenchProfile::from_env()
        .scales()
        .iter()
        .copied()
        .map(|scale| EdgeControlFixture::build(scale, register_edge_index))
        .collect()
}

#[derive(Clone, Debug)]
struct EdgeControlFixture {
    scale: usize,
    graph: SeleneGraph,
    point_label: DbString,
    connected_to_label: DbString,
    from_port_key: DbString,
    from_port_value: DbString,
    from_port_probe: Value,
    point_kind_key: DbString,
    output_kind: DbString,
    input_kind: DbString,
}

impl EdgeControlFixture {
    fn build(scale: usize, register_edge_index: bool) -> Self {
        let point_count = scale.max(1);
        let block_count = point_count.div_ceil(POINTS_PER_BLOCK);
        let block_label = db("CdlBlock");
        let point_label = db("Point");
        let exposes_label = db("EXPOSES");
        let connected_to_label = db("CONNECTED_TO");
        let bench_id_key = db("bench_id");
        let block_id_key = db("block_id");
        let point_kind_key = db("kind");
        let from_port_key = db("from_port");
        let to_port_key = db("to_port");
        let output_kind = db("output");
        let input_kind = db("input");
        let from_port_value = db("out_0");
        let shared = SharedGraph::new(GraphId::new(1));
        let mut blocks = Vec::with_capacity(block_count);
        let mut points = Vec::with_capacity(point_count);
        {
            let mut txn = shared.begin_write();
            let mut mutator = txn.mutator();
            for idx in 0..block_count {
                let props =
                    PropertyMap::from_pairs([(bench_id_key.clone(), Value::Int(idx as i64))])
                        .expect("block fixture properties fit core caps");
                let block = mutator
                    .create_node(LabelSet::single(block_label.clone()), props)
                    .expect("block create succeeds");
                blocks.push(block);
            }
            for idx in 0..point_count {
                let block = blocks[idx / POINTS_PER_BLOCK];
                let is_output = idx % POINTS_PER_BLOCK == 0;
                let props = PropertyMap::from_pairs([
                    (bench_id_key.clone(), Value::Int(idx as i64)),
                    (
                        block_id_key.clone(),
                        Value::Int((idx / POINTS_PER_BLOCK) as i64),
                    ),
                    (
                        point_kind_key.clone(),
                        Value::String(if is_output {
                            output_kind.clone()
                        } else {
                            input_kind.clone()
                        }),
                    ),
                ])
                .expect("point fixture properties fit core caps");
                let point = mutator
                    .create_node(LabelSet::single(point_label.clone()), props)
                    .expect("point create succeeds");
                mutator
                    .create_edge(exposes_label.clone(), block, point, PropertyMap::new())
                    .expect("exposes edge create succeeds");
                points.push(point);
            }
            for idx in 0..point_count {
                let target = points[(idx + POINTS_PER_BLOCK) % point_count];
                let props = PropertyMap::from_pairs([
                    (from_port_key.clone(), Value::String(port_name(idx, "out"))),
                    (to_port_key.clone(), Value::String(port_name(idx, "in"))),
                ])
                .expect("connected edge properties fit core caps");
                mutator
                    .create_edge(connected_to_label.clone(), points[idx], target, props)
                    .expect("connected edge create succeeds");
            }
            txn.commit().expect("edge fixture commit succeeds");
        }
        if register_edge_index {
            shared
                .create_edge_property_index(
                    connected_to_label.clone(),
                    from_port_key.clone(),
                    TypedIndexKind::String,
                )
                .expect("edge property index builds");
        }
        Self {
            scale,
            graph: shared.read().as_ref().clone(),
            point_label,
            connected_to_label,
            from_port_key,
            from_port_value: from_port_value.clone(),
            from_port_probe: Value::String(from_port_value),
            point_kind_key,
            output_kind,
            input_kind,
        }
    }

    const fn scale(&self) -> usize {
        self.scale
    }

    fn point_count(&self) -> usize {
        self.graph.node_label_cardinality(&self.point_label) as usize
    }

    fn connected_edge_count(&self) -> usize {
        self.graph.edge_label_cardinality(&self.connected_to_label) as usize
    }

    fn edge_property_scan_count(&self) -> usize {
        let Ok(candidates) = self
            .graph
            .edge_candidates_with_label(&self.connected_to_label)
        else {
            return 0;
        };
        candidates
            .iter()
            .filter(|&edge_id| {
                self.graph.edge_properties(edge_id).is_some_and(|props| {
                    matches!(
                        props.get(&self.from_port_key),
                        Some(Value::String(port)) if port == &self.from_port_value
                    )
                })
            })
            .count()
    }

    fn edge_property_index_lookup_count(&self) -> u64 {
        self.graph
            .edge_property_eq_cardinality(
                &self.connected_to_label,
                &self.from_port_key,
                &self.from_port_probe,
            )
            .unwrap_or(0)
    }

    fn point_connected_traversal_count(&self) -> usize {
        let Ok(candidates) = self.graph.node_candidates_with_label(&self.point_label) else {
            return 0;
        };
        candidates
            .iter()
            .map(|node| self.output_connected_inputs(node))
            .sum()
    }

    fn output_connected_inputs(&self, node: NodeId) -> usize {
        if !self.node_kind_is(node, &self.output_kind) {
            return 0;
        }
        self.graph.outgoing_edges(node).map_or(0, |entry| {
            entry
                .iter_label(&self.connected_to_label)
                .filter(|edge| self.node_kind_is(edge.neighbor, &self.input_kind))
                .count()
        })
    }

    fn node_kind_is(&self, node: NodeId, expected: &DbString) -> bool {
        self.graph.node_properties(node).is_some_and(|props| {
            matches!(
                props.get(&self.point_kind_key),
                Some(Value::String(kind)) if kind == expected
            )
        })
    }
}

fn port_name(idx: usize, prefix: &str) -> DbString {
    db(&format!("{prefix}_{}", idx % POINTS_PER_BLOCK))
}

fn db(value: &str) -> DbString {
    db_string(value).expect("fixture string fits DB string cap")
}
