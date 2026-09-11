//! Small valid seed for mutation past framing into named-type replay validation.

use selene_catalog::codec::CatalogDelta;
use selene_catalog::*;
use selene_core::{
    Change, GraphId as CoreGraphId, LabelSet, NodeId, PropertyMap, Value, db_string,
    logical::{GraphDefinition, GraphDelta, Limits},
};
use selene_graph::logical_transaction::{LogicalTransaction, ReplayState, TypeDelta};
use std::{collections::BTreeMap, sync::OnceLock};

pub fn fixture() -> &'static (ReplayState, Vec<u8>) {
    static FIXTURE: OnceLock<(ReplayState, Vec<u8>)> = OnceLock::new();
    FIXTURE.get_or_init(build)
}
fn generation(raw: u64) -> CatalogGeneration {
    CatalogGeneration::new(raw).unwrap()
}
fn node(raw: u64) -> Change {
    Change::NodeCreated {
        id: NodeId::new(raw),
        labels: LabelSet::from_iter([db_string("L").unwrap()]),
        properties: PropertyMap::from_pairs([(db_string("v").unwrap(), Value::Int(raw as i64))])
            .unwrap(),
    }
}
fn build() -> (ReplayState, Vec<u8>) {
    let catalog = CatalogId::new(1).unwrap();
    let directory = DirectoryId::new(1).unwrap();
    let creation = CreationMetadata::new(generation(1), None);
    let roots = CatalogSnapshotBuilder::new(
        generation(1),
        CatalogDescriptor::catalog(
            catalog,
            CatalogName::regular("selene").unwrap(),
            generation(1),
            creation.clone(),
        )
        .unwrap(),
        CatalogDescriptor::root_directory(directory, catalog, generation(1), creation).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap();
    let water = BTreeMap::from([
        (CatalogObjectKind::Catalog, 1),
        (CatalogObjectKind::Directory, 1),
    ]);
    let seed = ReplayState::seed(
        CatalogLogicalRecords::new(
            generation(1),
            water.clone(),
            roots.descriptors().cloned().collect(),
        )
        .unwrap(),
    )
    .unwrap();
    let schema = SchemaId::new(1).unwrap();
    let type_id = GraphTypeId::new(1).unwrap();
    let mut descriptors: Vec<_> = roots.descriptors().cloned().collect();
    let creation = CreationMetadata::new(generation(2), None);
    descriptors.push(
        CatalogDescriptor::schema(
            schema,
            CatalogName::regular("data").unwrap(),
            directory,
            generation(2),
            creation.clone(),
        )
        .unwrap(),
    );
    descriptors.push(
        CatalogDescriptor::graph_type(
            type_id,
            CatalogName::regular("Blueprint").unwrap(),
            schema,
            generation(2),
            creation.clone(),
        )
        .unwrap(),
    );
    for raw in [1, 2] {
        descriptors.push(
            CatalogDescriptor::graph(
                GraphId::new(raw).unwrap(),
                CatalogName::regular(format!("g{raw}")).unwrap(),
                schema,
                generation(2),
                creation.clone(),
                Some(type_id),
            )
            .unwrap(),
        );
    }
    let mut water = water;
    water.extend([
        (CatalogObjectKind::Schema, 1),
        (CatalogObjectKind::GraphType, 1),
        (CatalogObjectKind::Graph, 2),
    ]);
    let records = CatalogLogicalRecords::new(generation(2), water, descriptors).unwrap();
    let mut value_type = selene_core::ValueType::predefined(selene_core::PredefinedValueType::Int);
    value_type.not_null = true;
    let mut node_type =
        selene_core::NodeTypeDef::new(LabelSet::from_iter([db_string("L").unwrap()]));
    node_type.properties.push(selene_core::PropertyDef {
        name: db_string("v").unwrap(),
        value_type,
        nullable: false,
        default: None,
        immutable: true,
        unique: false,
        record_fields: None,
    });
    let definition = GraphDefinition {
        name: db_string("Blueprint").unwrap(),
        nodes: vec![(db_string("Thing").unwrap(), node_type)],
        edges: vec![],
    };
    let mut tx = LogicalTransaction {
        catalog: CatalogDelta::between(&roots, &records).unwrap(),
        graph_types: vec![TypeDelta {
            id: type_id,
            definition: Some(definition.clone()),
        }],
        graphs: [1, 2]
            .map(|raw| GraphDelta {
                id: CoreGraphId::new(raw),
                previous: None,
                generation: 1,
                next_node_id: 2,
                next_edge_id: 1,
                definition: Some(definition.clone()),
                backing_indexes: vec![],
                changes: vec![node(1)],
            })
            .into(),
    };
    let state = seed
        .apply_body(&tx.encode(Limits::default()).unwrap(), Limits::default())
        .unwrap();
    let snapshot = records.reconstruct().unwrap();
    let descriptors = snapshot
        .descriptors()
        .map(|d| {
            if d.id() == CatalogObjectId::GraphType(type_id) {
                CatalogDescriptor::new(
                    d.id(),
                    d.kind(),
                    d.name().clone(),
                    d.parent(),
                    generation(3),
                    d.creation().clone(),
                    d.payload().clone(),
                )
                .unwrap()
            } else {
                d.clone()
            }
        })
        .collect();
    let revision =
        CatalogLogicalRecords::new(generation(3), records.high_water().clone(), descriptors)
            .unwrap();
    tx.catalog = CatalogDelta::between(&snapshot, &revision).unwrap();
    tx.graphs.truncate(1);
    tx.graphs[0].previous = Some(1);
    tx.graphs[0].generation = 2;
    tx.graphs[0].next_node_id = 3;
    tx.graphs[0].changes = vec![node(2)];
    let bytes = tx.encode(Limits::default()).unwrap();
    assert!(state.apply_body(&bytes, Limits::default()).is_ok());
    (state, bytes)
}
