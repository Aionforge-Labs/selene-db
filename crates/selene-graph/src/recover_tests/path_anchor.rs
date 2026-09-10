//! Recovery anchoring regressions for parent-directory aliases.

use std::io::Write;
use std::os::unix::fs::symlink;

use selene_persist::{
    AUDIT_KIND_RESERVED_0, AuditLog, AuditRecord, DEFAULT_AUDIT_FILE_NAME, DEFAULT_WAL_FILE_NAME,
    MANIFEST_FILE_NAME, WalReader,
};

use super::*;

#[test]
fn recovery_alias_retarget_cannot_redirect_the_reopened_writer() {
    let root = temp_dir("recovery-alias-retarget");
    let first_dir = root.join("first");
    let second_dir = root.join("second");
    let alias = root.join("live");
    fs::create_dir(&first_dir).unwrap();
    fs::create_dir(&second_dir).unwrap();
    symlink(&first_dir, &alias).unwrap();
    append_wal(&first_dir, 0, &[node_created(49)]);
    let first_audit = first_dir.join(DEFAULT_AUDIT_FILE_NAME);
    let second_audit = second_dir.join(DEFAULT_AUDIT_FILE_NAME);
    for path in [&first_audit, &second_audit] {
        let mut audit = AuditLog::open(path).unwrap();
        audit
            .append(&AuditRecord {
                recorded_at_unix_nanos: 1,
                kind: AUDIT_KIND_RESERVED_0,
                payload: vec![1, 2, 3],
            })
            .unwrap();
        drop(audit);
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(&[0xAA, 0xBB, 0xCC])
            .unwrap();
    }
    let first_audit_torn_len = fs::metadata(&first_audit).unwrap().len();
    let second_audit_torn_len = fs::metadata(&second_audit).unwrap().len();

    let hook_alias = alias.clone();
    let hook_second = second_dir.clone();
    super::super::set_after_persist_recovery_hook(move || {
        fs::remove_file(&hook_alias).unwrap();
        symlink(&hook_second, &hook_alias).unwrap();
    });
    let recovered = SharedGraph::recover(&alias, GraphId::new(7)).unwrap();
    assert_eq!(
        alias.canonicalize().unwrap(),
        second_dir.canonicalize().unwrap()
    );
    assert_eq!(
        fs::metadata(&first_audit).unwrap().len(),
        first_audit_torn_len - 3
    );
    assert_eq!(
        fs::metadata(&second_audit).unwrap().len(),
        second_audit_torn_len
    );
    assert!(recovered.read().is_node_alive(NodeId::new(49)));
    let mut txn = recovered.begin_write();
    let created = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    assert_eq!(created, NodeId::new(50));
    txn.commit().unwrap();
    drop(recovered);

    let sequences: Vec<_> = WalReader::open(&first_dir.join(DEFAULT_WAL_FILE_NAME))
        .unwrap()
        .iterate(|_| true)
        .unwrap()
        .map(|entry| entry.unwrap().header.sequence)
        .collect();
    assert_eq!(sequences, vec![1, 2]);
    assert!(!second_dir.join(DEFAULT_WAL_FILE_NAME).exists());
    assert!(!second_dir.join(MANIFEST_FILE_NAME).exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retained_capability_drives_graph_builder_checkpoint_prune_audit_and_recovery() {
    use selene_persist::{PersistenceReadGuard, RetentionPolicy, StoreDirectory};
    let fixture = selene_testing::PersistenceTestPath::new();
    let original = fixture.parent().unwrap().join("store");
    let retained = original.with_file_name("retained");
    fs::create_dir(&original).unwrap();
    let cap = StoreDirectory::from_file(fs::File::open(&original).unwrap(), &original).unwrap();
    let builder = SharedGraph::builder(GraphId::new(7))
        .with_wal_in(&cap, Path::new(DEFAULT_WAL_FILE_NAME), WalConfig::default())
        .unwrap();
    fs::rename(&original, &retained).unwrap();
    fs::create_dir(&original).unwrap();
    let graph = builder
        .with_audit_log(original.join(DEFAULT_AUDIT_FILE_NAME))
        .unwrap()
        .build()
        .unwrap();
    let mut txn = graph.begin_write();
    let node = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();
    graph
        .checkpoint(crate::CheckpointConfig::default())
        .unwrap();
    drop(graph);
    let guard = PersistenceReadGuard::acquire_in(&cap).unwrap();
    assert_eq!(guard.read_manifest().unwrap().unwrap().live_snapshot_seq, 2);
    drop(guard);
    selene_persist::retention::prune_in(
        &cap,
        &RetentionPolicy {
            keep_n_wal_archives: 0,
            ..RetentionPolicy::default()
        },
    )
    .unwrap();
    let graph = SharedGraph::recover_in(&cap, GraphId::new(7)).unwrap();
    assert!(graph.read().is_node_alive(node));
    graph
        .checkpoint(crate::CheckpointConfig::default())
        .unwrap();
    assert!(
        AuditLog::read_all_in(&cap, Path::new(DEFAULT_AUDIT_FILE_NAME))
            .unwrap()
            .is_empty()
    );
    drop(graph);
    assert_eq!(fs::read_dir(&original).unwrap().count(), 0);
    assert!(retained.join("snapshot.3.snap").is_file());
    assert!(retained.join(DEFAULT_AUDIT_FILE_NAME).is_file());
    let graph = SharedGraph::recover_in(&cap, GraphId::new(7)).unwrap();
    assert_eq!(graph.read().node_count(), 1);
    assert!(graph.read().is_node_alive(node));
}

#[test]
fn real_parent_replacement_during_recovery_cannot_redirect_audit_reattachment() {
    let fixture = selene_testing::PersistenceTestPath::new();
    let original = fixture.parent().unwrap().join("store");
    let retained = original.with_file_name("retained");
    fs::create_dir(&original).unwrap();
    append_wal(&original, 0, &[node_created(49)]);
    drop(AuditLog::open(&original.join(DEFAULT_AUDIT_FILE_NAME)).unwrap());
    let hook_original = original.clone();
    let hook_retained = retained.clone();
    super::super::set_after_persist_recovery_hook(move || {
        fs::rename(&hook_original, &hook_retained).unwrap();
        fs::create_dir(&hook_original).unwrap();
    });
    let graph = SharedGraph::recover(&original, GraphId::new(7)).unwrap();
    assert!(graph.read().is_node_alive(NodeId::new(49)));
    graph
        .checkpoint(crate::CheckpointConfig::default())
        .unwrap();
    drop(graph);
    assert_eq!(fs::read_dir(original).unwrap().count(), 0);
    assert!(retained.join("snapshot.2.snap").is_file());
}
