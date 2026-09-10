use criterion::{BenchmarkId, Criterion};
use selene_core::{SchemaPropertyIndexKind, SchemaVectorIndexKind};
use selene_db::{
    CreatePolicy, DeclarationDefinition, DeclarationMetadata, DeclarationState, DropPolicy,
    ElementKind, IndexConfiguration, IndexDeclaration, NativeBinding, NativeDeclaration,
    NativeProjection, PathSegment, PropertyTarget,
};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

pub(super) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("catalog_declaration/outer_publication");
    for (graphs, declarations) in [(100, 4), (1_000, 16)] {
        let database = super::graph_fixture(graphs);
        let catalog = database.catalog();
        for graph in 0..graphs {
            let owner = super::graph("graphs", format!("graph_{graph:05}"));
            for index in 0..declarations {
                let configuration = match index % 3 {
                    0 => IndexConfiguration::Property(vec![SchemaPropertyIndexKind::I64]),
                    1 => IndexConfiguration::Vector {
                        kind: SchemaVectorIndexKind::Flat,
                        dimension: 384,
                        hnsw: None,
                        ivf: None,
                    },
                    _ => IndexConfiguration::Text,
                };
                catalog
                    .declare(
                        &owner,
                        &PathSegment::regular(format!("index_{index}")).unwrap(),
                        DeclarationDefinition::Index(IndexDeclaration {
                            metadata: DeclarationMetadata::new(DeclarationState::Inactive),
                            target: PropertyTarget {
                                element: ElementKind::Node,
                                label: "Document".into(),
                                properties: vec![format!("property_{index}")],
                            },
                            configuration,
                        }),
                        CreatePolicy::Strict,
                    )
                    .unwrap();
            }
        }
        let owner = super::graph("graphs", "graph_00000");
        let name = PathSegment::regular("timed_projection").unwrap();
        let definition = DeclarationDefinition::Native(NativeDeclaration {
            metadata: DeclarationMetadata::new(DeclarationState::Inactive),
            binding: NativeBinding::Projection(NativeProjection {
                node_labels: vec!["Document".into()],
                edge_labels: vec![],
                weight_property: None,
            }),
        });
        group.bench_function(
            BenchmarkId::from_parameter(format!("{graphs}x{declarations}")),
            |b| {
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let input = definition.clone();
                        let start = Instant::now();
                        black_box(
                            catalog
                                .declare(&owner, &name, input, CreatePolicy::Strict)
                                .unwrap(),
                        );
                        elapsed += start.elapsed();
                        catalog
                            .drop_declaration(&owner, &name, DropPolicy::Strict)
                            .unwrap();
                    }
                    elapsed
                });
            },
        );
    }
    group.finish();
}
