//! v1.2 multi-writer BRIEF 1 — single-committer + `WriteTxn::seal` integration
//! tests.
//!
//! These exercise the seal-and-handover commit path through the public API: the
//! committer is the sole snapshot publisher, commits route through it and block
//! until durable + visible, GG02 still aborts pre-handoff on the session thread,
//! ids stay monotonic in commit order, compaction serializes with commits, and a
//! committer that panics fails every waiter cleanly rather than hanging.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use selene_core::{Change, DbString, GraphId, LabelSet, PropertyMap, PropertyValueType, Value};
use selene_graph::{
    GraphError, GraphTypeDef, NodeTypeDef, PropertyTypeDef, SharedGraph, ValidationMode,
};

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn prop(name: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(name), value)]).expect("valid property map")
}

/// A closed-graph type requiring a non-null `name` on `Person` nodes, so an
/// empty-property `Person` insert is a GG02 violation.
fn person_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("committer.person.graph"),
        node_types: vec![NodeTypeDef {
            name: db_string("committer.person"),
            key_labels: LabelSet::single(db_string("Person")),
            properties: vec![PropertyTypeDef {
                name: db_string("name"),
                value_type: PropertyValueType::String,
                list_element_type: None,
                required: true,
                default: None,
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    }
}

#[test]
fn commit_routes_through_committer_and_is_visible_on_return() {
    let shared = SharedGraph::new(GraphId::new(1_001));
    let mut txn = shared.begin_write();
    let node_id = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .expect("create_node ok");
    let outcome = txn.commit().expect("commit ok");

    // commit() returns ⇒ durable + visible: the published snapshot already has
    // the node, with no extra synchronization on the caller's part.
    assert_eq!(outcome.generation, 1);
    assert!(shared.read().is_node_alive(node_id));
}

#[test]
fn handover_preserves_change_list_and_ids() {
    let shared = SharedGraph::new(GraphId::new(1_002));
    let mut txn = shared.begin_write();
    let a = txn
        .mutator()
        .create_node(LabelSet::single(db_string("A")), PropertyMap::new())
        .unwrap();
    let b = txn
        .mutator()
        .create_node(LabelSet::single(db_string("B")), PropertyMap::new())
        .unwrap();
    let outcome = txn.commit().unwrap();

    // The change list survives the seal → committer handoff intact.
    assert_eq!(outcome.changes.len(), 2);
    assert!(matches!(outcome.changes[0], Change::NodeCreated { id, .. } if id == a));
    assert!(matches!(outcome.changes[1], Change::NodeCreated { id, .. } if id == b));
    // next ids are reported from the seal-time peek.
    assert_eq!(outcome.next_node_id, b.get() + 1);
}

#[test]
fn id_monotonicity_across_sequential_commits() {
    let shared = SharedGraph::new(GraphId::new(1_003));
    let mut ids = Vec::new();
    for _ in 0..10 {
        let mut txn = shared.begin_write();
        let id = txn
            .mutator()
            .create_node(LabelSet::new(), PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        ids.push(id);
    }
    // Ids are monotonic in commit order (== seal order == publish order).
    for pair in ids.windows(2) {
        assert!(pair[1].get() > pair[0].get(), "ids strictly increase");
    }
}

#[test]
fn gg02_violation_aborts_in_seal_and_rolls_back_generation_bump() {
    let shared = SharedGraph::builder(GraphId::new(1_004))
        .bound_to(person_graph_type())
        .expect("bind closed graph")
        .build()
        .expect("build closed graph");

    let gen_before = shared.read().meta.generation;

    let mut txn = shared.begin_write();
    // A Person with no `name` violates the closed-graph type.
    txn.mutator()
        .create_node(LabelSet::single(db_string("Person")), PropertyMap::new())
        .expect("mutation stages (validation is at commit)");
    let err = txn.commit().expect_err("GG02 must abort the commit");
    assert!(
        matches!(err, GraphError::TypeViolation(_)),
        "expected closed-graph violation, got {err:?}"
    );

    // The seal-time generation bump is rolled back by Drop (no published epoch
    // advance), exactly as pre-v1.2: the abort happens on the session thread
    // before any handoff to the committer.
    assert_eq!(shared.read().meta.generation, gen_before);
    assert_eq!(shared.read().node_count(), 0);
}

#[test]
fn gg02_aborts_under_contention_never_advance_published_generation() {
    // P2: GG02 abort rollback under contention. Half the threads submit valid
    // commits, half submit guaranteed-GG02-violating commits, in a synchronized
    // storm on a closed graph. Each violating commit must abort on its own
    // session thread (in seal(), under the lock) and roll back its generation
    // bump via Drop, so the published generation only ever advances per VALID
    // commit. Final published node_count and generation must equal exactly the
    // number of valid commits — no aborted txn ever advanced the published
    // state, even with concurrent valid writers racing the same lock.
    let shared = Arc::new(
        SharedGraph::builder(GraphId::new(1_015))
            .bound_to(person_graph_type())
            .unwrap()
            .build()
            .unwrap(),
    );
    let writer_threads: usize = 6;
    let per: usize = 30;
    let valid_threads = writer_threads / 2;
    let barrier = Arc::new(Barrier::new(writer_threads));
    let mut handles = Vec::new();
    for t in 0..writer_threads {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        let valid = t % 2 == 0;
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..per {
                let mut txn = shared.begin_write();
                if valid {
                    txn.mutator()
                        .create_node(
                            LabelSet::single(db_string("Person")),
                            prop("name", Value::String(db_string("ada"))),
                        )
                        .unwrap();
                    txn.commit().expect("valid Person commit succeeds");
                } else {
                    // A Person with no `name` violates the closed-graph type.
                    txn.mutator()
                        .create_node(LabelSet::single(db_string("Person")), PropertyMap::new())
                        .unwrap();
                    let err = txn.commit().expect_err("GG02 violation aborts");
                    assert!(
                        matches!(err, GraphError::TypeViolation(_)),
                        "expected closed-graph violation, got {err:?}"
                    );
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("no writer thread panicked");
    }

    let expected_valid = (valid_threads * per) as u64;
    let snap = shared.read();
    assert_eq!(
        snap.node_count() as u64,
        expected_valid,
        "only valid commits are visible",
    );
    assert_eq!(
        snap.meta.generation, expected_valid,
        "the published generation advanced once per valid commit; no aborted \
         GG02 txn ever advanced it",
    );
    snap.assert_indexes_consistent()
        .expect("final snapshot is structurally consistent");
}

#[test]
fn abort_isolation_does_not_leak_into_next_commit() {
    let shared = SharedGraph::builder(GraphId::new(1_005))
        .bound_to(person_graph_type())
        .unwrap()
        .build()
        .unwrap();

    // Aborted commit (GG02).
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node(LabelSet::single(db_string("Person")), PropertyMap::new())
            .unwrap();
        assert!(txn.commit().is_err());
    }
    // A subsequent valid commit succeeds and publishes generation 1 (the
    // aborted one never advanced the published generation).
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(db_string("Person")),
                prop("name", Value::String(db_string("ada"))),
            )
            .unwrap();
        let outcome = txn.commit().unwrap();
        assert_eq!(outcome.generation, 1);
    }
    assert_eq!(shared.read().node_count(), 1);
}

#[test]
fn concurrent_memory_commits_publish_gapfree_generations() {
    let graph_id = GraphId::new(1_009);
    let threads: usize = 8;
    let per: usize = 25;
    let total = (threads * per) as u64;

    let seqs = Arc::new(std::sync::Mutex::new(Vec::<u64>::with_capacity(
        total as usize,
    )));
    let snapshot;
    {
        let shared = Arc::new(SharedGraph::new(graph_id));
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let shared = Arc::clone(&shared);
            let barrier = Arc::clone(&barrier);
            let seqs = Arc::clone(&seqs);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..per {
                    let mut txn = shared.begin_write();
                    txn.mutator()
                        .create_node(LabelSet::single(db_string("D")), PropertyMap::new())
                        .unwrap();
                    let outcome = txn.commit().unwrap();
                    let seq = outcome.generation;
                    seqs.lock().unwrap().push(seq);
                }
            }));
        }
        for h in handles {
            h.join().expect("no writer panicked");
        }
        assert_eq!(shared.read().node_count() as u64, total);
        snapshot = shared.read();
    }

    // Generations are assigned in seal order, without loss or duplicate replies.
    let mut sorted = seqs.lock().unwrap().clone();
    sorted.sort_unstable();
    let expected: Vec<u64> = (1..=total).collect();
    assert_eq!(
        sorted, expected,
        "published generations are gap-free 1..=total"
    );

    assert_eq!(snapshot.node_count() as u64, total);
}

#[test]
fn compact_serializes_with_concurrent_commits() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(1_007)));

    // Seed: create then delete a batch of nodes to leave reclaimable holes.
    let mut survivors = Vec::new();
    {
        let mut txn = shared.begin_write();
        let mut ids = Vec::new();
        for _ in 0..50 {
            ids.push(
                txn.mutator()
                    .create_node(LabelSet::single(db_string("T")), PropertyMap::new())
                    .unwrap(),
            );
        }
        txn.commit().unwrap();
        let mut txn = shared.begin_write();
        for (idx, id) in ids.iter().enumerate() {
            if idx % 2 == 0 {
                txn.mutator().delete_node(*id).unwrap();
            } else {
                survivors.push(*id);
            }
        }
        txn.commit().unwrap();
    }

    // Hammer commits from N threads while one thread compacts repeatedly. The
    // committer serializes both, so neither corrupts the other.
    let threads: usize = 8;
    let barrier = Arc::new(Barrier::new(threads + 1));
    let mut handles = Vec::new();
    for t in 0..threads {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..25 {
                let mut txn = shared.begin_write();
                txn.mutator()
                    .create_node(
                        LabelSet::single(db_string("W")),
                        prop("t", Value::Int(t as i64)),
                    )
                    .unwrap();
                txn.commit().unwrap();
            }
        }));
    }
    let compactor = {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            for _ in 0..10 {
                shared.compact().expect("compaction serializes cleanly");
            }
        })
    };
    for h in handles {
        h.join().expect("writer thread did not panic");
    }
    compactor.join().expect("compactor did not panic");

    // 8 threads * 25 new nodes + all survivors are alive and consistent.
    assert_eq!(shared.read().node_count(), threads * 25 + survivors.len());
    for id in &survivors {
        assert!(shared.read().is_node_alive(*id));
    }
}

#[test]
fn index_ddl_serializes_with_commits_through_committer() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(1_008)));
    let label = db_string("Doc");
    let key = db_string("rank");

    // Seed an indexed-compatible node so the DDL build passes.
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node(LabelSet::single(label.clone()), prop("rank", Value::Int(1)))
            .unwrap();
        txn.commit().unwrap();
    }

    let threads: usize = 4;
    let barrier = Arc::new(Barrier::new(threads + 1));
    let mut handles = Vec::new();
    for _ in 0..threads {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        let value = label.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for n in 0..20 {
                let mut txn = shared.begin_write();
                txn.mutator()
                    .create_node(LabelSet::single(value.clone()), prop("rank", Value::Int(n)))
                    .unwrap();
                txn.commit().unwrap();
            }
        }));
    }
    let ddl = {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        let label = label.clone();
        let key = key.clone();
        thread::spawn(move || {
            barrier.wait();
            shared
                .create_property_index(label, key, selene_graph::TypedIndexKind::I64)
                .expect("index DDL commits through the funnel");
        })
    };
    for h in handles {
        h.join().expect("writer thread did not panic");
    }
    ddl.join().expect("ddl thread did not panic");

    assert_eq!(shared.read().node_count(), threads * 20 + 1);
    assert!(
        shared.read().property_index.contains_key(&(label, key)),
        "the index DDL published through the committer",
    );
}

#[test]
fn string_admission_race_class_concurrency_is_consistent() {
    // The "would this catch the DbString admission race" bar: many threads commit
    // overlapping label/property writes concurrently; the single committer
    // serializes every publish so the final snapshot is internally consistent
    // (label index ↔ columns ↔ adjacency), validated by the debug-only
    // assert_indexes_consistent that runs on every committer publish.
    let shared = Arc::new(SharedGraph::new(GraphId::new(1_011)));
    let threads: usize = 16;
    let per = 40;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for t in 0..threads {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for n in 0..per {
                let mut txn = shared.begin_write();
                txn.mutator()
                    .create_node(
                        LabelSet::single(db_string("Shared")),
                        prop("k", Value::Int((t * per + n) as i64)),
                    )
                    .unwrap();
                txn.commit().unwrap();
            }
        }));
    }
    for h in handles {
        h.join().expect("no writer thread panicked");
    }
    assert_eq!(shared.read().node_count(), threads * per);
    // The whole-graph structural net (also run per-commit in debug builds).
    shared
        .read()
        .assert_indexes_consistent()
        .expect("final snapshot is structurally consistent");
}

#[test]
fn explicit_txn_releases_lock_before_recv_block_no_deadlock() {
    // Deadlock invariant: an explicit transaction releases the write lock at
    // seal() BEFORE it enqueues + recv-blocks on the committer. Since the P0/P1
    // fix, compaction also builds its dense graph on the CALLER thread under the
    // lock (seal-and-handover) and hands the committer a pre-built snapshot, so
    // the committer never takes the write lock for ANY work item — the
    // send-under-lock/queued-compact deadlock surface is structurally gone. This
    // test pins the liveness: a long explicit-txn commit storm racing a compact
    // storm (each compactor briefly holds the lock to build) completes within a
    // bounded deadline with no wedge.
    let shared = Arc::new(SharedGraph::new(GraphId::new(1_013)));
    {
        let mut txn = shared.begin_write();
        for _ in 0..10 {
            txn.mutator()
                .create_node(LabelSet::single(db_string("X")), PropertyMap::new())
                .unwrap();
        }
        txn.commit().unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    let writer = {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            for _ in 0..20 {
                let mut txn = shared.begin_write();
                txn.mutator()
                    .create_node(LabelSet::new(), PropertyMap::new())
                    .unwrap();
                txn.commit().unwrap();
            }
        })
    };
    let compactor = {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            for _ in 0..20 {
                shared.compact().unwrap();
            }
        })
    };
    writer.join().expect("writer finished");
    compactor.join().expect("compactor finished");
    assert!(
        Instant::now() < deadline,
        "no deadlock between explicit-txn commits and compaction",
    );
}

#[test]
fn back_pressure_holds_under_fan_in_burst() {
    // Many threads each fire a burst of commits; the bounded inbound channel
    // applies back-pressure (a full channel blocks the sender) without losing
    // or reordering any commit. Correctness check: every commit lands.
    let shared = Arc::new(SharedGraph::new(GraphId::new(1_014)));
    let threads: usize = 12;
    let per = 100;
    let count = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..threads {
        let shared = Arc::clone(&shared);
        let count = Arc::clone(&count);
        handles.push(thread::spawn(move || {
            for _ in 0..per {
                let mut txn = shared.begin_write();
                txn.mutator()
                    .create_node(LabelSet::new(), PropertyMap::new())
                    .unwrap();
                txn.commit().unwrap();
                count.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.join().expect("no thread panicked under back-pressure");
    }
    assert_eq!(count.load(Ordering::Relaxed) as usize, threads * per);
    assert_eq!(shared.read().node_count(), threads * per);
}

#[test]
fn dropping_shared_graph_terminates_the_committer_promptly() {
    // P2 liveness pin: dropping a SharedGraph joins the committer thread
    // unconditionally. That relies on every WriteTxn-held submit handle being
    // gone first (they borrow &SharedGraph, so they are). Pin that the join
    // returns within a bounded deadline — a future change that let a submit
    // handle escape to a 'static owner would wedge this drop in join() forever.
    let shared = SharedGraph::new(GraphId::new(1_016));
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node(LabelSet::new(), PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
    }

    // Drop on a worker thread and require it to finish (committer joined) within
    // a deadline. The watchdog thread asserts the drop did not hang.
    let done = Arc::new(AtomicU64::new(0));
    let done_writer = Arc::clone(&done);
    let dropper = thread::spawn(move || {
        drop(shared);
        done_writer.store(1, Ordering::Release);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while done.load(Ordering::Acquire) == 0 {
        assert!(
            Instant::now() < deadline,
            "dropping SharedGraph did not join the committer within the deadline",
        );
        thread::yield_now();
    }
    dropper.join().expect("dropper thread did not panic");
}
