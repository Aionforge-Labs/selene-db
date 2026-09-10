//! Optional procedure-argument integration tests.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use selene_core::{DbString, GraphId, Value};
use selene_gql::{
    AnalysisError, BindingTableSchema, GqlStatus, GqlType, PipelineOp, ProcedureContext,
    ProcedureDefaultValue, ProcedureError, ProcedureHandle, ProcedureMetadata, ProcedureMutability,
    ProcedureOutputSchema, ProcedureParameter, ProcedureRegistry, ProcedureResult,
    ProcedureSignature, ProcedureTier, Session, Statement, analyze, parse, plan,
};
use selene_graph::SharedGraph;

#[derive(Debug)]
struct RecordingRegistry {
    metadata: HashMap<Box<[DbString]>, ProcedureMetadata>,
    calls: Mutex<Vec<Vec<Value>>>,
}

impl RecordingRegistry {
    fn new(parameters: Vec<ProcedureParameter>) -> Self {
        let mut metadata = HashMap::new();
        metadata.insert(
            vec![db_string("test"), db_string("optional")].into_boxed_slice(),
            ProcedureMetadata::new(
                ProcedureHandle::new(1),
                ProcedureSignature::new(parameters),
                ProcedureOutputSchema {
                    columns: Vec::new(),
                },
                ProcedureTier::Graph,
                ProcedureMutability::Read,
            ),
        );
        Self {
            metadata,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> MutexGuard<'_, Vec<Vec<Value>>> {
        self.calls.lock().expect("calls mutex")
    }
}

impl ProcedureRegistry for RecordingRegistry {
    fn lookup(&self, name: &[DbString]) -> Option<ProcedureMetadata> {
        self.metadata.get(name).cloned()
    }

    fn execute(
        &self,
        _handle: ProcedureHandle,
        args: &[Value],
        _ctx: &mut ProcedureContext<'_, '_>,
    ) -> Result<ProcedureResult, ProcedureError> {
        self.calls().push(args.to_vec());
        Ok(ProcedureResult {
            rows: vec![Vec::new()],
        })
    }
}

fn optional_registry() -> RecordingRegistry {
    RecordingRegistry::new(vec![
        ProcedureParameter::new(db_string("id"), GqlType::Integer, false),
        ProcedureParameter::new(db_string("enabled"), GqlType::Boolean, false)
            .with_default(ProcedureDefaultValue::Boolean(false)),
    ])
}

fn exact_registry() -> RecordingRegistry {
    RecordingRegistry::new(vec![
        ProcedureParameter::new(db_string("id"), GqlType::Integer, false),
        ProcedureParameter::new(db_string("enabled"), GqlType::Boolean, false),
    ])
}

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test strings fit DB string cap")
}

#[test]
fn min_arity_materializes_default_before_runtime_handler() {
    let registry = optional_registry();
    let graph = SharedGraph::new(GraphId::new(13_000));
    let mut session = Session::new(&graph);

    session
        .execute_source("CALL test.optional(7)", &registry)
        .expect("defaulted call executes");

    let calls = registry.calls();
    assert_eq!(calls.as_slice(), [vec![Value::Int(7), Value::Bool(false)]]);
}

#[test]
fn max_arity_preserves_explicit_argument() {
    let registry = optional_registry();
    let graph = SharedGraph::new(GraphId::new(13_001));
    let mut session = Session::new(&graph);

    session
        .execute_source("CALL test.optional(7, TRUE)", &registry)
        .expect("explicit call executes");

    let calls = registry.calls();
    assert_eq!(calls.as_slice(), [vec![Value::Int(7), Value::Bool(true)]]);
}

#[test]
fn optional_arity_errors_keep_22g03_and_range_message() {
    let registry = optional_registry();
    let statement = parse("CALL test.optional()").expect("test source parses");

    let err = analyze(statement, &registry, None).expect_err("too few args");

    assert_eq!(err.gqlstatus(), GqlStatus::DATATYPE_MISMATCH);
    assert!(err.to_string().contains("expected 1..=2"));
    assert!(matches!(
        err,
        AnalysisError::WrongArgumentCount {
            minimum: 1,
            expected: 2,
            actual: 0,
            ..
        }
    ));

    let statement = parse("CALL test.optional(1, TRUE, FALSE)").expect("test source parses");
    let err = analyze(statement, &registry, None).expect_err("too many args");
    assert!(matches!(
        err,
        AnalysisError::WrongArgumentCount {
            minimum: 1,
            expected: 2,
            actual: 3,
            ..
        }
    ));
}

#[test]
fn exact_arity_procedure_still_rejects_wrong_count() {
    let registry = exact_registry();
    let statement = parse("CALL test.optional(1)").expect("test source parses");

    let err = analyze(statement, &registry, None).expect_err("exact arity still enforced");

    assert_eq!(err.gqlstatus(), GqlStatus::DATATYPE_MISMATCH);
    assert!(err.to_string().contains("expected 2"));
    assert!(matches!(
        err,
        AnalysisError::WrongArgumentCount {
            minimum: 2,
            expected: 2,
            actual: 1,
            ..
        }
    ));
}

#[test]
fn analyzer_and_planner_both_see_defaulted_argument() {
    let registry = optional_registry();
    let statement = parse("CALL test.optional(7)").expect("test source parses");
    let analyzed = analyze(statement, &registry, None).expect("test source analyzes");

    match analyzed.source() {
        Statement::Call(call) => assert_eq!(call.args.len(), 1),
        other => panic!("expected analyzed CALL, got {other:?}"),
    }
    assert_eq!(analyzed.calls[0].defaults().len(), 1);

    let plan = plan(&analyzed, &registry).expect("test source plans");
    assert_eq!(plan.output_schema, BindingTableSchema { columns: vec![] });
    let [PipelineOp::Call(call)] = plan.pipeline.as_slice() else {
        panic!("expected single planned CALL op");
    };
    assert_eq!(call.args.len(), 2);
}
