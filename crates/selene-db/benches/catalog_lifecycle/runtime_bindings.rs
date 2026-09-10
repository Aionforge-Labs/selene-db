//! Same physical data/indexes, varying only owner declaration count and binding.

use criterion::{BenchmarkId, Criterion};
use selene_catalog::*;
use selene_core::{
    LabelSet, PropertyMap, SchemaPropertyIndexKind, SchemaVectorIndexKind, Value, VectorValue,
    db_string,
};
use selene_graph::{SeleneGraph, SharedGraph, TypedIndexKind, VectorIndexKind};
use std::hint::black_box;

fn physical(vector: bool) -> SeleneGraph {
    let graph = SharedGraph::new(selene_core::GraphId::new(1));
    let label = db_string("Doc").unwrap();
    let property = db_string("value").unwrap();
    let mut transaction = graph.begin_write();
    {
        let mut m = transaction.mutator();
        for index in 0..1024 {
            let value = if vector {
                let mut components = vec![0.0; 16];
                components[0] = index as f32;
                Value::Vector(VectorValue::new(components).unwrap())
            } else {
                Value::Int(index)
            };
            m.create_node(
                LabelSet::from_iter([label.clone()]),
                PropertyMap::from_pairs([(property.clone(), value)]).unwrap(),
            )
            .unwrap();
        }
        if vector {
            m.create_vector_index_named(
                label,
                property,
                VectorIndexKind::Flat,
                16,
                Some(db_string("zz_query").unwrap()),
            )
            .unwrap();
        } else {
            m.create_property_index_named(
                label,
                property,
                TypedIndexKind::I64,
                Some(db_string("zz_query").unwrap()),
            )
            .unwrap();
        }
    }
    transaction.commit().unwrap();
    graph.read().as_ref().clone()
}

fn bind(mut graph: SeleneGraph, count: usize, vector: bool) -> SeleneGraph {
    let generation = CatalogGeneration::new(1).unwrap();
    let creation = CreationMetadata::new(generation, None);
    let catalog = CatalogId::new(1).unwrap();
    let root = DirectoryId::new(1).unwrap();
    let schema = SchemaId::new(1).unwrap();
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
                schema,
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
                schema,
                generation,
                creation.clone(),
                None,
            )
            .unwrap(),
        )
        .unwrap();
    for index in 0..count {
        let query = index + 1 == count;
        let name = if query {
            "zz_query".to_owned()
        } else {
            format!("inactive_{index:03}")
        };
        let property = if query {
            "value".to_owned()
        } else {
            format!("absent_{index:03}")
        };
        let configuration = if vector {
            IndexConfiguration::Vector {
                kind: SchemaVectorIndexKind::Flat,
                dimension: 16,
                hnsw: None,
                ivf: None,
            }
        } else {
            IndexConfiguration::Property(vec![SchemaPropertyIndexKind::I64])
        };
        builder
            .insert(
                CatalogDescriptor::index(
                    IndexId::new(index as u64 + 1).unwrap(),
                    CatalogName::regular(name).unwrap(),
                    CatalogParent::Graph(GraphId::new(1).unwrap()),
                    generation,
                    creation.clone(),
                    IndexDeclaration {
                        metadata: DeclarationMetadata::new(if query {
                            DeclarationState::Ready
                        } else {
                            DeclarationState::Inactive
                        }),
                        target: PropertyTarget {
                            element: ElementKind::Node,
                            label: "Doc".into(),
                            properties: vec![property],
                        },
                        configuration,
                    },
                )
                .unwrap(),
            )
            .unwrap();
    }
    graph.bind_catalog(&builder.build().unwrap()).unwrap();
    graph
}

pub(super) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("catalog_runtime_binding");
    let label = db_string("Doc").unwrap();
    let property = db_string("value").unwrap();
    let value = Value::Int(512);
    for vector in [false, true] {
        let unbound = physical(vector);
        for count in [1, 16, 256] {
            let bound = bind(unbound.clone(), count, vector);
            for (mode, graph) in [("unbound", &unbound), ("bound", &bound)] {
                if vector {
                    assert_eq!(
                        graph
                            .vector_index_for(&label, &property)
                            .unwrap()
                            .dimension(),
                        16
                    );
                    group.bench_function(
                        BenchmarkId::new(format!("vector_access_{mode}"), count),
                        |b| {
                            b.iter(|| {
                                black_box(
                                    black_box(graph)
                                        .vector_index_for(black_box(&label), black_box(&property)),
                                )
                            });
                        },
                    );
                } else {
                    assert_eq!(
                        graph
                            .node_candidates_with_property_eq(&label, &property, &value)
                            .unwrap()
                            .unwrap()
                            .len(),
                        1
                    );
                    group.bench_function(
                        BenchmarkId::new(format!("property_access_{mode}"), count),
                        |b| {
                            b.iter(|| {
                                black_box(
                                    black_box(graph).property_index_for(
                                        black_box(&label),
                                        black_box(&property),
                                    ),
                                )
                            });
                        },
                    );
                    group.bench_function(
                        BenchmarkId::new(format!("candidate_probe_{mode}"), count),
                        |b| {
                            b.iter(|| {
                                black_box(
                                    black_box(graph)
                                        .node_candidates_with_property_eq(
                                            black_box(&label),
                                            black_box(&property),
                                            black_box(&value),
                                        )
                                        .unwrap(),
                                )
                            });
                        },
                    );
                }
            }
        }
    }
    group.finish();
}
