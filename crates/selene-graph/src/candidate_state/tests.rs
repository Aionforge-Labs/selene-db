use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, GraphId, HlcTimestamp, Origin, PropertyMap, db_string};
use selene_persist::{
    DEFAULT_WAL_FILE_NAME, SectionCompression, SnapshotConfig, SyncPolicy, WalConfig, WalWriter,
};

use super::*;
use crate::SharedGraph;

#[path = "required_edges_tests.rs"]
mod required_edges_tests;

#[path = "recovery_attachment_tests.rs"]
mod recovery_attachment_tests;

fn label(name: &str) -> DbString {
    db_string(name).unwrap()
}

fn current_spec() -> (CandidateStateSpec, DbString, DbString, DbString, DbString) {
    let name = label("current");
    let doc = label("MemoryFact");
    let superseded = label("SUPERSEDED_BY");
    let contradicts = label("CONTRADICTS");
    let spec = CandidateStateSpec::new(name.clone())
        .require_label(doc.clone())
        .exclude_outgoing(superseded.clone())
        .exclude_incoming(contradicts.clone());
    (spec, name, doc, superseded, contradicts)
}

fn provider_with(spec: CandidateStateSpec) -> Arc<MaintainedCandidateStateProvider> {
    Arc::new(MaintainedCandidateStateProvider::new([spec]).unwrap())
}

#[test]
fn checkpoint_snapshot_requires_matching_candidate_generation() {
    let (spec, _, _, _, _) = current_spec();
    let provider = provider_with(spec);
    IndexProvider::write_section_at_generation(provider.as_ref(), SubTag(CANDIDATE_STATE_SUB), 0)
        .expect("initial generation matches");
    assert!(matches!(
        IndexProvider::write_section_at_generation(
            provider.as_ref(),
            SubTag(CANDIDATE_STATE_SUB),
            1,
        ),
        Err(ProviderError::Inconsistent { reason })
            if reason.contains("generation 0") && reason.contains("generation 1")
    ));
}

fn candidate_nodes(provider: &MaintainedCandidateStateProvider, name: &DbString) -> Vec<NodeId> {
    provider
        .candidate_set(name)
        .expect("candidate set is configured")
        .into_nodes()
}

#[test]
fn provider_tracks_label_and_edge_exclusions_through_commits() {
    let (spec, name, doc, superseded, contradicts) = current_spec();
    let provider = provider_with(spec);
    let shared = SharedGraph::builder(GraphId::new(81_001))
        .with_provider(provider.clone() as Arc<dyn IndexProvider>)
        .build()
        .unwrap();

    let (active, stale, unresolved, non_doc) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let unresolved = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        let non_doc = mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (active, stale, unresolved, non_doc)
    };

    assert_eq!(
        candidate_nodes(&provider, &name),
        vec![active, stale, unresolved]
    );
    assert_eq!(provider.generation(), 1);
    assert!(!provider.contains(&name, non_doc));

    let (stale_edge, contradiction_edge) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let stale_edge = mutator
            .create_edge(superseded, stale, active, PropertyMap::new())
            .unwrap();
        let contradiction_edge = mutator
            .create_edge(contradicts, non_doc, unresolved, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (stale_edge, contradiction_edge)
    };

    assert_eq!(candidate_nodes(&provider, &name), vec![active]);

    {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        mutator.delete_edge(stale_edge).unwrap();
        mutator.delete_edge(contradiction_edge).unwrap();
        txn.commit().unwrap();
    }

    assert_eq!(
        candidate_nodes(&provider, &name),
        vec![active, stale, unresolved]
    );
}

#[test]
fn shared_graph_lists_generation_checked_candidate_state_metadata() {
    let (spec, name, doc, superseded, contradicts) = current_spec();
    let provider = provider_with(spec);
    let shared = SharedGraph::builder(GraphId::new(81_016))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .unwrap();

    {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let unresolved = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        let blocked = mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .unwrap();
        mutator
            .create_edge(superseded.clone(), stale, active, PropertyMap::new())
            .unwrap();
        mutator
            .create_edge(contradicts.clone(), blocked, unresolved, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
    }

    let infos = shared.vector_candidate_state_infos().unwrap();

    assert_eq!(infos.len(), 1);
    let info = &infos[0];
    assert_eq!(info.name, name);
    assert_eq!(info.generation, shared.read().meta.generation);
    assert_eq!(info.candidate_count, 1);
    assert_eq!(info.required_label, Some(label("MemoryFact")));
    assert_eq!(info.require_outgoing, Vec::<DbString>::new());
    assert_eq!(info.require_incoming, Vec::<DbString>::new());
    assert_eq!(info.exclude_outgoing, vec![superseded]);
    assert_eq!(info.exclude_incoming, vec![contradicts]);
}

#[test]
fn shared_graph_resolves_generation_checked_candidate_state_set() {
    let (spec, name, doc, superseded, _) = current_spec();
    let provider = provider_with(spec);
    let shared = SharedGraph::builder(GraphId::new(81_017))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .unwrap();

    let (active, stale) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        mutator
            .create_edge(superseded, stale, active, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (active, stale)
    };

    let set = shared
        .vector_candidate_set(&name)
        .unwrap()
        .expect("candidate state exists");

    assert_eq!(set.as_nodes(), &[active]);
    assert!(!set.as_nodes().contains(&stale));
    assert!(
        shared
            .vector_candidate_set(&label("missing"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn shared_graph_resolves_typed_state_and_rebinds_after_layout_remint() {
    let (spec, name, doc, _, _) = current_spec();
    let provider = provider_with(spec);
    let shared = SharedGraph::builder(GraphId::new(81_018))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .unwrap();
    let (kept, deleted) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let kept = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let deleted = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (kept, deleted)
    };
    {
        let mut txn = shared.begin_write();
        txn.mutator().delete_node(deleted).unwrap();
        txn.commit().unwrap();
    }

    let before = shared.read();
    let typed_before = shared
        .node_candidate_set(&name)
        .unwrap()
        .expect("typed state exists");
    assert_eq!(typed_before.iter().collect::<Vec<_>>(), vec![kept]);
    assert_eq!(
        shared
            .vector_candidate_set(&name)
            .unwrap()
            .expect("legacy state exists")
            .as_nodes(),
        &[kept]
    );

    shared.compact().unwrap();
    let after = shared.read();
    assert!(!typed_before.shares_physical_layout_with(&after));
    let rebound = shared
        .node_candidate_set(&name)
        .unwrap()
        .expect("typed state rebinds");
    assert_eq!(rebound.iter().collect::<Vec<_>>(), vec![kept]);
    assert!(rebound.shares_physical_layout_with(&after));
    drop(before);
}

#[test]
fn provider_can_rebuild_from_existing_graph_snapshot() {
    let (spec, name, doc, superseded, _) = current_spec();
    let shared = SharedGraph::new(GraphId::new(81_002));
    let (active, stale) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        mutator
            .create_edge(superseded, stale, active, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (active, stale)
    };

    let provider = MaintainedCandidateStateProvider::from_graph([spec], shared.read().as_ref())
        .expect("provider rebuild succeeds");

    assert_eq!(provider.generation(), shared.read().meta.generation);
    assert_eq!(candidate_nodes(&provider, &name), vec![active]);
    assert!(!provider.contains(&name, stale));
}

#[test]
fn provider_generation_checked_candidate_set_rejects_stale_state() {
    let (spec, name, doc, _, _) = current_spec();
    let provider = provider_with(spec);
    let node = NodeId::new(1);
    provider
        .on_changes(&[Change::NodeCreated {
            id: node,
            labels: LabelSet::single(doc),
            properties: PropertyMap::new(),
        }])
        .expect("change applies");

    let err = provider
        .candidate_set_at_generation(&name, 1)
        .expect_err("watermark has not advanced");
    assert!(matches!(err, ProviderError::Inconsistent { .. }));

    provider.on_commit_applied(1).expect("watermark advances");
    assert_eq!(
        provider
            .candidate_set_at_generation(&name, 1)
            .expect("generation matches")
            .expect("set exists")
            .into_nodes(),
        vec![node]
    );
}

#[test]
fn provider_node_delete_prunes_incident_tracked_edges_without_edge_tombstones() {
    let (spec, name, doc, _, contradicts) = current_spec();
    let provider = provider_with(spec);
    let blocker = NodeId::new(1);
    let blocked = NodeId::new(2);
    let edge = EdgeId::new(1);

    provider
        .on_changes(&[
            Change::NodeCreated {
                id: blocker,
                labels: LabelSet::new(),
                properties: PropertyMap::new(),
            },
            Change::NodeCreated {
                id: blocked,
                labels: LabelSet::single(doc),
                properties: PropertyMap::new(),
            },
            Change::EdgeCreated {
                directionality: selene_core::EdgeDirectionality::Directed,
                id: edge,
                label: contradicts,
                source: blocker,
                target: blocked,
                properties: PropertyMap::new(),
            },
        ])
        .expect("initial changes apply");
    provider.on_commit_applied(1).expect("watermark advances");
    assert!(candidate_nodes(&provider, &name).is_empty());

    provider
        .on_changes(&[Change::NodeDeleted { id: blocker }])
        .expect("node delete applies");
    provider.on_commit_applied(2).expect("watermark advances");

    assert_eq!(candidate_nodes(&provider, &name), vec![blocked]);
}

#[test]
fn provider_snapshot_and_wal_replay_preserve_delete_reverse_state() {
    let (spec, name, doc, superseded, _) = current_spec();
    let provider = provider_with(spec.clone());
    let shared = SharedGraph::builder(GraphId::new(81_003))
        .with_provider(provider.clone() as Arc<dyn IndexProvider>)
        .build()
        .unwrap();
    let (active, stale, stale_edge) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        let edge = mutator
            .create_edge(superseded, stale, active, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (active, stale, edge)
    };
    assert_eq!(candidate_nodes(&provider, &name), vec![active]);

    let dir = temp_dir("candidate-state");
    write_snapshot(&dir, &shared, 1);
    append_wal(&dir, 1, &[Change::EdgeDeleted { id: stale_edge }]);

    let recovered_provider = provider_with(spec);
    let recovered = SharedGraph::recover_with_providers(
        &dir,
        GraphId::new(81_003),
        vec![recovered_provider.clone() as Arc<dyn IndexProvider>],
    )
    .unwrap();

    assert!(!recovered.read().is_edge_alive(stale_edge));
    assert_eq!(
        recovered_provider.generation(),
        recovered.read().meta.generation
    );
    assert_eq!(
        candidate_nodes(&recovered_provider, &name),
        vec![active, stale]
    );
    let recovered_snapshot = recovered.read();
    let typed = recovered
        .node_candidate_set(&name)
        .unwrap()
        .expect("recovered typed state exists");
    assert_eq!(typed.iter().collect::<Vec<_>>(), vec![active, stale]);
    assert!(typed.shares_physical_layout_with(&recovered_snapshot));
    assert!(typed.shares_workspace_binding_with(&recovered_snapshot));
}

#[test]
fn recovery_rebuilds_candidate_state_when_snapshot_section_is_absent() {
    let (spec, name, doc, superseded, _) = current_spec();
    let shared = SharedGraph::new(GraphId::new(81_007));
    let (active, stale) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        mutator
            .create_edge(superseded, stale, active, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (active, stale)
    };

    let dir = temp_dir("candidate-state-missing-section");
    write_snapshot(&dir, &shared, 1);

    let recovered_provider = provider_with(spec);
    let recovered = SharedGraph::recover_with_providers(
        &dir,
        GraphId::new(81_007),
        vec![recovered_provider.clone() as Arc<dyn IndexProvider>],
    )
    .unwrap();

    assert_eq!(
        recovered_provider.generation(),
        recovered.read().meta.generation
    );
    assert_eq!(candidate_nodes(&recovered_provider, &name), vec![active]);
    assert!(!recovered_provider.contains(&name, stale));
}

#[test]
fn provider_wal_replay_expands_declarative_edge_truncate_from_state() {
    let (spec, name, doc, superseded, _) = current_spec();
    let provider = provider_with(spec.clone());
    let shared = SharedGraph::builder(GraphId::new(81_005))
        .with_provider(provider.clone() as Arc<dyn IndexProvider>)
        .build()
        .unwrap();
    let (active, stale, stale_edge) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        let edge = mutator
            .create_edge(superseded.clone(), stale, active, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (active, stale, edge)
    };
    assert_eq!(candidate_nodes(&provider, &name), vec![active]);

    let dir = temp_dir("candidate-state-edge-truncate");
    write_snapshot(&dir, &shared, 1);
    append_wal(
        &dir,
        1,
        &[Change::EdgesOfTypeTruncated { label: superseded }],
    );

    let recovered_provider = provider_with(spec);
    let recovered = SharedGraph::recover_with_providers(
        &dir,
        GraphId::new(81_005),
        vec![recovered_provider.clone() as Arc<dyn IndexProvider>],
    )
    .unwrap();

    assert!(!recovered.read().is_edge_alive(stale_edge));
    assert_eq!(
        recovered_provider.generation(),
        recovered.read().meta.generation
    );
    assert_eq!(
        candidate_nodes(&recovered_provider, &name),
        vec![active, stale]
    );
}

#[test]
fn provider_wal_replay_expands_declarative_node_truncate_from_state() {
    let (spec, name, doc, superseded, _) = current_spec();
    let provider = provider_with(spec.clone());
    let shared = SharedGraph::builder(GraphId::new(81_006))
        .with_provider(provider.clone() as Arc<dyn IndexProvider>)
        .build()
        .unwrap();
    let (active, stale, stale_edge) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let edge = mutator
            .create_edge(superseded, stale, active, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (active, stale, edge)
    };
    assert_eq!(candidate_nodes(&provider, &name), vec![active]);

    let dir = temp_dir("candidate-state-node-truncate");
    write_snapshot(&dir, &shared, 1);
    append_wal(&dir, 1, &[Change::NodesOfTypeTruncated { label: doc }]);

    let recovered_provider = provider_with(spec);
    let recovered = SharedGraph::recover_with_providers(
        &dir,
        GraphId::new(81_006),
        vec![recovered_provider.clone() as Arc<dyn IndexProvider>],
    )
    .unwrap();

    assert!(!recovered.read().is_node_alive(active));
    assert!(!recovered.read().is_node_alive(stale));
    assert!(!recovered.read().is_edge_alive(stale_edge));
    assert_eq!(
        recovered_provider.generation(),
        recovered.read().meta.generation
    );
    assert!(candidate_nodes(&recovered_provider, &name).is_empty());
}

#[test]
fn duplicate_spec_names_are_rejected() {
    let name = label("current");
    let err = match MaintainedCandidateStateProvider::new([
        CandidateStateSpec::new(name.clone()),
        CandidateStateSpec::new(name),
    ]) {
        Ok(_) => panic!("duplicate specs must be rejected"),
        Err(error) => error,
    };

    assert!(matches!(err, ProviderError::Inconsistent { .. }));
}

#[test]
fn provider_rejects_snapshot_spec_drift() {
    let (spec, _, _, _, _) = current_spec();
    let provider = provider_with(spec);
    let bytes = provider
        .write_section(SubTag(CANDIDATE_STATE_SUB))
        .expect("snapshot section writes");
    let drifted = provider_with(CandidateStateSpec::new(label("other")));

    let err = drifted
        .read_section(SubTag(CANDIDATE_STATE_SUB), &bytes)
        .expect_err("spec drift must fail recovery");

    assert!(matches!(err, ProviderError::InvalidPayload { .. }));
}

#[test]
fn provider_rejects_snapshot_dangling_tracked_edge() {
    let (spec, _, doc, superseded, _) = current_spec();
    let provider = provider_with(spec.clone());
    let snapshot = CandidateStateSnapshot {
        version: SNAPSHOT_VERSION,
        generation: 7,
        specs: vec![spec],
        node_labels: vec![(NodeId::new(1), LabelSet::single(doc))],
        edges: vec![(
            EdgeId::new(1),
            TrackedEdge {
                label: superseded,
                source: NodeId::new(2),
                target: NodeId::new(1),
            },
        )],
    };
    let bytes = postcard::to_stdvec(&snapshot).unwrap();

    let err = provider
        .read_section(SubTag(CANDIDATE_STATE_SUB), &bytes)
        .expect_err("dangling tracked edge must fail recovery");

    assert!(matches!(err, ProviderError::InvalidPayload { .. }));
}

#[test]
fn provider_canonicalizes_public_spec_label_vectors() {
    let name = label("current");
    let doc = label("MemoryFact");
    let superseded = label("SUPERSEDED_BY");
    let other = label("OTHER");
    let spec = CandidateStateSpec {
        name: name.clone(),
        required_label: Some(doc.clone()),
        require_outgoing: Vec::new(),
        require_incoming: Vec::new(),
        exclude_outgoing: vec![other, superseded.clone(), superseded.clone()],
        exclude_incoming: Vec::new(),
    };
    let provider = provider_with(spec);
    let shared = SharedGraph::builder(GraphId::new(81_004))
        .with_provider(provider.clone() as Arc<dyn IndexProvider>)
        .build()
        .unwrap();
    let (active, stale) = {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        let active = mutator
            .create_node(LabelSet::single(doc.clone()), PropertyMap::new())
            .unwrap();
        let stale = mutator
            .create_node(LabelSet::single(doc), PropertyMap::new())
            .unwrap();
        mutator
            .create_edge(superseded, stale, active, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
        (active, stale)
    };

    assert_eq!(candidate_nodes(&provider, &name), vec![active]);
    assert!(!provider.contains(&name, stale));
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-graph-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    dir
}

fn write_snapshot(dir: &Path, shared: &SharedGraph, sequence: u64) {
    let outcome = shared
        .write_snapshot(SnapshotConfig {
            dir: dir.to_path_buf(),
            sequence,
            compression: SectionCompression::None,
            fsync: false,
        })
        .unwrap();
    assert_eq!(outcome.snapshot_seq, sequence);
}

fn append_wal(dir: &Path, snapshot_seq: u64, changes: &[Change]) {
    let mut writer = WalWriter::open(
        &dir.join(DEFAULT_WAL_FILE_NAME),
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq,
        },
    )
    .unwrap();
    writer
        .append(HlcTimestamp::zero(), Origin::Local, None, changes)
        .unwrap();
    writer.flush().unwrap();
}
