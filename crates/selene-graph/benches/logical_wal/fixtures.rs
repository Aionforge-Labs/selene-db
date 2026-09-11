use selene_catalog::codec::CatalogDelta;
use selene_catalog::*;
use selene_core::{
    Change, EdgeDirectionality, EdgeId, GraphId as CoreGraphId, JsonValue, LabelSet, NodeId,
    PropertyMap, Record, Value, VectorValue, db_string, logical::GraphDelta,
};
use selene_graph::logical_transaction::LogicalTransaction;
use selene_persist::{
    control::{StoreEpoch, StoreId},
    logical_frame::Context,
};
use std::collections::BTreeMap;

pub fn context() -> Context {
    let mut id = [0; 16];
    id[6] = 0x40;
    id[8] = 0x80;
    Context {
        store: StoreId::from_bytes(id).unwrap(),
        epoch: StoreEpoch::new(1).unwrap(),
        sequence: 1,
        segment: [1; 32],
        previous: [2; 32],
    }
}
fn gen2() -> CatalogGeneration {
    CatalogGeneration::new(2).unwrap()
}
fn value(shape: &str, index: usize) -> Value {
    match shape {
        "scalar" | "catalog"=>Value::Int((index * 17) as i64),
        "vector"=>Value::Vector(VectorValue::new((0..384).map(|i| ((i * 7 + index) as f32).sin()).collect::<Vec<_>>()).unwrap()),
        "json"=>Value::Json(JsonValue::parse_str(&format!("{{\"kind\":\"memory\",\"id\":{index},\"active\":true,\"source\":\"codec benchmark\"}}")).unwrap()),
        "list"=>Value::List((0..32).map(|i| Value::Int((index + i) as i64)).collect()),
        "record"=>Value::Record(Box::new(Record::Open(smallvec::smallvec![
            (db_string("id").unwrap(), Value::Uint128(index as u128)),
            (db_string("payload").unwrap(), value("json", index)),
            (db_string("numbers").unwrap(), Value::List(vec![Value::Int(-9), Value::Null, Value::Float32(0.5)])),
        ]))),
        "threshold"=>Value::Bytes([].into()),
        _=>panic!("unknown benchmark shape"),
    }
}
pub fn transaction(shape: &str, count: usize) -> LogicalTransaction {
    let generation = CatalogGeneration::new(1).unwrap();
    let catalog_id = CatalogId::new(1).unwrap();
    let root = DirectoryId::new(1).unwrap();
    let creation = CreationMetadata::new(generation, None);
    let snapshot = CatalogSnapshotBuilder::new(
        generation,
        CatalogDescriptor::catalog(
            catalog_id,
            CatalogName::regular("selene").unwrap(),
            generation,
            creation.clone(),
        )
        .unwrap(),
        CatalogDescriptor::root_directory(root, catalog_id, generation, creation).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap();
    let mut descriptors: Vec<_> = snapshot.descriptors().cloned().collect();
    let schema = SchemaId::new(1).unwrap();
    let creation = CreationMetadata::new(gen2(), None);
    descriptors.push(
        CatalogDescriptor::schema(
            schema,
            CatalogName::regular("data").unwrap(),
            root,
            gen2(),
            creation.clone(),
        )
        .unwrap(),
    );
    let mut graphs = Vec::new();
    for graph in 1..=2 {
        descriptors.push(
            CatalogDescriptor::graph(
                GraphId::new(graph).unwrap(),
                CatalogName::regular(format!("g{graph}")).unwrap(),
                schema,
                gen2(),
                creation.clone(),
                None,
            )
            .unwrap(),
        );
        let mut changes = Vec::new();
        for index in 1..=count {
            changes.push(Change::NodeCreated {
                id: NodeId::new(index as u64),
                labels: LabelSet::new(),
                properties: PropertyMap::from_pairs([(
                    db_string("payload").unwrap(),
                    value(shape, index),
                )])
                .unwrap(),
            });
        }
        for index in 1..count {
            changes.push(Change::EdgeCreated {
                id: EdgeId::new(index as u64),
                directionality: if index % 2 == 0 {
                    EdgeDirectionality::Directed
                } else {
                    EdgeDirectionality::Undirected
                },
                label: db_string("LINK").unwrap(),
                source: NodeId::new(index as u64),
                target: NodeId::new(index as u64 + 1),
                properties: PropertyMap::new(),
            });
        }
        graphs.push(GraphDelta {
            id: CoreGraphId::new(graph),
            previous: None,
            generation: 1,
            next_node_id: count as u64 + 1,
            next_edge_id: count.max(1) as u64,
            definition: None,
            backing_indexes: vec![],
            changes,
        });
    }
    let native_count = if shape == "catalog" { count } else { 0 };
    for i in 1..=native_count {
        descriptors.push(
            CatalogDescriptor::procedure(
                ProcedureId::new(i as u64).unwrap(),
                CatalogName::regular(format!("declaration_{i}")).unwrap(),
                CatalogParent::Graph(GraphId::new(1).unwrap()),
                gen2(),
                creation.clone(),
                NativeDeclaration {
                    metadata: DeclarationMetadata::new(DeclarationState::Inactive),
                    binding: NativeBinding::Projection(NativeProjection {
                        node_labels: vec![format!("L{i}")],
                        edge_labels: vec!["LINK".into()],
                        weight_property: Some("weight".into()),
                    }),
                },
            )
            .unwrap(),
        );
    }
    let records = CatalogLogicalRecords::new(
        gen2(),
        BTreeMap::from([
            (CatalogObjectKind::Catalog, 1),
            (CatalogObjectKind::Directory, 1),
            (CatalogObjectKind::Schema, 1),
            (CatalogObjectKind::Graph, 2),
            (CatalogObjectKind::Procedure, native_count as u64),
        ]),
        descriptors,
    )
    .unwrap();
    LogicalTransaction {
        catalog: CatalogDelta::between(&snapshot, &records).unwrap(),
        graph_types: vec![],
        graphs,
    }
}
