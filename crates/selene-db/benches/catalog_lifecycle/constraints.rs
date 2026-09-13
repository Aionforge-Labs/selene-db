//! Absolute facade costs, with activation setup outside per-transaction timing.
use super::*;
use selene_db::{
    ConstraintDeclaration, ConstraintKind, DeclarationMetadata, DeclarationState, ElementKind,
    GraphTypeDefinition, NodeTypeDefinition, PathSegment, PropertyDefinition, PropertyTarget,
};

fn name(text: &str) -> PathSegment {
    PathSegment::regular(text).unwrap()
}
fn fixture(size: usize, arity: usize, activate: bool) -> (Database, ObjectPath) {
    let db = Database::builder().build();
    let path = graph("constraints", "data");
    let ty = graph("constraints", "shape");
    db.catalog()
        .create_schema(&schema("constraints"), CreatePolicy::Strict)
        .unwrap();
    let node = NodeTypeDefinition::new(name("Item"), vec![name("Item")])
        .unwrap()
        .with_property(PropertyDefinition::new(name("a"), Type::INT64).unwrap())
        .with_property(PropertyDefinition::new(name("b"), Type::INT64).unwrap());
    db.catalog()
        .create_graph_type(
            &ty,
            GraphTypeDefinition::builder()
                .with_node_type(node)
                .build()
                .unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    db.catalog()
        .create_graph(&path, Some(&ty), CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    let rows = (0..size)
        .map(|i| format!("(:Item {{a: {i}, b: {i}}})"))
        .collect::<Vec<_>>()
        .join(", ");
    session.execute(&format!("INSERT {rows}")).unwrap();
    // Query selection is index-backed; do not time a full MATCH scan as if it
    // were constraint-enforcement cost. This is still end-to-end facade latency.
    session.execute("CREATE INDEX by_b ON :Item(b)").unwrap();
    if activate {
        activation(&db, &path, arity);
    }
    (db, path)
}
fn activation(db: &Database, path: &ObjectPath, arity: usize) {
    black_box(
        db.catalog()
            .create_constraint(
                path,
                &name("tuple"),
                ConstraintDeclaration {
                    metadata: DeclarationMetadata::new(DeclarationState::Inactive),
                    target: PropertyTarget {
                        element: ElementKind::Node,
                        label: "Item".into(),
                        properties: ["a", "b"][..arity].iter().map(|p| p.to_string()).collect(),
                    },
                    declaring_type: "Item".into(),
                    kind: ConstraintKind::Key,
                    backing_index: None,
                },
            )
            .unwrap(),
    );
}

pub(super) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("composite_constraints");
    for size in [100, 1_000] {
        for arity in [1, 2] {
            group.bench_function(format!("activation/{size}/{arity}"), |b| {
                b.iter_batched(
                    || fixture(size, arity, false),
                    |(db, path)| {
                        activation(&db, &path, arity);
                        black_box((db, path))
                    },
                    BatchSize::PerIteration,
                );
            });
            let (db, path) = fixture(size, arity, true);
            let session = db.session(&path).unwrap();
            group.bench_function(format!("one_update/{size}/{arity}"), |b| {
                b.iter(|| {
                    black_box(
                        session
                            .execute("MATCH (n:Item) WHERE n.b = 0 SET n.a = n.a - 1")
                            .unwrap(),
                    )
                });
            });
            for rollback in [false, true] {
                group.bench_function(
                    format!(
                        "mixed_{}/{size}/{arity}",
                        if rollback { "rollback" } else { "commit" }
                    ),
                    |b| {
                        b.iter(|| {
                            session.execute("START TRANSACTION").unwrap();
                            session
                                .execute("MATCH (n:Item) WHERE n.b = 1 DELETE n")
                                .unwrap();
                            session.execute("INSERT (:Item {a: 1, b: 1})").unwrap();
                            session
                                .execute("MATCH (n:Item) WHERE n.b = 0 SET n.a = n.a - 1")
                                .unwrap();
                            black_box(
                                session
                                    .execute(if rollback { "ROLLBACK" } else { "COMMIT" })
                                    .unwrap(),
                            )
                        });
                    },
                );
            }
        }
    }
    group.finish();
}
