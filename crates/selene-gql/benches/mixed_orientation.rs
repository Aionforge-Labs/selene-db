#![allow(missing_docs)]
//! One-hop runtime expansion with matched node/edge identities and output size.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_core::{EdgeDirectionality, GraphId, LabelSet, PropertyMap, Value, db_string};
use selene_gql::{
    EmptyProcedureRegistry, OptimizeContext, TxContext, analyze, execute_pattern, optimize, parse,
    plan,
};
use selene_graph::SharedGraph;
use selene_testing::{BenchProfile, MockIndexCatalog};
use std::{hint::black_box, time::Duration};

fn fixture(degree: usize, mixed: bool) -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(907));
    let mut tx = graph.begin_write();
    let mut m = tx.mutator();
    let root = m
        .create_node(
            LabelSet::single(db_string("Root").unwrap()),
            PropertyMap::new(),
        )
        .unwrap();
    for i in 0..degree {
        let leaf = m
            .create_node(
                LabelSet::single(db_string("Leaf").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        let direction = if mixed && i % 2 == 0 {
            EdgeDirectionality::Undirected
        } else {
            EdgeDirectionality::Directed
        };
        m.create_mixed_edge(
            db_string("E").unwrap(),
            root,
            leaf,
            direction,
            PropertyMap::new(),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    graph
}

fn bench_one_hop(c: &mut Criterion) {
    let mut group = c.benchmark_group("gql_mixed_orientation");
    for degree in [8, 1024] {
        let directed = fixture(degree, false);
        let mixed = fixture(degree, true);
        for (name, graph, spelling) in [
            ("directed_right", &directed, "-[e]->"),
            ("directed_any", &directed, "-[e]-"),
            ("mixed_any", &mixed, "-[e]-"),
        ] {
            let statement = parse(&format!("MATCH (a:Root){spelling}(b:Leaf) RETURN e")).unwrap();
            let analyzed = analyze(statement, &EmptyProcedureRegistry, None).unwrap();
            let catalog = MockIndexCatalog::new().with_node_label_index(db_string("Root").unwrap());
            let plan = optimize(
                plan(&analyzed, &EmptyProcedureRegistry).unwrap(),
                &OptimizeContext::default().with_index_catalog(&catalog),
            );
            let ctx = TxContext::read_only(
                graph.read(),
                &plan.impl_defined_caps,
                &EmptyProcedureRegistry,
                graph.index_providers(),
            )
            .with_plan_metadata(&plan.expr_ids, &plan.subqueries);
            let pattern = plan.pattern_plan.as_ref().unwrap();
            let check = execute_pattern(pattern, &ctx).unwrap();
            assert_eq!(check.row_count(), degree);
            let mut identities = check
                .rows()
                .iter()
                .map(|row| {
                    row.values()
                        .iter()
                        .find_map(|v| match v {
                            Value::EdgeRef(e) => Some(e.get()),
                            _ => None,
                        })
                        .unwrap()
                })
                .collect::<Vec<_>>();
            identities.sort_unstable();
            assert_eq!(identities, (1..=degree as u64).collect::<Vec<_>>());
            group.throughput(Throughput::Elements(degree as u64));
            group.bench_with_input(BenchmarkId::new(name, degree), &degree, |b, _| {
                b.iter(|| black_box(execute_pattern(black_box(pattern), black_box(&ctx)).unwrap()));
            });
        }
    }
    group.finish();
}

fn config() -> Criterion {
    Criterion::default()
        .sample_size(BenchProfile::from_env().sample_size())
        .warm_up_time(Duration::from_secs(3))
        .measurement_time(Duration::from_secs(5))
}
criterion_group! { name = benches; config = config(); targets = bench_one_hop }
criterion_main!(benches);
