//! Selective/nonselective scalar JSON paths, maintenance, and complete rebuild.
use super::*;
use selene_db::{PathSegment, ScalarIndexKind};

fn fixture(indexed: bool) -> (Database, ObjectPath, selene_db::Session) {
    let db = Database::builder().build();
    let path = graph("expressions", "data");
    db.catalog()
        .create_schema(&schema("expressions"), CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    let input = (0..1_000)
        .map(|i| {
            let key = if i == 0 { "rare" } else { "common" };
            format!("(:Doc {{id: {i}, body: CAST('{{\"key\":\"{key}\"}}' AS JSON)}})")
        })
        .collect::<Vec<_>>()
        .join(",");
    session.execute(&format!("INSERT {input}")).unwrap();
    if indexed {
        create(&db, &path);
    }
    (db, path, session)
}

fn create(db: &Database, path: &ObjectPath) {
    db.catalog()
        .create_expression_index(
            path,
            &PathSegment::regular("key").unwrap(),
            "Doc",
            "json_get_path_scalar(n.body, 'key')",
            ScalarIndexKind::String,
        )
        .unwrap();
}

pub(super) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("scalar_expression");
    for indexed in [false, true] {
        let (_db, _path, session) = fixture(indexed);
        for (name, key, expected) in [("selective", "rare", 1), ("nonselective", "common", 999)] {
            let query = format!(
                "MATCH (n:Doc) WHERE json_get_path_scalar(n.body, 'key') = '{key}' RETURN count(*)"
            );
            let ExecutionOutcome::Rows { result, .. } = session.execute(&query).unwrap() else {
                panic!("rows")
            };
            assert_eq!(result.rows()[0].values(), &[Value::Int(expected)]);
            group.bench_function(format!("{name}/{indexed}/1000"), |b| {
                b.iter(|| black_box(session.execute(black_box(&query)).unwrap()))
            });
        }
        let mut toggle = false;
        group.bench_function(format!("maintenance/{indexed}/1000"), |b| b.iter(|| {
            toggle = !toggle;
            let key = if toggle { "rare" } else { "changed" };
            session.execute(&format!("MATCH (n:Doc) WHERE n.id = 0 SET n.body = CAST('{{\"key\":\"{key}\"}}' AS JSON)")).unwrap()
        }));
    }
    group.bench_function("rebuild/1000", |b| {
        b.iter_batched(
            || fixture(false),
            |(db, path, _session)| {
                create(&db, &path);
                black_box(db);
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
}
