//! Whole-query measurements: filter/join, path selection, projection and results.

use criterion::{BenchmarkId, Criterion, Throughput};
use selene_gql::{EmptyProcedureRegistry, Session, StatementOutput};
use std::{hint::black_box, num::NonZeroUsize, process::Command};

const SOURCE: &str = "MATCH (a:N), (tag:Root) FILTER a <> tag \
    MATCH ALL SHORTEST p = (a)-[r:K{1,3}]->(b) \
    MATCH (b)-[:K]->(c) FILTER size(r) >= 2 RETURN a, r, p, c";

fn query(session: &mut Session<'_>) -> selene_gql::BindingTable {
    let StatementOutput::Rows(table) = session
        .execute_source(SOURCE, &EmptyProcedureRegistry)
        .unwrap()
    else {
        panic!("whole path query rows")
    };
    table
}

fn rss_bytes() -> i64 {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(output.status.success(), "native ps RSS measurement failed");
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse::<i64>()
        .unwrap()
        * 1024
}

/// A fresh-process retained-result RSS witness, not an allocator or peak claim.
pub(super) fn memory_child(n: usize) {
    let (graph, _) = super::fixture("sparse", n);
    let mut session = Session::new(&graph).with_plan_cache(NonZeroUsize::new(4).unwrap());
    let before = rss_bytes();
    let result = query(&mut session);
    assert_eq!(result.row_count(), 2 * n - 9);
    let retained = rss_bytes();
    eprintln!(
        "path_query_memory/n={n}: rows={} before_rss_bytes={before} retained_result_rss_bytes={retained} delta_bytes={}",
        result.row_count(),
        retained - before
    );
    black_box(result);
}

pub(super) fn whole_path_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("gql_path_whole_query");
    for n in [64, 256, 1024] {
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let output = Command::new(std::env::current_exe().unwrap())
                .env("SELENE_PATH_QUERY_MEMORY", n.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "memory child: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        let (graph, _) = super::fixture("sparse", n);
        let mut session = Session::new(&graph).with_plan_cache(NonZeroUsize::new(4).unwrap());
        let expected = 2 * n - 9;
        assert_eq!(query(&mut session).row_count(), expected);
        group.throughput(Throughput::Elements(expected as u64));
        group.bench_with_input(
            BenchmarkId::new("filter_join_selected_path", n),
            &n,
            |b, _| {
                // Warm plan cache, complete execution and result allocation/drop.
                b.iter(|| black_box(query(&mut session)));
            },
        );
    }
    group.finish();
}
