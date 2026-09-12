//! Independent selection witnesses, including actual path identities, not counts alone.

use super::{
    oracle::Fixture,
    tests::{analyzed, run},
    *,
};
use crate::{
    EmptyProcedureRegistry, ImplDefinedCaps, PathSelector, lower_path_automata_with_defaults,
};
use selene_core::{EdgeDirection, Path, Value};
use std::collections::BTreeMap;

fn paths(result: &PathExecution) -> Vec<&Path> {
    result
        .table
        .rows()
        .iter()
        .map(|row| match &row.values()[0] {
            Value::Path(p) => p.as_ref(),
            value => panic!("expected PATH, got {value:?}"),
        })
        .collect()
}

#[test]
fn endpoint_partitions_have_independent_shortest_lengths_and_declared_paths() {
    let f = Fixture::new(
        5,
        &[
            (0, 1, true, true),
            (0, 2, true, true),
            (2, 3, true, true),
            (3, 4, true, true),
        ],
    );
    let result = run(
        &f.graph,
        "MATCH ALL SHORTEST p = (a:Root)-[r{1,3}]->(b) RETURN p",
        Default::default(),
    )
    .unwrap();
    let actual: BTreeMap<_, _> = paths(&result)
        .into_iter()
        .map(|p| (p.segments.last().unwrap().node, p.segments.len()))
        .collect();
    assert_eq!(actual[&f.nodes[1]], 1);
    assert_eq!(actual[&f.nodes[4]], 3);
    assert_eq!(
        result.table.schema().columns[0].ty,
        crate::AnalyzedType::Resolved(crate::GqlType::Path)
    );
}

#[test]
fn qualifies_full_group_before_selection_including_later_endpoint_references() {
    let f = Fixture::new(
        3,
        &[(0, 2, true, true), (0, 1, true, true), (1, 2, true, true)],
    );
    for source in [
        "MATCH ALL SHORTEST p = (a:Root)-[r{1,2} WHERE size(r) = 2]->(b) RETURN p",
        "MATCH ALL SHORTEST p = (a:Root)-[r{1,2}]->(b WHERE size(r) = 2) RETURN p",
    ] {
        let result = run(&f.graph, source, Default::default()).unwrap();
        assert_eq!(paths(&result).len(), 1, "{source}");
        assert_eq!(paths(&result)[0].segments.len(), 2, "{source}");
        assert_eq!(paths(&result)[0].segments[1].node, f.nodes[2]);
    }
}

#[test]
fn ties_counted_paths_and_counted_groups_differ_without_collapsing_parallel_edges() {
    let f = Fixture::new(
        3,
        &[
            (0, 2, true, true),
            (0, 2, true, true),
            (0, 1, true, true),
            (1, 2, true, true),
        ],
    );
    for (selector, expected) in [
        ("ALL SHORTEST", vec![1, 1, 1]),
        ("SHORTEST 1", vec![1, 1]),
        ("SHORTEST 2", vec![1, 1, 1]),
        ("SHORTEST 2 GROUPS", vec![1, 1, 1, 2]),
        ("ANY 99 PATHS", vec![1, 1, 1, 2]),
        ("SHORTEST 99", vec![1, 1, 1, 2]),
    ] {
        let source = format!("MATCH {selector} p = (a:Root)-[r{{1,2}}]->(b) RETURN p");
        let result = run(&f.graph, &source, Default::default()).unwrap();
        let mut lengths: Vec<_> = paths(&result).iter().map(|p| p.segments.len()).collect();
        lengths.sort();
        assert_eq!(lengths, expected, "{source}");
        let unique: std::collections::HashSet<_> = paths(&result)
            .iter()
            .map(|p| p.segments.iter().map(|s| s.edge).collect::<Vec<_>>())
            .collect();
        assert_eq!(unique.len(), result.table.row_count());
    }
}

#[test]
fn zero_edge_paths_reversed_directed_loops_and_mixed_orientation_are_typed() {
    let f = Fixture::new(
        2,
        &[(0, 1, true, true), (0, 1, false, true), (0, 0, true, true)],
    );
    for (source, direction) in [
        (
            "MATCH ALL p = (a)<-[r{1}]-(b) RETURN p",
            EdgeDirection::Incoming,
        ),
        (
            "MATCH ALL p = (a)~[r{1}]~(b) RETURN p",
            EdgeDirection::Undirected,
        ),
    ] {
        let result = run(&f.graph, source, Default::default()).unwrap();
        assert!(
            paths(&result)
                .iter()
                .all(|p| p.segments[0].direction == direction)
        );
    }
    let result = run(
        &f.graph,
        "MATCH ALL SHORTEST p = (a)-[r{0,1}]->(b) RETURN p",
        Default::default(),
    )
    .unwrap();
    assert!(
        paths(&result)
            .iter()
            .any(|p| p.start == f.nodes[0] && p.segments.is_empty())
    );
    assert!(
        paths(&result)
            .iter()
            .any(|p| p.start == f.nodes[1] && p.segments.is_empty())
    );
}

#[test]
fn open_selective_search_finishes_by_history_or_exhaustion_never_silent_hop_cutoff() {
    let f = Fixture::new(
        3,
        &[(0, 1, true, true), (1, 2, true, true), (2, 0, true, true)],
    );
    let result = run(
        &f.graph,
        "MATCH ALL SHORTEST p = (a:Root)-[r+]->(b) RETURN p",
        Default::default(),
    )
    .unwrap();
    let mut lengths: Vec<_> = paths(&result).iter().map(|p| p.segments.len()).collect();
    lengths.sort();
    assert_eq!(lengths, [1, 2, 3]);
    assert!(matches!(
        run(
            &f.graph,
            "MATCH ALL SHORTEST WALK p = (a:Root)-[r+]->(b) RETURN p",
            PathExecutionLimits {
                max_hops: 2,
                ..Default::default()
            }
        ),
        Err(ExecutorError::ProgramLimitExceeded {
            detail: "max_path_hops",
            ..
        })
    ));
    let dag = Fixture::new(3, &[(0, 1, true, true), (1, 2, true, true)]);
    let result = run(
        &dag.graph,
        "MATCH ALL SHORTEST WALK p = (a:Root)-[r+ WHERE size(r) > 1]->(b) RETURN p",
        Default::default(),
    )
    .unwrap();
    assert_eq!(paths(&result)[0].segments.len(), 2);
    let result = run(
        &f.graph,
        "MATCH ALL SHORTEST p = (a:Root)-[r+]->(b:Absent) RETURN p",
        Default::default(),
    )
    .unwrap();
    assert!(paths(&result).is_empty());
}

#[test]
fn zero_native_counts_return_empty_but_written_zero_keeps_22g0f() {
    let f = Fixture::new(1, &[(0, 0, true, true)]);
    for prefix in ["ANY 0", "SHORTEST 0", "SHORTEST 0 GROUPS"] {
        assert_eq!(
            crate::parse(&format!("MATCH {prefix} (a) RETURN a"))
                .unwrap_err()
                .gqlstatus()
                .as_str(),
            "22G0F"
        );
    }
    let a = analyzed("MATCH ANY p = (a)-[r{0,2}]->(b) RETURN p");
    for selector in [
        PathSelector::Any { paths: 0 },
        PathSelector::CountedShortest { paths: 0 },
        PathSelector::CountedShortestGroup { groups: 0 },
    ] {
        let mut set = lower_path_automata_with_defaults(&a).unwrap();
        set.automata[0].selector.selector = Some(selector);
        let program = BoundedPathProgram::compile(&set.automata, &a).unwrap();
        let caps = ImplDefinedCaps::default();
        let tx = TxContext::read_only(
            f.graph.read(),
            &caps,
            &EmptyProcedureRegistry,
            f.graph.index_providers(),
        );
        assert_eq!(
            program
                .execute(&tx, Default::default())
                .unwrap()
                .table
                .row_count(),
            0
        );
    }
}

#[test]
fn memory_exhaustion_while_collecting_ties_never_publishes_partial_all_shortest() {
    let edges = vec![(0, 1, true, true); 32];
    let f = Fixture::new(2, &edges);
    let source = "MATCH ALL SHORTEST p = (a:Root)-[r]->(b) RETURN p";
    let complete = run(&f.graph, source, Default::default()).unwrap();
    assert_eq!(complete.table.row_count(), 32);
    for max_bytes in [
        0,
        complete.stats.peak_bytes / 2,
        complete.stats.peak_bytes - 1,
    ] {
        assert!(matches!(
            run(
                &f.graph,
                source,
                PathExecutionLimits {
                    max_bytes,
                    ..Default::default()
                }
            ),
            Err(ExecutorError::ProgramLimitExceeded {
                detail: "max_path_bytes",
                ..
            })
        ));
    }
}

#[test]
fn exhaustive_enumerate_qualify_partition_select_small_directed_multigraphs() {
    // Independent finite model: expand edge tuples, qualify lengths, partition,
    // sort and take paths/groups. Does not call native transitions or selection.
    for mask in 0u8..32 {
        let possible = [
            (0, 0, true, true),
            (0, 1, true, true),
            (0, 1, true, true),
            (1, 0, true, true),
            (1, 1, true, true),
        ];
        let edges: Vec<_> = possible
            .into_iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, e)| e)
            .collect();
        let f = Fixture::new(2, &edges);
        for (selector, count, groups) in [
            ("ANY 2 WALK PATHS", 2, false),
            ("ALL SHORTEST", 1, true),
            ("SHORTEST 2", 2, false),
            ("SHORTEST 2 WALK GROUPS", 2, true),
        ] {
            let mut partitions: BTreeMap<_, Vec<(Vec<usize>, Vec<usize>)>> = BTreeMap::new();
            for start in 0..2 {
                let mut level = vec![(vec![start], vec![])];
                for length in 0..=3 {
                    for (nodes, ids) in &level {
                        if length != 1 {
                            partitions
                                .entry((start, *nodes.last().unwrap()))
                                .or_default()
                                .push((nodes.clone(), ids.clone()));
                        }
                    }
                    let mut next = Vec::new();
                    for (nodes, ids) in level {
                        for (i, &(a, b, _, _)) in edges.iter().enumerate() {
                            if nodes.last() == Some(&a) {
                                let mut n = nodes.clone();
                                n.push(b);
                                let mut e = ids.clone();
                                e.push(i);
                                next.push((n, e));
                            }
                        }
                    }
                    level = next;
                }
            }
            let mut expected = Vec::new();
            for entries in partitions.values_mut() {
                entries.sort_by_key(|(n, e)| (e.len(), n.clone(), e.clone()));
                let mut lengths: Vec<_> = entries.iter().map(|(_, e)| e.len()).collect();
                lengths.dedup();
                lengths.truncate(count);
                expected.extend(
                    entries
                        .iter()
                        .enumerate()
                        .filter(|(i, (_, e))| {
                            if groups {
                                lengths.contains(&e.len())
                            } else {
                                *i < count
                            }
                        })
                        .map(|(_, (n, e))| {
                            (
                                f.nodes[n[0]],
                                e.iter().map(|&i| f.edges[i].0).collect::<Vec<_>>(),
                            )
                        }),
                );
            }
            let prefix = if selector.contains("WALK") {
                selector.to_owned()
            } else {
                format!("{selector} WALK")
            };
            let source =
                format!("MATCH {prefix} p = (a)-[r{{0,3}} WHERE size(r) <> 1]->(b) RETURN p");
            let result = run(&f.graph, &source, Default::default()).unwrap();
            let mut actual: Vec<_> = paths(&result)
                .iter()
                .map(|p| {
                    (
                        p.start,
                        p.segments.iter().map(|s| s.edge).collect::<Vec<_>>(),
                    )
                })
                .collect();
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "mask {mask}: {source}");
            for size in [1, 7] {
                let table = super::differentials::statement_table(&f, &source, size);
                let mut actual: Vec<_> = table
                    .rows()
                    .iter()
                    .map(|row| {
                        let Value::Path(path) = &row.values()[0] else {
                            panic!("physical PATH")
                        };
                        (
                            path.start,
                            path.segments.iter().map(|s| s.edge).collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                actual.sort();
                assert_eq!(
                    actual, expected,
                    "physical mask {mask}, batch {size}: {source}"
                );
            }
        }
    }
}
