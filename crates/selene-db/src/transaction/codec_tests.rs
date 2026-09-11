use super::*;
use crate::{CreatePolicy, Database, ObjectPath, SchemaPath, catalog_stage::CatalogStager};
use selene_core::{GraphId, LabelSet, PropertyMap, logical::Limits};
use selene_graph::logical_transaction::ReplayState;

#[test]
fn real_catalog_draft_encodes_all_named_graphs_without_publication() {
    let database = Database::builder().build();
    let inner = database.catalog().inner.clone();
    let base = inner.state.load_full();
    let seed = database.catalog().snapshot().logical_catalog().unwrap();
    let replay = ReplayState::seed(seed).unwrap();
    inner.with_mutation_reservation(|reservation| {
        let mut draft = DatabaseDraft::new(&base, &reservation);
        let schema = SchemaPath::regular("selene", "codec").unwrap();
        let mut stage = CatalogStager::new(&inner, &mut draft);
        stage.create_schema(&schema, CreatePolicy::Strict).unwrap();
        for name in ["one", "two"] {
            stage
                .create_graph(
                    &ObjectPath::regular("selene", "codec", name).unwrap(),
                    None,
                    CreatePolicy::Strict,
                )
                .unwrap();
        }
        let transaction = draft.logical_transaction(&base).unwrap();
        assert_eq!(transaction.graphs.len(), 2);
        let bytes = transaction.encode(Limits::default()).unwrap();
        let candidate = replay.apply_body(&bytes, Limits::default()).unwrap();
        assert_eq!(candidate.graph_summary(GraphId::new(1)), Some((0, 0, 1, 1)));
        assert_eq!(candidate.graph_summary(GraphId::new(2)), Some((0, 0, 1, 1)));
        assert!(Arc::ptr_eq(&base, &inner.state.load_full()));
        assert!(base.graphs.is_empty());
    });
}

#[test]
fn real_prepared_statement_sequence_retains_every_logical_change() {
    let database = Database::builder().build();
    let schema = SchemaPath::regular("selene", "codec_sequence").unwrap();
    let path = ObjectPath::regular("selene", "codec_sequence", "main").unwrap();
    database
        .catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    database
        .catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let id = selene_catalog::GraphId::new(
        database
            .catalog()
            .snapshot()
            .resolve_graph(&path)
            .unwrap()
            .id
            .get(),
    )
    .unwrap();
    let inner = database.catalog().inner.clone();
    let base = inner.state.load_full();
    inner.with_mutation_reservation(|reservation| {
        let mut draft = DatabaseDraft::new(&base, &reservation);
        draft.pin_graph(&base, id).unwrap();
        for _ in 0..2 {
            let scratch = draft.mutation_scratch().unwrap();
            let mut tx = scratch.begin_write();
            tx.mutator()
                .create_node(LabelSet::new(), PropertyMap::new())
                .unwrap();
            draft
                .attach_prepared_graph(id, tx.prepare_unpublished(None, None).unwrap())
                .unwrap();
        }
        let transaction = draft.logical_transaction(&base).unwrap();
        assert_eq!(transaction.graphs[0].changes.len(), 2);
        assert_eq!(transaction.graphs[0].next_node_id, 3);
        assert!(Arc::ptr_eq(&base, &inner.state.load_full()));
        assert_eq!(base.graphs[&id].graph.read().node_count(), 0);
    });
}

#[test]
fn real_draft_registered_named_type_binding_is_enforced_after_codec_decode() {
    use crate::{GraphTypeDefinition, NodeTypeDefinition, PathSegment};
    use selene_catalog::{CatalogObjectId, CatalogPayload};
    use selene_core::{Change, NodeId};
    let database = Database::builder().build();
    let inner = database.catalog().inner.clone();
    let base = inner.state.load_full();
    let replay =
        ReplayState::seed(database.catalog().snapshot().logical_catalog().unwrap()).unwrap();
    inner.with_mutation_reservation(|reservation| {
        let mut draft = DatabaseDraft::new(&base, &reservation);
        let type_path = ObjectPath::regular("selene", "named_codec", "Blueprint").unwrap();
        let graph_path = ObjectPath::regular("selene", "named_codec", "data").unwrap();
        let definition = GraphTypeDefinition::builder()
            .with_node_type(
                NodeTypeDefinition::new(
                    PathSegment::regular("Thing").unwrap(),
                    vec![PathSegment::regular("L").unwrap()],
                )
                .unwrap(),
            )
            .build()
            .unwrap();
        let mut stage = CatalogStager::new(&inner, &mut draft);
        stage
            .create_schema(
                &SchemaPath::regular("selene", "named_codec").unwrap(),
                CreatePolicy::Strict,
            )
            .unwrap();
        stage
            .create_graph_type(&type_path, definition, CreatePolicy::Strict)
            .unwrap();
        stage
            .create_graph(&graph_path, Some(&type_path), CreatePolicy::Strict)
            .unwrap();
        let mut tx = draft.logical_transaction(&base).unwrap();
        assert_eq!(tx.graph_types.len(), 1);
        let records = tx.catalog.apply(replay.catalog()).unwrap();
        let snapshot = records.reconstruct().unwrap();
        let graph_id = selene_catalog::GraphId::new(tx.graphs[0].id.get()).unwrap();
        assert_eq!(
            snapshot
                .descriptor(CatalogObjectId::Graph(graph_id))
                .unwrap()
                .payload(),
            &CatalogPayload::Graph {
                graph_type: Some(tx.graph_types[0].id)
            }
        );
        assert_eq!(
            tx.graphs[0].definition.as_ref().unwrap().name.as_str(),
            "Blueprint"
        );
        let valid = replay
            .apply_body(&tx.encode(Limits::default()).unwrap(), Limits::default())
            .unwrap();
        assert_eq!(
            valid.graph_summary(GraphId::new(graph_id.get())),
            Some((0, 0, 1, 1))
        );
        // Tamper only with the instance's claim and data, leaving the real catalog
        // binding and complete registered type body intact.
        tx.graphs[0].definition = None;
        tx.graphs[0].generation = 1;
        tx.graphs[0].next_node_id = 2;
        tx.graphs[0].changes.push(Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::new(),
            properties: PropertyMap::new(),
        });
        assert!(
            replay
                .apply_body(&tx.encode(Limits::default()).unwrap(), Limits::default())
                .is_err()
        );
        assert!(Arc::ptr_eq(&base, &inner.state.load_full()));
        assert!(base.graphs.is_empty());
    });
}

#[test]
fn draft_type_descriptor_revision_carries_unchanged_runtime_body() {
    use crate::{GraphTypeDefinition, NodeTypeDefinition, PathSegment};
    use selene_catalog::{CatalogDescriptor, CatalogObjectId, CatalogTransaction};
    let database = Database::builder().build();
    let schema = SchemaPath::regular("selene", "type_revision_codec").unwrap();
    let path = ObjectPath::regular("selene", "type_revision_codec", "Blueprint").unwrap();
    database
        .catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    let definition = GraphTypeDefinition::builder()
        .with_node_type(
            NodeTypeDefinition::new(
                PathSegment::regular("Thing").unwrap(),
                vec![PathSegment::regular("L").unwrap()],
            )
            .unwrap(),
        )
        .build()
        .unwrap();
    database
        .catalog()
        .create_graph_type(&path, definition, CreatePolicy::Strict)
        .unwrap();
    let inner = database.catalog().inner.clone();
    let base = inner.state.load_full();
    inner.with_mutation_reservation(|reservation| {
        let mut draft = DatabaseDraft::new(&base, &reservation);
        let (&id, original) = base.graph_types.first_key_value().unwrap();
        let object = CatalogObjectId::GraphType(id);
        let descriptor = draft.catalog.descriptor(object).unwrap();
        let mut catalog = CatalogTransaction::new(&draft.catalog).unwrap();
        assert!(catalog.remove(object).is_some());
        catalog
            .insert(
                CatalogDescriptor::new(
                    object,
                    descriptor.kind(),
                    descriptor.name().clone(),
                    descriptor.parent(),
                    catalog.generation(),
                    descriptor.creation().clone(),
                    descriptor.payload().clone(),
                )
                .unwrap(),
            )
            .unwrap();
        draft.catalog = catalog.build().unwrap();
        let tx = draft.logical_transaction(&base).unwrap();
        assert_eq!(tx.graph_types.len(), 1);
        assert_eq!(tx.graph_types[0].id, id);
        assert_eq!(
            tx.graph_types[0].definition.as_ref().unwrap(),
            &selene_graph::logical_transaction::definition(original).unwrap()
        );
        assert!(Arc::ptr_eq(&base, &inner.state.load_full()));
    });
}
