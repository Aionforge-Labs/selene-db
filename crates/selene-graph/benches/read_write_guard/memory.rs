//! Fresh-process RSS at construction, held clones and copy-on-write mutation.

use selene_core::{LabelDiff, LabelSet, PropertyDiff, PropertyMap, Value, db_string};
use selene_graph::SharedGraph;
use std::{hint::black_box, io::Write, process::Command};

fn rss() -> u64 {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(output.status.success());
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap()
        * 1024
}

pub fn child(scale: usize) {
    let before = rss();
    let graph = super::fixture::sparse(scale.max(super::fixture::LABELS * 2));
    let built = rss();
    let clones: Vec<_> = (0..16).map(|_| graph.clone()).collect();
    let cloned = rss();
    let shared = SharedGraph::from_graph(graph.clone());
    let id = shared
        .read()
        .live_node_candidates()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let mut held = Vec::new();
    for index in 0..16 {
        held.push(shared.read());
        let mut tx = shared.begin_write();
        {
            let mut m = tx.mutator();
            m.update_node(
                id,
                LabelDiff::new([], []).unwrap(),
                PropertyDiff::new([(db_string("cow").unwrap(), Value::Int(index))], []).unwrap(),
            )
            .unwrap();
            let created = m
                .create_node(
                    LabelSet::single(super::fixture::label(1)),
                    PropertyMap::new(),
                )
                .unwrap();
            m.delete_node(created).unwrap();
        }
        tx.commit().unwrap();
    }
    writeln!(
        std::io::stdout().lock(),
        "lookup_memory scale={scale} empty={before} built={built} clones16={cloned} versions16={}",
        rss()
    )
    .unwrap();
    black_box((graph, clones, shared, held));
}

pub fn measure(scale: usize) {
    for _ in 0..3 {
        let output = Command::new(std::env::current_exe().unwrap())
            .env("SELENE_LOOKUP_MEMORY_CHILD", scale.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::io::stdout().write_all(&output.stdout).unwrap();
    }
}
