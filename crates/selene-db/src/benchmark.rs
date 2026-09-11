//! Scripted measurement seam only. No database/session/writer escapes this harness.

use crate::database::DatabaseInner;
use crate::*;
use selene_core::logical::Limits;
use selene_graph::logical_transaction::ReplayState;
use selene_persist::{
    StoreDirectory,
    control::{CompatibilityIdentity, EmptyStoreControl},
    logical_stream::{LogicalReader, LogicalWal},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Measured acknowledgments and exact WAL byte growth for a scripted workload.
pub struct CommitMeasurements {
    /// One duration per successful facade acknowledgment, including request staging.
    pub latency: Vec<Duration>,
    /// Complete log length, including setup transactions.
    pub bytes: u64,
}

/// Run bounded single-transaction inserts through private durable construction.
/// Setup and semantic real-file replay assertions are outside each timed request.
/// This is not a general durable creation API and yields no database handle.
pub fn measure_commits(
    dir: &StoreDirectory,
    named: bool,
    initial: usize,
    samples: usize,
) -> CommitMeasurements {
    assert!(initial <= 1024 && (1..=256).contains(&samples));
    let identity =
        CompatibilityIdentity::new("commit-benchmark", 1, [9; 32], [17, 0, 0], "binary", 1)
            .unwrap();
    let control = EmptyStoreControl::create_empty(dir, identity.clone()).unwrap();
    let database = Database {
        inner: Arc::new(
            DatabaseInner::with_wal(
                DatabaseConfig::default(),
                LogicalWal::create(control).unwrap(),
            )
            .unwrap(),
        ),
    };
    let mut replay =
        ReplayState::seed(database.catalog().snapshot().logical_catalog().unwrap()).unwrap();
    let schema = SchemaPath::regular("selene", "bench").unwrap();
    let graph = ObjectPath::regular("selene", "bench", "data").unwrap();
    let ty = ObjectPath::regular("selene", "bench", "Shape").unwrap();
    database
        .catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    if named {
        let definition = GraphTypeDefinition::builder()
            .with_node_type(
                NodeTypeDefinition::new(
                    PathSegment::regular("Base").unwrap(),
                    vec![PathSegment::regular("Base").unwrap()],
                )
                .unwrap(),
            )
            .build()
            .unwrap();
        database
            .catalog()
            .create_graph_type(&ty, definition, CreatePolicy::Strict)
            .unwrap();
    }
    database
        .catalog()
        .create_graph(&graph, named.then_some(&ty), CreatePolicy::Strict)
        .unwrap();
    let session = database.session(&graph).unwrap();
    if initial != 0 {
        let query = format!("INSERT {}", vec!["(:Base)"; initial].join(", "));
        session.execute(&query).unwrap();
    }
    let mut latency = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        session.execute("INSERT (:Base)").unwrap();
        latency.push(start.elapsed());
    }
    assert_eq!(
        session.execute("MATCH (n) RETURN n").unwrap().row_count(),
        Some(initial + samples)
    );
    drop(session);
    drop(database);
    let mut reader = LogicalReader::open(dir, &identity, Limits::default().bytes).unwrap();
    while let Some(body) = reader.next_body().unwrap() {
        replay = replay.apply_body(&body, Limits::default()).unwrap();
    }
    assert!(!reader.incomplete_tail());
    assert_eq!(
        replay
            .graph_summary(selene_core::GraphId::new(1))
            .unwrap()
            .0,
        initial + samples
    );
    let bytes = dir
        .open_read("WAL-00000000000000000001.logical")
        .unwrap()
        .metadata()
        .unwrap()
        .len();
    CommitMeasurements { latency, bytes }
}
