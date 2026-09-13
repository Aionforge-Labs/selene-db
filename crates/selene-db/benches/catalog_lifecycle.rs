#![allow(missing_docs)]
//! Resolve, list, lifecycle publication, and named-graph selection costs.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::{hint::black_box, sync::Arc, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_db::{
    AllowAllAuthorizationPolicy, AuthHookError, AuthorizationId, CreatePolicy, Database,
    DropPolicy, EdgeId, ExecutionOutcome, GeneralParameter, NodeId, ObjectPath, Principal,
    PrincipalId, PrincipalProvider, Request, RequestParams, SchemaPath, SessionOptions,
    TransactionAccessMode, Type, Value,
};

const OMITTED: ExecutionOutcome = ExecutionOutcome::SUCCESSFUL_OMITTED;

#[path = "catalog_lifecycle/declarations.rs"]
mod declarations;

#[path = "catalog_lifecycle/constraints.rs"]
mod constraints;
#[path = "catalog_lifecycle/runtime_bindings.rs"]
mod runtime_bindings;

struct BenchmarkPrincipalProvider {
    principal: Principal,
}

impl PrincipalProvider for BenchmarkPrincipalProvider {
    fn resolve(
        &self,
        _authorization_id: &AuthorizationId,
    ) -> Result<Option<Principal>, AuthHookError> {
        Ok(Some(self.principal.clone()))
    }
}

fn schema(name: impl AsRef<str>) -> SchemaPath {
    SchemaPath::regular("selene", name.as_ref()).expect("benchmark schema path is valid")
}

fn graph(schema: &str, name: impl AsRef<str>) -> ObjectPath {
    ObjectPath::regular("selene", schema, name.as_ref()).expect("benchmark graph path is valid")
}

fn catalog_fixture(schema_count: usize) -> Database {
    let database = Database::builder().build();
    let catalog = database.catalog();
    for index in 0..schema_count {
        catalog
            .create_schema(&schema(format!("schema_{index:05}")), CreatePolicy::Strict)
            .expect("benchmark schema creation succeeds");
    }
    database
}

fn graph_fixture(graph_count: usize) -> Database {
    let database = Database::builder().build();
    let catalog = database.catalog();
    catalog
        .create_schema(&schema("graphs"), CreatePolicy::Strict)
        .expect("benchmark graph schema creation succeeds");
    for index in 0..graph_count {
        catalog
            .create_graph(
                &graph("graphs", format!("graph_{index:05}")),
                None,
                CreatePolicy::Strict,
            )
            .expect("benchmark graph creation succeeds");
    }
    database
}

fn catalog_scales() -> &'static [usize] {
    match std::env::var("SELENE_BENCH_PROFILE").ok().as_deref() {
        Some("full") | Some("stress") => &[16, 256, 1_024],
        _ => &[16, 256],
    }
}

fn graph_scales() -> &'static [usize] {
    match std::env::var("SELENE_BENCH_PROFILE").ok().as_deref() {
        Some("full") | Some("stress") => &[4, 32, 128],
        _ => &[4, 32],
    }
}

fn criterion_config() -> Criterion {
    let (samples, measurement_ms) = match std::env::var("SELENE_BENCH_PROFILE").ok().as_deref() {
        Some("full") | Some("stress") => (30, 1_500),
        _ => (10, 500),
    };
    Criterion::default()
        .sample_size(samples)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(measurement_ms))
}

fn bench_catalog_lifecycle(c: &mut Criterion) {
    let mut reads = c.benchmark_group("catalog_lifecycle/read");
    for &scale in catalog_scales() {
        let database = catalog_fixture(scale);
        let catalog = database.catalog();
        let target = schema(format!("schema_{:05}", scale / 2));
        reads.throughput(Throughput::Elements(1));
        reads.bench_with_input(BenchmarkId::new("resolve_schema", scale), &scale, |b, _| {
            b.iter(|| {
                black_box(
                    black_box(&catalog)
                        .snapshot()
                        .resolve_schema(black_box(&target))
                        .expect("benchmark schema exists"),
                )
            });
        });
        reads.bench_with_input(BenchmarkId::new("list_schemas", scale), &scale, |b, _| {
            b.iter(|| {
                black_box(
                    black_box(&catalog)
                        .snapshot()
                        .schemas()
                        .expect("benchmark catalog is valid"),
                )
            });
        });
        reads.bench_with_input(
            BenchmarkId::new("clone_outer_snapshot", scale),
            &scale,
            |b, _| {
                let snapshot = catalog.snapshot();
                b.iter(|| black_box(black_box(&snapshot).clone()));
            },
        );
    }
    reads.finish();

    let mut publication = c.benchmark_group("catalog_lifecycle/outer_snapshot_publication");
    for &scale in catalog_scales() {
        let database = catalog_fixture(scale);
        let catalog = database.catalog();
        let target = schema("publication_target");
        publication.bench_with_input(BenchmarkId::new("create_schema", scale), &scale, |b, _| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let started = std::time::Instant::now();
                    black_box(
                        catalog
                            .create_schema(&target, CreatePolicy::Strict)
                            .expect("timed schema create succeeds"),
                    );
                    elapsed += started.elapsed();
                    catalog
                        .drop_schema(&target, DropPolicy::Strict)
                        .expect("untimed schema cleanup succeeds");
                }
                elapsed
            });
        });
        catalog
            .create_schema(&target, CreatePolicy::Strict)
            .expect("drop fixture schema creation succeeds");
        publication.bench_with_input(BenchmarkId::new("drop_schema", scale), &scale, |b, _| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let started = std::time::Instant::now();
                    black_box(
                        catalog
                            .drop_schema(&target, DropPolicy::Strict)
                            .expect("timed schema drop succeeds"),
                    );
                    elapsed += started.elapsed();
                    catalog
                        .create_schema(&target, CreatePolicy::Strict)
                        .expect("untimed schema recreation succeeds");
                }
                elapsed
            });
        });
    }
    publication.finish();

    // GQL database-catalog DDL through a selected fixture graph at the same
    // schema scales as the Rust-API publication rows.
    let mut gql = c.benchmark_group("catalog_lifecycle/gql_ddl");
    for &scale in catalog_scales() {
        let database = catalog_fixture(scale);
        let catalog = database.catalog();
        let session_path = graph("schema_00000", "catalog_session");
        catalog
            .create_graph(&session_path, None, CreatePolicy::Strict)
            .expect("selected-session fixture graph creation succeeds");
        let session = database
            .session(&session_path)
            .expect("selected-session fixture resolves");
        let schema_target = schema("gql_target");
        let graph_target = graph("gql_target", "g");
        let graph_type_target = graph("gql_target", "shape");
        let bound_graph_target = graph("gql_target", "typed_g");
        gql.bench_with_input(
            BenchmarkId::new("create_schema_gql", scale),
            &scale,
            |b, _| {
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let started = std::time::Instant::now();
                        let outcome = session
                            .execute("CREATE SCHEMA /gql_target")
                            .expect("timed GQL schema create succeeds");
                        elapsed += started.elapsed();
                        assert_eq!(black_box(outcome), OMITTED);
                        catalog
                            .drop_schema(&schema_target, DropPolicy::Strict)
                            .expect("untimed schema cleanup succeeds");
                    }
                    elapsed
                });
            },
        );
        catalog
            .create_schema(&schema_target, CreatePolicy::Strict)
            .expect("graph fixture schema creation succeeds");
        gql.bench_with_input(
            BenchmarkId::new("create_graph_type_gql", scale),
            &scale,
            |b, _| {
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let started = std::time::Instant::now();
                        let outcome = session
                            .execute("CREATE GRAPH TYPE /gql_target/shape { NODE TYPE Person () }")
                            .expect("timed GQL graph-type create succeeds");
                        elapsed += started.elapsed();
                        assert_eq!(black_box(outcome), OMITTED);
                        catalog
                            .drop_graph_type(&graph_type_target, DropPolicy::Strict)
                            .expect("untimed graph-type cleanup succeeds");
                    }
                    elapsed
                });
            },
        );
        session
            .execute("CREATE GRAPH TYPE /gql_target/shape { NODE TYPE Person () }")
            .expect("bound-graph fixture type creation succeeds");
        gql.bench_with_input(
            BenchmarkId::new("create_graph_gql", scale),
            &scale,
            |b, _| {
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let started = std::time::Instant::now();
                        let outcome = session
                            .execute("CREATE GRAPH /gql_target/g ANY")
                            .expect("timed GQL graph create succeeds");
                        elapsed += started.elapsed();
                        assert_eq!(black_box(outcome), OMITTED);
                        catalog
                            .drop_graph(&graph_target, DropPolicy::Strict)
                            .expect("untimed graph cleanup succeeds");
                    }
                    elapsed
                });
            },
        );
        gql.bench_with_input(
            BenchmarkId::new("create_bound_graph_gql", scale),
            &scale,
            |b, _| {
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let started = std::time::Instant::now();
                        let outcome = session
                            .execute("CREATE GRAPH /gql_target/typed_g TYPED /gql_target/shape")
                            .expect("timed GQL bound graph create succeeds");
                        elapsed += started.elapsed();
                        assert_eq!(black_box(outcome), OMITTED);
                        catalog
                            .drop_graph(&bound_graph_target, DropPolicy::Strict)
                            .expect("untimed bound-graph cleanup succeeds");
                    }
                    elapsed
                });
            },
        );
        catalog
            .create_graph(&graph_target, None, CreatePolicy::Strict)
            .expect("drop fixture graph creation succeeds");
        gql.bench_with_input(BenchmarkId::new("drop_graph_gql", scale), &scale, |b, _| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let started = std::time::Instant::now();
                    let outcome = session
                        .execute("DROP GRAPH /gql_target/g")
                        .expect("timed GQL graph drop succeeds");
                    elapsed += started.elapsed();
                    assert_eq!(black_box(outcome), OMITTED);
                    catalog
                        .create_graph(&graph_target, None, CreatePolicy::Strict)
                        .expect("untimed graph recreation succeeds");
                }
                elapsed
            });
        });
        // The fixture graph exists after the drop row's untimed recreation;
        // each replacement leaves exactly one graph at the path, so no
        // cleanup is needed between iterations.
        gql.bench_with_input(
            BenchmarkId::new("create_or_replace_graph_gql", scale),
            &scale,
            |b, _| {
                b.iter(|| {
                    assert_eq!(
                        black_box(&session)
                            .execute(black_box("CREATE OR REPLACE GRAPH /gql_target/g ANY"))
                            .expect("timed GQL graph replace succeeds"),
                        OMITTED
                    );
                });
            },
        );
        gql.bench_with_input(
            BenchmarkId::new("create_graph_if_not_exists_noop_gql", scale),
            &scale,
            |b, _| {
                b.iter(|| {
                    assert_eq!(
                        black_box(&session)
                            .execute(black_box("CREATE GRAPH IF NOT EXISTS /gql_target/g ANY"))
                            .expect("conditional no-op succeeds"),
                        OMITTED
                    );
                });
            },
        );
    }
    gql.finish();

    // M03-PR04 Part 1 authority rows. Each timed mutation performs one facade
    // reservation, scratch-graph staging, unpublished graph preparation, CORE
    // replacement construction, and one outer DatabaseState publication.
    let authority_database = graph_fixture(1);
    let authority_catalog = authority_database.catalog();
    let authority_session = authority_database
        .session(&graph("graphs", "graph_00000"))
        .expect("authority benchmark session resolves");
    let authority_schema = schema("authority_publication");
    let mut authority = c.benchmark_group("catalog_lifecycle/transaction_authority");
    authority.throughput(Throughput::Elements(1));
    authority.bench_function("direct_schema_reserve_publish", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let started = std::time::Instant::now();
                black_box(
                    authority_catalog
                        .create_schema(&authority_schema, CreatePolicy::Strict)
                        .expect("timed authority schema create succeeds"),
                );
                elapsed += started.elapsed();
                authority_catalog
                    .drop_schema(&authority_schema, DropPolicy::Strict)
                    .expect("untimed authority schema cleanup succeeds");
            }
            elapsed
        });
    });
    authority.bench_function("empty_start_commit", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let started = std::time::Instant::now();
                authority_session
                    .start_transaction(TransactionAccessMode::ReadWrite)
                    .expect("timed empty transaction start succeeds");
                authority_session
                    .commit_transaction()
                    .expect("timed empty transaction commit succeeds");
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    authority.bench_function("empty_start_rollback", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let started = std::time::Instant::now();
                authority_session
                    .start_transaction(TransactionAccessMode::ReadWrite)
                    .expect("timed empty transaction start succeeds");
                authority_session
                    .rollback_transaction()
                    .expect("timed empty transaction rollback succeeds");
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    authority.bench_function("read_snapshot_start_rollback", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let started = std::time::Instant::now();
                authority_session
                    .start_transaction(TransactionAccessMode::ReadOnly)
                    .expect("timed read snapshot acquisition succeeds");
                authority_session
                    .rollback_transaction()
                    .expect("timed read snapshot rollback succeeds");
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    authority.bench_function("selected_insert_stage_publish", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let started = std::time::Instant::now();
                black_box(&authority_session)
                    .execute(black_box("INSERT (:AuthorityBench) FINISH"))
                    .expect("timed selected insert succeeds");
                elapsed += started.elapsed();
                authority_session
                    .execute("MATCH (n:AuthorityBench) DELETE n FINISH")
                    .expect("untimed selected insert cleanup succeeds");
            }
            elapsed
        });
    });
    authority.bench_function("explicit_four_write_stage_commit", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let started = std::time::Instant::now();
                authority_session
                    .start_transaction(TransactionAccessMode::ReadWrite)
                    .expect("timed explicit transaction start succeeds");
                for _ in 0..4 {
                    authority_session
                        .execute("INSERT (:ExplicitAuthorityBench) FINISH")
                        .expect("timed explicit staging succeeds");
                }
                authority_session
                    .commit_transaction()
                    .expect("timed explicit commit succeeds");
                elapsed += started.elapsed();
                authority_session
                    .execute("MATCH (n:ExplicitAuthorityBench) DELETE n FINISH")
                    .expect("untimed explicit transaction cleanup succeeds");
            }
            elapsed
        });
    });
    authority.bench_function("selected_publish_then_read", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let started = std::time::Instant::now();
                authority_session
                    .execute("INSERT (:AuthorityVisible) FINISH")
                    .expect("timed visibility insert succeeds");
                let rows = authority_session
                    .execute("MATCH (n:AuthorityVisible) RETURN n")
                    .expect("timed visibility read succeeds");
                assert_eq!(black_box(rows.row_count()), Some(1));
                elapsed += started.elapsed();
                authority_session
                    .execute("MATCH (n:AuthorityVisible) DELETE n FINISH")
                    .expect("untimed visibility cleanup succeeds");
            }
            elapsed
        });
    });
    authority.finish();

    let mut selection = c.benchmark_group("catalog_lifecycle/session_creation");
    for &scale in graph_scales() {
        let database = graph_fixture(scale);
        let target = graph("graphs", format!("graph_{:05}", scale / 2));
        selection.bench_with_input(
            BenchmarkId::new("default_context", scale),
            &scale,
            |b, _| {
                b.iter(|| {
                    black_box(
                        black_box(&database)
                            .session(black_box(&target))
                            .expect("benchmark graph exists"),
                    )
                });
            },
        );
        let options = SessionOptions::new()
            .with_authorization_id(
                AuthorizationId::new("benchmark-authorization")
                    .expect("benchmark authorization ID is valid"),
            )
            .with_principal_provider(Arc::new(BenchmarkPrincipalProvider {
                principal: Principal::new(
                    PrincipalId::new("benchmark-principal")
                        .expect("benchmark principal ID is valid"),
                ),
            }))
            .with_authorization_policy(Arc::new(AllowAllAuthorizationPolicy));
        selection.bench_with_input(
            BenchmarkId::new("authenticated_allow_context", scale),
            &scale,
            |b, _| {
                b.iter(|| {
                    black_box(
                        black_box(&database)
                            .session_with_options(black_box(&target), black_box(options.clone()))
                            .expect("authenticated benchmark session is allowed"),
                    )
                });
            },
        );
    }
    selection.finish();

    let database = graph_fixture(1);
    let session = database
        .session(&graph("graphs", "graph_00000"))
        .expect("minimal request fixture resolves");
    c.bench_function("catalog_lifecycle/request/minimal_execute", |b| {
        b.iter(|| {
            assert_eq!(
                black_box(&session)
                    .execute(black_box("RETURN 1"))
                    .expect("minimal request succeeds")
                    .row_count(),
                Some(1)
            );
        });
    });

    let mut request_setup = c.benchmark_group("catalog_lifecycle/request_setup");
    for parameter_count in [0_usize, 10, 100, 1_000] {
        let mut parameters = RequestParams::new();
        for index in 0..parameter_count {
            parameters
                .insert(
                    &format!("parameter_{index:04}"),
                    GeneralParameter::new(Type::INT64, Value::Int(index as i64))
                        .expect("benchmark parameter type is valid"),
                )
                .expect("benchmark parameter name is unique and valid");
        }
        request_setup.bench_with_input(
            BenchmarkId::from_parameter(parameter_count),
            &parameters,
            |b, parameters| {
                b.iter(|| {
                    let outcome = black_box(&session).execute_request(Request::with_params(
                        black_box("RETURN 1"),
                        black_box(parameters.clone()),
                    ));
                    assert_eq!(
                        black_box(outcome.execution()).and_then(ExecutionOutcome::row_count),
                        Some(1)
                    );
                });
            },
        );
    }
    request_setup.finish();

    let controls_database = graph_fixture(2);
    let controls = controls_database
        .session(&graph("graphs", "graph_00000"))
        .expect("session-control fixture resolves");
    controls.execute("RETURN 1").expect("warm cache succeeds");
    let mut control = c.benchmark_group("catalog_lifecycle/session_control");
    control.bench_function("prepared_cache_hit", |b| {
        b.iter(|| {
            assert_eq!(
                black_box(&controls)
                    .execute(black_box("RETURN 1"))
                    .expect("cache-hit request succeeds")
                    .row_count(),
                Some(1)
            );
        });
    });
    control.bench_function("characteristic_miss_reprepare", |b| {
        b.iter_batched(
            || {
                controls
                    .execute("SESSION SET TIME ZONE '+00:00'")
                    .expect("untimed characteristic change succeeds");
            },
            |()| {
                assert_eq!(
                    controls
                        .execute(black_box("RETURN 1"))
                        .expect("dependency miss reparses")
                        .row_count(),
                    Some(1)
                );
            },
            BatchSize::SmallInput,
        );
    });
    control.bench_function("set_reset_graph_resolve", |b| {
        b.iter(|| {
            controls
                .execute(black_box("SESSION SET GRAPH graph_00001"))
                .expect("graph switch succeeds");
            controls
                .execute(black_box("SESSION RESET GRAPH"))
                .expect("graph reset succeeds");
        });
    });
    control.bench_function("repeated_short_query_10", |b| {
        b.iter(|| {
            for _ in 0..10 {
                black_box(&controls)
                    .execute(black_box("RETURN 1"))
                    .expect("repeated request succeeds");
            }
        });
    });
    control.finish();

    let reference_database = graph_fixture(1);
    let reference_path = graph("graphs", "graph_00000");
    let reference_session = reference_database
        .session(&reference_path)
        .expect("reference fixture session resolves");
    reference_session
        .execute("INSERT (:BenchA)-[:BENCH_LINK]->(:BenchB) FINISH")
        .expect("reference fixture elements are created");
    let graph_reference = reference_database
        .graph_reference(&reference_path)
        .expect("graph reference is issued");
    let node_reference = reference_session
        .node_reference(NodeId::new(1))
        .expect("node reference is issued");
    let edge_reference = reference_session
        .edge_reference(EdgeId::new(1))
        .expect("edge reference is issued");
    let mut references = c.benchmark_group("catalog_lifecycle/reference_validation");
    references.throughput(Throughput::Elements(1));
    references.bench_function("graph", |b| {
        b.iter(|| {
            black_box(&reference_database)
                .resolve_graph_reference(black_box(graph_reference))
                .expect("live graph reference validates")
        });
    });
    references.bench_function("node", |b| {
        b.iter(|| {
            black_box(&reference_session)
                .resolve_node_reference(black_box(node_reference))
                .expect("live node reference validates")
        });
    });
    references.bench_function("edge", |b| {
        b.iter(|| {
            black_box(&reference_session)
                .resolve_edge_reference(black_box(edge_reference))
                .expect("live edge reference validates")
        });
    });
    references.finish();
}

criterion_group! {
    name = catalog_lifecycle;
    config = criterion_config();
    targets = bench_catalog_lifecycle, declarations::bench, runtime_bindings::bench, constraints::bench
}
criterion_main!(catalog_lifecycle);
