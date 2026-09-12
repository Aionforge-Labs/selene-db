//! Same registered benchmark binary; absolute selected-path phase/retention costs.

use super::*;

pub(super) fn selected_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("gql_selected_paths");
    for (shape, n) in [
        ("many_ties", 128),
        ("long_path", 128),
        ("rejected_shortest", 32),
    ] {
        let graph = SharedGraph::new(GraphId::new(50503));
        let mut write = graph.begin_write();
        let mut m = write.mutator();
        let nodes: Vec<_> = (0..=n)
            .map(|i| {
                m.create_node(
                    LabelSet::single(
                        db_string(if i == 0 {
                            "Root"
                        } else if i == n {
                            "Target"
                        } else {
                            "N"
                        })
                        .unwrap(),
                    ),
                    PropertyMap::new(),
                )
                .unwrap()
            })
            .collect();
        let mut edge = |a: usize, b: usize| {
            m.create_edge(
                db_string("K").unwrap(),
                nodes[a],
                nodes[b],
                PropertyMap::new(),
            )
            .unwrap();
        };
        let (max, expected, condition) = match shape {
            "many_ties" => {
                for _ in 0..n {
                    edge(0, n);
                }
                (1, n, "")
            }
            "long_path" => {
                for i in 0..n {
                    edge(i, i + 1);
                }
                (n, 1, "")
            }
            "rejected_shortest" => {
                edge(0, n);
                for i in 1..n {
                    edge(0, i);
                    edge(i, n);
                }
                (2, n - 1, " WHERE size(r) > 1")
            }
            _ => unreachable!(),
        };
        write.commit().unwrap();
        let source = format!(
            "MATCH ALL SHORTEST p = (a:Root)-[r{{1,{max}}}{condition}]->(b:Target) RETURN p"
        );
        let analyzed = analyze(parse(&source).unwrap(), &EmptyProcedureRegistry, None).unwrap();
        let set = lower_path_automata_with_defaults(&analyzed).unwrap();
        let program = BoundedPathProgram::compile(&set.automata, &analyzed).unwrap();
        let mut caps = ImplDefinedCaps::default();
        caps.max_quantifier = 128;
        let tx = TxContext::read_only(
            graph.read(),
            &caps,
            &EmptyProcedureRegistry,
            graph.index_providers(),
        );
        let check = program.execute(&tx, Default::default()).unwrap();
        assert_eq!(check.table.row_count(), expected);
        let s = &check.stats;
        eprintln!(
            "selected_paths/{shape}/{n}: rows={} states={} history_clones={} history_peak_est_bytes={} candidates_peak_est_bytes={} total_peak_est_bytes={} discovery_us={} selection_us={} materialization_us={}",
            expected,
            s.product_states,
            s.history_clones,
            s.peak_history_bytes,
            s.peak_candidate_bytes,
            s.peak_bytes,
            s.discovery_time.as_micros(),
            s.selection_time.as_micros(),
            s.materialization_time.as_micros()
        );
        group.bench_function(BenchmarkId::new(shape, n), |b| {
            b.iter(|| black_box(program.execute(black_box(&tx), Default::default()).unwrap()))
        });
    }
    group.finish();
}
