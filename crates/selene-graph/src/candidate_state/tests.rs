use std::sync::Arc;

use selene_core::{Change, GraphId, PropertyMap, db_string};

use super::*;
use crate::SharedGraph;

#[path = "required_edges_tests.rs"]
mod required_edges_tests;

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
fn candidate_discovery_requires_matching_generation() {
    let (spec, _, _, _, _) = current_spec();
    let provider = provider_with(spec);
    IndexProvider::vector_candidate_state_infos(provider.as_ref(), 0)
        .expect("initial generation matches");
    assert!(matches!(
        IndexProvider::vector_candidate_state_infos(provider.as_ref(), 1),
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
fn provider_rebuild_and_live_delete_preserve_reverse_state() {
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

    let recovered_provider =
        Arc::new(MaintainedCandidateStateProvider::from_graph([spec], &shared.read()).unwrap());
    let recovered = SharedGraph::from_graph_with_providers(
        shared.read().as_ref().clone(),
        vec![recovered_provider.clone() as Arc<dyn IndexProvider>],
    )
    .unwrap();
    let mut txn = recovered.begin_write();
    txn.mutator().delete_edge(stale_edge).unwrap();
    txn.commit().unwrap();
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
fn native_attachment_rebuilds_candidates_from_primary_graph() {
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

    let recovered_provider =
        Arc::new(MaintainedCandidateStateProvider::from_graph([spec], &shared.read()).unwrap());
    let recovered = SharedGraph::from_graph_with_providers(
        shared.read().as_ref().clone(),
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
fn provider_live_edge_truncate_and_rebuild_agree() {
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

    let recovered_provider =
        Arc::new(MaintainedCandidateStateProvider::from_graph([spec], &shared.read()).unwrap());
    let recovered = SharedGraph::from_graph_with_providers(
        shared.read().as_ref().clone(),
        vec![recovered_provider.clone() as Arc<dyn IndexProvider>],
    )
    .unwrap();
    let mut txn = recovered.begin_write();
    txn.mutator().truncate_edge_type(superseded).unwrap();
    txn.commit().unwrap();
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
fn provider_live_node_truncate_and_rebuild_agree() {
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

    let recovered_provider =
        Arc::new(MaintainedCandidateStateProvider::from_graph([spec], &shared.read()).unwrap());
    let recovered = SharedGraph::from_graph_with_providers(
        shared.read().as_ref().clone(),
        vec![recovered_provider.clone() as Arc<dyn IndexProvider>],
    )
    .unwrap();
    let mut txn = recovered.begin_write();
    txn.mutator().truncate_node_type(doc).unwrap();
    txn.commit().unwrap();
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
