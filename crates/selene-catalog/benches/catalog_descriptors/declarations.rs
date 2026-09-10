use criterion::{BenchmarkId, Criterion};
use selene_catalog::{
    CatalogDescriptor, CatalogObjectId, CatalogParent, CatalogTransaction, ConstraintDeclaration,
    ConstraintId, ConstraintKind, CreationMetadata, DeclarationMetadata, DeclarationState,
    ElementKind, GraphId, IndexConfiguration, IndexDeclaration, IndexId, PropertyTarget,
};
use selene_core::{SchemaPropertyIndexKind, SchemaVectorIndexKind};
use std::hint::black_box;

pub(super) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("catalog_declaration");
    for graphs in [100, 1_000] {
        for declarations in [4, 16] {
            let base = super::snapshot(graphs);
            let mut transaction = CatalogTransaction::new(&base).unwrap();
            let generation = transaction.generation();
            for graph in 1..=graphs {
                for offset in 0..declarations {
                    let raw = ((graph - 1) * declarations + offset + 1) as u64;
                    let owner = CatalogParent::Graph(GraphId::new(graph as u64).unwrap());
                    let target = PropertyTarget {
                        element: ElementKind::Node,
                        label: "Document".into(),
                        properties: vec![format!("property_{offset}")],
                    };
                    let metadata = DeclarationMetadata::new(DeclarationState::Inactive);
                    let name = super::name(format!("declaration_{offset}"));
                    let creation = CreationMetadata::new(generation, None);
                    let descriptor = if offset % 4 == 3 {
                        CatalogDescriptor::constraint(
                            ConstraintId::new(raw).unwrap(),
                            name,
                            owner,
                            generation,
                            creation,
                            ConstraintDeclaration {
                                metadata,
                                target,
                                declaring_type: "Document".into(),
                                kind: ConstraintKind::Unique,
                                backing_index: None,
                            },
                        )
                        .unwrap()
                    } else {
                        let configuration = match offset % 4 {
                            0 => IndexConfiguration::Property(vec![SchemaPropertyIndexKind::I64]),
                            1 => IndexConfiguration::Vector {
                                kind: SchemaVectorIndexKind::Flat,
                                dimension: 384,
                                hnsw: None,
                                ivf: None,
                            },
                            _ => IndexConfiguration::Text,
                        };
                        CatalogDescriptor::index(
                            IndexId::new(raw).unwrap(),
                            name,
                            owner,
                            generation,
                            creation,
                            IndexDeclaration {
                                metadata,
                                target,
                                configuration,
                            },
                        )
                        .unwrap()
                    };
                    transaction.insert(descriptor).unwrap();
                }
            }
            let snapshot = transaction.build().unwrap();
            let scale = format!("{graphs}x{declarations}");
            let owner = CatalogObjectId::Graph(GraphId::new((graphs / 2) as u64).unwrap());
            let query = super::name("declaration_0");
            group.bench_with_input(
                BenchmarkId::new("lookup", &scale),
                &snapshot,
                |b, snapshot| {
                    b.iter(|| {
                        black_box(
                            snapshot
                                .declaration(black_box(owner), black_box(&query))
                                .unwrap(),
                        )
                    });
                },
            );
            group.bench_with_input(
                BenchmarkId::new("clone_arc", &scale),
                &snapshot,
                |b, snapshot| {
                    b.iter(|| black_box(snapshot.clone()));
                },
            );
            group.bench_with_input(
                BenchmarkId::new("draft_build", &scale),
                &snapshot,
                |b, snapshot| {
                    // Full metadata clone, whole-graph validation, construction and destruction
                    // of the replacement catalog are timed. Fixture construction is not.
                    b.iter(|| {
                        black_box(
                            CatalogTransaction::new(black_box(snapshot))
                                .unwrap()
                                .build()
                                .unwrap(),
                        )
                    });
                },
            );
        }
    }
    group.finish();
}
