//! Advanced, explicitly catalog-bound graph regressions (not facade activation).

use selene_catalog::*;
use selene_core::{DbString, SchemaPropertyIndexKind, SchemaVectorIndexKind, Value, db_string};
use selene_graph::{SeleneGraph, SharedGraph, TypedIndexKind, VectorIndexKind};

fn key(value: &str) -> DbString {
    db_string(value).unwrap()
}

fn physical() -> SeleneGraph {
    let graph = SharedGraph::new(selene_core::GraphId::new(1));
    selene_gql::Session::new(&graph).execute_source(
        "INSERT (a:Doc {p: 'alpha', body: 'alpha text'}), (b:Doc {p: 'beta', body: 'beta text'}), (a)-[:Link {p: 'alpha'}]->(b)",
        &selene_gql::BuiltinProcedureRegistry::new()).unwrap();
    let mut transaction = graph.begin_write();
    let mut m = transaction.mutator();
    m.create_property_index_named(
        key("Doc"),
        key("p"),
        TypedIndexKind::String,
        Some(key("scalar")),
    )
    .unwrap();
    m.create_edge_property_index_named(
        key("Link"),
        key("p"),
        TypedIndexKind::String,
        Some(key("edge")),
    )
    .unwrap();
    m.create_composite_property_index_named(
        key("Doc"),
        [key("p"), key("body")].into_iter().collect(),
        [TypedIndexKind::String; 2].into_iter().collect(),
        Some(key("composite")),
    )
    .unwrap();
    m.create_vector_index_named(
        key("Doc"),
        key("v"),
        VectorIndexKind::Flat,
        2,
        Some(key("vector")),
    )
    .unwrap();
    m.create_text_index_named(key("Doc"), key("body"), Some(key("text")))
        .unwrap();
    transaction.commit().unwrap();
    graph.read().as_ref().clone()
}

fn declarations(state: DeclarationState) -> Vec<CatalogDescriptor> {
    [
        (
            "scalar",
            ElementKind::Node,
            "Doc",
            vec!["p"],
            IndexConfiguration::Property(vec![SchemaPropertyIndexKind::String]),
        ),
        (
            "edge",
            ElementKind::Edge,
            "Link",
            vec!["p"],
            IndexConfiguration::Property(vec![SchemaPropertyIndexKind::String]),
        ),
        (
            "composite",
            ElementKind::Node,
            "Doc",
            vec!["p", "body"],
            IndexConfiguration::Property(vec![SchemaPropertyIndexKind::String; 2]),
        ),
        (
            "vector",
            ElementKind::Node,
            "Doc",
            vec!["v"],
            IndexConfiguration::Vector {
                kind: SchemaVectorIndexKind::Flat,
                dimension: 2,
                hnsw: None,
                ivf: None,
            },
        ),
        (
            "text",
            ElementKind::Node,
            "Doc",
            vec!["body"],
            IndexConfiguration::Text,
        ),
    ]
    .into_iter()
    .enumerate()
    .map(
        |(index, (name, element, label, properties, configuration))| {
            CatalogDescriptor::index(
                IndexId::new(index as u64 + 1).unwrap(),
                CatalogName::regular(name).unwrap(),
                CatalogParent::Graph(GraphId::new(1).unwrap()),
                CatalogGeneration::new(1).unwrap(),
                CreationMetadata::new(CatalogGeneration::new(1).unwrap(), None),
                IndexDeclaration {
                    metadata: DeclarationMetadata::new(state),
                    target: PropertyTarget {
                        element,
                        label: label.into(),
                        properties: properties.into_iter().map(String::from).collect(),
                    },
                    configuration,
                },
            )
            .unwrap()
        },
    )
    .collect()
}

fn catalog(declarations: Vec<CatalogDescriptor>) -> CatalogSnapshot {
    let generation = CatalogGeneration::new(1).unwrap();
    let creation = CreationMetadata::new(generation, None);
    let root = DirectoryId::new(1).unwrap();
    let catalog = CatalogId::new(1).unwrap();
    let mut builder = CatalogSnapshotBuilder::new(
        generation,
        CatalogDescriptor::catalog(
            catalog,
            CatalogName::regular("selene").unwrap(),
            generation,
            creation.clone(),
        )
        .unwrap(),
        CatalogDescriptor::root_directory(root, catalog, generation, creation.clone()).unwrap(),
    )
    .unwrap();
    builder
        .insert(
            CatalogDescriptor::schema(
                SchemaId::new(1).unwrap(),
                CatalogName::regular("s").unwrap(),
                root,
                generation,
                creation.clone(),
            )
            .unwrap(),
        )
        .unwrap();
    builder
        .insert(
            CatalogDescriptor::graph(
                GraphId::new(1).unwrap(),
                CatalogName::regular("g").unwrap(),
                SchemaId::new(1).unwrap(),
                generation,
                creation,
                None,
            )
            .unwrap(),
        )
        .unwrap();
    for declaration in declarations {
        builder.insert(declaration).unwrap();
    }
    builder.build().unwrap()
}

fn bound(state: DeclarationState) -> SeleneGraph {
    let mut graph = physical();
    graph.bind_catalog(&catalog(declarations(state))).unwrap();
    graph
}

#[test]
fn aliases_cannot_mask_an_unadvertised_physical_registration() {
    let mut graph = physical();
    let mut descriptors = declarations(DeclarationState::Ready);
    let first = descriptors[0].clone();
    // Replace the edge declaration by a second ready alias of the scalar.
    // The old count-only admission still counts five matches against five indexes.
    descriptors[1] = CatalogDescriptor::index(
        IndexId::new(2).unwrap(),
        CatalogName::regular("alias").unwrap(),
        first.parent(),
        first.generation(),
        first.creation().clone(),
        match first.payload() {
            CatalogPayload::Index(index) => index.clone(),
            _ => unreachable!(),
        },
    )
    .unwrap();
    assert!(graph.bind_catalog(&catalog(descriptors)).is_err());
}

#[test]
fn every_ineligible_lifecycle_declines_candidate_and_cardinality_probes() {
    for state in [
        DeclarationState::Inactive,
        DeclarationState::Building,
        DeclarationState::Failed,
    ] {
        let graph = bound(state);
        let label = key("Doc");
        let property = key("p");
        let value = Value::String(key("alpha"));
        assert!(
            graph
                .node_candidates_with_property_eq(&label, &property, &value)
                .unwrap()
                .is_none(),
            "{state:?}"
        );
        assert!(
            graph
                .node_candidates_with_property_any(&label, &property, &[])
                .unwrap()
                .is_none()
        );
        assert!(
            graph
                .node_candidates_with_property_range(&label, &property, ..)
                .unwrap()
                .is_none()
        );
        assert!(
            graph
                .node_candidates_with_property_prefix(&label, &property, "a")
                .unwrap()
                .is_none()
        );
        assert!(
            graph
                .edge_candidates_with_property_eq(&key("Link"), &property, &value)
                .unwrap()
                .is_none()
        );
        assert!(
            graph
                .edge_candidates_with_property_any(&key("Link"), &property, &[])
                .unwrap()
                .is_none()
        );
        assert!(
            graph
                .edge_candidates_with_property_range(&key("Link"), &property, ..)
                .unwrap()
                .is_none()
        );
        assert!(
            graph
                .node_property_eq_cardinality(&label, &property, &value)
                .is_none()
        );
        assert!(
            graph
                .edge_property_range_cardinality(&key("Link"), &property, ..)
                .is_none()
        );
        assert!(
            graph
                .composite_property_index_for(&label, &[key("body"), property.clone()])
                .is_none()
        );
        assert!(graph.vector_index_for(&label, &key("v")).is_none());
        assert!(graph.text_index_for(&label, &key("body")).is_none());
    }
}

#[test]
fn required_property_filters_reject_ineligible_backing() {
    for state in [
        DeclarationState::Inactive,
        DeclarationState::Building,
        DeclarationState::Failed,
    ] {
        let mut physical = physical();
        let mut descriptors = declarations(DeclarationState::Ready);
        for descriptor in &mut descriptors[..2] {
            let mut payload = descriptor.payload().clone();
            let CatalogPayload::Index(index) = &mut payload else {
                unreachable!()
            };
            index.metadata.state = state;
            *descriptor = CatalogDescriptor::new(
                descriptor.id(),
                descriptor.kind(),
                descriptor.name().clone(),
                descriptor.parent(),
                descriptor.generation(),
                descriptor.creation().clone(),
                payload,
            )
            .unwrap();
        }
        physical.bind_catalog(&catalog(descriptors)).unwrap();
        let graph = SharedGraph::try_from_graph(physical).unwrap();
        let ready = SharedGraph::try_from_graph(bound(DeclarationState::Ready)).unwrap();
        let registry = selene_gql::BuiltinProcedureRegistry::new();
        for query in [
            "CALL selene.text_search_nodes('Doc', 'body', 'alpha', 10, 'p', $values) YIELD node_id, score",
            "CALL selene.text_search_nodes('Doc', 'body', 'alpha', 10, NULL, NULL, 'Link', 'p', $values, 'source') YIELD node_id, score",
        ] {
            let mut control = selene_gql::Session::new(&ready);
            control.bind_parameter(
                key("values"),
                Value::List(vec![Value::String(key("alpha"))]),
            );
            let selene_gql::StatementOutput::Rows(rows) =
                control.execute_source(query, &registry).unwrap()
            else {
                panic!("control rows")
            };
            assert_eq!(rows.row_count(), 1);
            let mut session = selene_gql::Session::new(&graph);
            session.bind_parameter(
                key("values"),
                Value::List(vec![Value::String(key("alpha"))]),
            );
            let error = session.execute_source(query, &registry).unwrap_err();
            assert_eq!(
                error.gqlstatus(),
                selene_gql::GqlStatus::INVALID_PROCEDURE_ARGUMENT
            );
        }
    }
}

#[test]
fn vector_diagnostics_audit_physical_backing_even_when_ineligible() {
    for state in [
        DeclarationState::Inactive,
        DeclarationState::Building,
        DeclarationState::Failed,
    ] {
        let graph = SharedGraph::try_from_graph(bound(state)).unwrap();
        let registry = selene_gql::BuiltinProcedureRegistry::new();
        let result = selene_gql::Session::new(&graph)
            .execute_source(
                "CALL selene.vector_index_stats() YIELD name, indexed_rows",
                &registry,
            )
            .unwrap();
        let selene_gql::StatementOutput::Rows(table) = result else {
            panic!("diagnostic rows")
        };
        assert_eq!(table.rows().len(), 1);
        assert_eq!(table.rows()[0].values()[0], Value::String(key("vector")));
        selene_gql::Session::new(&graph)
            .execute_source("CALL selene.verify()", &registry)
            .unwrap();
    }
}

#[test]
fn advanced_compaction_preserves_all_ineligible_bindings() {
    for state in [
        DeclarationState::Inactive,
        DeclarationState::Building,
        DeclarationState::Failed,
    ] {
        let graph = SharedGraph::try_from_graph(bound(state)).unwrap();
        graph.compact().unwrap();
        let snapshot = graph.read();
        assert!(
            snapshot
                .property_index_for(&key("Doc"), &key("p"))
                .is_none(),
            "{state:?}"
        );
        assert!(
            snapshot
                .edge_property_index_for(&key("Link"), &key("p"))
                .is_none()
        );
        assert!(
            snapshot
                .composite_property_index_for(&key("Doc"), &[key("p"), key("body")])
                .is_none()
        );
        assert!(snapshot.vector_index_for(&key("Doc"), &key("v")).is_none());
        assert!(snapshot.text_index_for(&key("Doc"), &key("body")).is_none());
        assert_eq!(snapshot.node_count(), 2);
        assert_eq!(snapshot.edge_count(), 1);
    }
}

#[test]
fn keyed_bindings_reject_changed_owner_name_and_configuration() {
    let mut graph = bound(DeclarationState::Ready);
    let label = key("Doc");
    let property = key("p");
    assert!(graph.property_index_for(&label, &property).is_some());
    graph.meta.graph_id = selene_core::GraphId::new(2);
    assert!(graph.property_index_for(&label, &property).is_none());
    graph.meta.graph_id = selene_core::GraphId::new(1);
    graph
        .property_index
        .get_mut(&(label.clone(), property.clone()))
        .unwrap()
        .name = Some(key("renamed"));
    assert!(graph.property_index_for(&label, &property).is_none());
    graph
        .property_index
        .get_mut(&(label.clone(), property.clone()))
        .unwrap()
        .name = Some(key("scalar"));
    assert!(graph.property_index_for(&label, &property).is_some());
    graph
        .property_index
        .get_mut(&(label.clone(), property.clone()))
        .unwrap()
        .index = std::sync::Arc::new(selene_graph::TypedIndex::new(TypedIndexKind::I64));
    assert!(graph.property_index_for(&label, &property).is_none());
    assert!(
        graph
            .node_candidates_with_property_any(&label, &property, &[])
            .unwrap()
            .is_none()
    );
}

#[test]
fn required_filters_keep_bad_value_errors_and_drift_scan_fallback() {
    let graph = SharedGraph::try_from_graph(bound(DeclarationState::Ready)).unwrap();
    let registry = selene_gql::BuiltinProcedureRegistry::new();
    for drifted in [false, true] {
        if drifted {
            selene_gql::Session::new(&graph)
                .execute_source("MATCH (n:Doc) WHERE n.p = 'beta' SET n.p = 1", &registry)
                .unwrap();
        }
        let snapshot = graph.read();
        let info = snapshot
            .property_index_read_info(selene_graph::IndexedEntity::Node, &key("Doc"), &key("p"))
            .unwrap();
        assert_eq!(info.is_complete(), !drifted);
        assert!(info.admits(&Value::String(key("alpha"))));
        assert!(!info.admits(&Value::Int(1)));
        drop(snapshot);
        for (values, rows) in [(vec![Value::String(key("alpha"))], 1), (vec![], 0)] {
            let mut session = selene_gql::Session::new(&graph);
            session.bind_parameter(key("values"), Value::List(values));
            let selene_gql::StatementOutput::Rows(table) = session.execute_source(
                "CALL selene.text_search_nodes('Doc', 'body', 'alpha', 10, 'p', $values) YIELD node_id, score", &registry).unwrap() else { panic!("rows") };
            assert_eq!(table.row_count(), rows);
        }
        let mut session = selene_gql::Session::new(&graph);
        session.bind_parameter(key("values"), Value::List(vec![Value::Int(1)]));
        let error = session.execute_source("CALL selene.text_search_nodes('Doc', 'body', 'alpha', 10, 'p', $values) YIELD node_id, score", &registry).unwrap_err();
        assert_eq!(
            error.gqlstatus(),
            selene_gql::GqlStatus::INVALID_PROCEDURE_ARGUMENT
        );
    }
}
