//! Native call boundary and projection costs; no numerical-kernel speedup claim.

use criterion::Criterion;
use selene_algorithms::{GraphProjection, ProjectionCatalog, ProjectionConfig};
use selene_core::{GraphId, LabelSet, PropertyMap, db_string};
use selene_gql::{BuiltinProcedureRegistry, CallPlanCache, Session};
use selene_graph::SharedGraph;
use std::{hint::black_box, num::NonZeroUsize, sync::Arc};

pub(super) fn bench_native_calls(c: &mut Criterion) {
    let mut group = c.benchmark_group("procedure_native_call");
    for nodes in [8, 64] {
        let graph = SharedGraph::new(GraphId::new(81_000 + nodes));
        let mut tx = graph.begin_write();
        for _ in 0..nodes {
            tx.mutator()
                .create_node(
                    LabelSet::single(db_string("N").unwrap()),
                    PropertyMap::new(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
        let registry = BuiltinProcedureRegistry::new();
        let cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(32).unwrap()));
        let mut session = Session::new(&graph).with_call_plan_cache(cache);
        session
            .execute_source(
                "CALL algo.projection_build('p', NULL, NULL, NULL)",
                &registry,
            )
            .unwrap();
        const ONE: &str = "CALL algo.wcc_count('p') YIELD count";
        const MANY: &str =
            "MATCH (n:N) CALL algo.wcc_count('p') YIELD count AS components RETURN components";
        session.execute_source(ONE, &registry).unwrap();
        session.execute_source(MANY, &registry).unwrap();
        group.bench_function(format!("small_query/{nodes}"), |b| {
            b.iter(|| black_box(session.execute_source(ONE, &registry).unwrap()))
        });
        group.bench_function(format!("per_statement/{nodes}"), |b| {
            b.iter(|| {
                for _ in 0..nodes {
                    black_box(session.execute_source(ONE, &registry).unwrap());
                }
            })
        });
        group.bench_function(format!("input_batch/{nodes}"), |b| {
            b.iter(|| black_box(session.execute_source(MANY, &registry).unwrap()))
        });
        let planned = |source| {
            let analyzed =
                selene_gql::analyze(selene_gql::parse(source).unwrap(), &registry, None).unwrap();
            selene_gql::plan(&analyzed, &registry).unwrap()
        };
        let one = planned(ONE);
        let many = planned(MANY);
        group.bench_function(format!("preplanned_statements/{nodes}"), |b| {
            b.iter(|| {
                for _ in 0..nodes {
                    black_box(
                        selene_gql::execute_statement(&one, &mut session, &registry).unwrap(),
                    );
                }
            })
        });
        group.bench_function(format!("preplanned_batch/{nodes}"), |b| {
            b.iter(|| {
                black_box(selene_gql::execute_statement(&many, &mut session, &registry).unwrap())
            })
        });
        let snapshot = graph.read();
        let config = ProjectionConfig {
            name: "p".into(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        };
        let catalog = ProjectionCatalog::new();
        catalog.project(&snapshot, &config).unwrap();
        group.bench_function(format!("projection_build/{nodes}"), |b| {
            b.iter(|| black_box(GraphProjection::build(&snapshot, &config, None).unwrap()))
        });
        group.bench_function(format!("projection_reuse/{nodes}"), |b| {
            b.iter(|| black_box(catalog.resolve(&snapshot, "p").unwrap()))
        });
    }
    group.finish();
}
