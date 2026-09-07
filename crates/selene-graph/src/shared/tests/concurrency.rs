use super::*;

#[test]
fn read_is_lock_free_concurrent_with_another_reader() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(1)));
    thread::scope(|scope| {
        for _ in 0..8 {
            let shared = Arc::clone(&shared);
            scope.spawn(move || {
                for _ in 0..10_000 {
                    assert_eq!(shared.read().meta.graph_id, GraphId::new(1));
                }
            });
        }
    });
}

#[test]
fn read_during_write_lock_held_does_not_block() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(1)));
    thread::scope(|scope| {
        let writer_graph = Arc::clone(&shared);
        let writer = scope.spawn(move || {
            let _txn = writer_graph.begin_write();
            thread::sleep(Duration::from_millis(75));
        });
        thread::sleep(Duration::from_millis(10));
        let start = Instant::now();
        for _ in 0..4 {
            let reader_graph = Arc::clone(&shared);
            scope.spawn(move || {
                for _ in 0..100 {
                    assert_eq!(reader_graph.read().node_count(), 0);
                }
            });
        }
        writer.join().unwrap();
        assert!(start.elapsed() < Duration::from_millis(500));
    });
}

#[test]
fn pre_commit_snapshot_is_isolated_from_tail_and_alive_cow_mutations() {
    // B1 end-to-end: the tail columns and alive bitmaps are now Arc-shared
    // COW state, so a snapshot taken BEFORE a commit must never observe that
    // commit's creates or deletes — neither in liveness nor in column values.
    use selene_core::{LabelSet, PropertyMap, db_string};

    let shared = SharedGraph::new(GraphId::new(1));
    let label = db_string("Iso").unwrap();
    let (keep, doomed, edge) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let keep = mutator
            .create_node(LabelSet::single(label.clone()), PropertyMap::new())
            .expect("create keep");
        let doomed = mutator
            .create_node(LabelSet::single(label.clone()), PropertyMap::new())
            .expect("create doomed");
        let edge = mutator
            .create_edge(
                db_string("iso.link").unwrap(),
                keep,
                doomed,
                PropertyMap::new(),
            )
            .expect("create edge");
        txn.commit().expect("seed commit");
        (keep, doomed, edge)
    };

    let snapshot = shared.read();
    assert_eq!(snapshot.node_count(), 2);
    assert_eq!(snapshot.edge_count(), 1);

    {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        mutator.delete_edge(edge).expect("delete edge");
        mutator.delete_node(doomed).expect("delete doomed");
        mutator
            .create_node(LabelSet::single(label.clone()), PropertyMap::new())
            .expect("create post-snapshot node");
        txn.commit().expect("mutating commit");
    }

    // The pre-commit snapshot still sees the original graph...
    assert_eq!(snapshot.node_count(), 2);
    assert_eq!(snapshot.edge_count(), 1);
    assert!(snapshot.is_node_alive(keep));
    assert!(snapshot.is_node_alive(doomed));
    assert!(snapshot.node_store.alive.contains(1));
    assert!(snapshot.edge_store.alive.contains(0));
    assert!(snapshot.node_properties(doomed).is_some());

    // ...while a fresh read sees the post-commit state.
    let current = shared.read();
    assert_eq!(current.node_count(), 2); // keep + the new node
    assert_eq!(current.edge_count(), 0);
    assert!(current.is_node_alive(keep));
    assert!(!current.is_node_alive(doomed));
}
