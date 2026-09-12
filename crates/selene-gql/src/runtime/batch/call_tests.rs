//! Native boundary regressions independent of the row executor.

use super::{BatchPolicy, fixtures::person_graph, query::execute_with_test_policy};
use crate::{
    runtime::{BindingTable, ExecutorError, TxContext},
    *,
};
use selene_core::{DbString, Value, db_string};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

struct Registry {
    metadata: ProcedureMetadata,
    epoch: AtomicU64,
    available: AtomicBool,
    calls: AtomicUsize,
    bad_output: bool,
}

impl Registry {
    fn new() -> Self {
        Self {
            metadata: ProcedureMetadata::new(
                ProcedureHandle::new(1),
                ProcedureSignature::new(vec![
                    ProcedureParameter::new(db_string("value").unwrap(), GqlType::Integer, false),
                    ProcedureParameter::new(db_string("offset").unwrap(), GqlType::Integer, false)
                        .with_default(ProcedureDefaultValue::Integer(10)),
                ]),
                ProcedureOutputSchema {
                    columns: vec![ProcedureOutputColumn::new(
                        db_string("out").unwrap(),
                        GqlType::Integer,
                    )],
                },
                ProcedureTier::Graph,
                ProcedureMutability::Read,
            ),
            epoch: AtomicU64::new(0),
            available: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            bad_output: false,
        }
    }
}

impl ProcedureRegistry for Registry {
    fn lookup(&self, _: &[DbString]) -> Option<ProcedureMetadata> {
        self.available
            .load(Ordering::SeqCst)
            .then(|| self.metadata.clone())
    }
    fn registry_version(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    fn execute(
        &self,
        _: ProcedureHandle,
        args: &[Value],
        ctx: &mut ProcedureContext<'_, '_>,
    ) -> Result<ProcedureResult, ProcedureError> {
        assert!(matches!(ctx, ProcedureContext::Graph(_)));
        self.calls.fetch_add(1, Ordering::SeqCst);
        let [Value::Int(value), Value::Int(offset)] = args else {
            panic!("unvalidated args")
        };
        let rows = if self.bad_output {
            vec![vec![Value::Bool(true)]]
        } else if value % 2 == 0 {
            Vec::new()
        } else {
            vec![vec![Value::Int(value + offset)]]
        };
        Ok(ProcedureResult { rows })
    }
}

fn planned(source: &str, registry: &Registry) -> ExecutionPlan {
    plan(
        &analyze(parse(source).unwrap(), registry, None).unwrap(),
        registry,
    )
    .unwrap()
}

fn execute(
    plan: &ExecutionPlan,
    registry: &Registry,
    rows: usize,
) -> Result<BindingTable, ExecutorError> {
    let graph = person_graph();
    let ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        registry,
        graph.index_providers(),
    );
    execute_with_test_policy(plan, &ctx, BatchPolicy::new(rows, 1 << 20).unwrap()).map(|prefix| {
        let prefix = prefix.expect("call must be batch routed");
        assert_eq!(prefix.suffix_from, plan.pipeline.len());
        prefix.table
    })
}

#[test]
fn optional_calls_invoke_once_per_input_and_defaults_do_not_leak_across_batches() {
    for size in [1, 2, 3, 7, 1024] {
        let registry = Registry::new();
        let plan = planned(
            "MATCH (p:Person) OPTIONAL CALL test.echo(p.age) YIELD out RETURN p.age AS age, out ORDER BY age",
            &registry,
        );
        let output = execute(&plan, &registry, size).unwrap();
        let expected: Vec<_> = (21..=28)
            .map(|age| {
                vec![
                    Value::Int(age),
                    if age % 2 == 0 {
                        Value::Null
                    } else {
                        Value::Int(age + 10)
                    },
                ]
            })
            .collect();
        assert_eq!(
            output
                .rows()
                .iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(registry.calls.load(Ordering::SeqCst), 8);
    }
}

#[test]
fn registry_epoch_signature_and_availability_fail_before_invocation_even_on_empty_input() {
    for mode in 0..4 {
        let mut registry = Registry::new();
        let plan = planned(
            "MATCH (p:Absent) CALL test.echo(1) YIELD out RETURN out",
            &registry,
        );
        match mode {
            0 => {
                registry.epoch.store(1, Ordering::SeqCst);
            }
            1 => {
                registry.available.store(false, Ordering::SeqCst);
            }
            2 => registry.metadata.signature.parameters[0].ty = GqlType::Boolean,
            _ => registry.metadata.mutability = ProcedureMutability::SchemaWrite,
        }
        assert!(matches!(
            execute(&plan, &registry, 2),
            Err(ExecutorError::Procedure { .. })
        ));
        assert_eq!(registry.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn boundary_rejects_dynamic_wrong_type_and_wrong_result_shape() {
    let mut registry = Registry::new();
    for source in [
        "CALL test.echo()",
        "CALL test.echo(1, 2, 3)",
        "CALL test.echo(TRUE)",
    ] {
        assert!(analyze(parse(source).unwrap(), &registry, None).is_err());
    }
    let plan = planned(
        "MATCH (p:Person) CALL test.echo(p.name) YIELD out RETURN out",
        &registry,
    );
    assert!(matches!(
        execute(&plan, &registry, 2),
        Err(ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { .. },
            ..
        })
    ));
    assert_eq!(registry.calls.load(Ordering::SeqCst), 0);
    registry.bad_output = true;
    let plan = planned("CALL test.echo(1) YIELD out", &registry);
    assert!(matches!(
        execute(&plan, &registry, 2),
        Err(ExecutorError::Procedure {
            source: ProcedureError::Internal { .. },
            ..
        })
    ));
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn calls_are_eager_before_limit_and_preserve_nonoptional_multiplicity() {
    let registry = Registry::new();
    let plan = planned(
        "MATCH (p:Person) CALL test.echo(p.age) YIELD out RETURN out LIMIT 1",
        &registry,
    );
    let table = execute(&plan, &registry, 2).unwrap();
    assert_eq!(table.rows().len(), 1);
    assert_eq!(registry.calls.load(Ordering::SeqCst), 8);
}

#[test]
fn native_batch_ids_after_delete_and_compaction_match_direct_reference_projection() {
    let graph = person_graph();
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session
        .execute_source(
            "CALL algo.projection_build('p', ['Person'], NULL, NULL)",
            &registry,
        )
        .unwrap();
    session
        .execute_source("MATCH (p:Person {name: 'Dan'}) DELETE p", &registry)
        .unwrap();
    session
        .execute_source("CALL selene.compact()", &registry)
        .unwrap();
    let source = "CALL algo.wcc('p') YIELD node_id, component_id";
    let analyzed = analyze(parse(source).unwrap(), &registry, None).unwrap();
    let plan = plan(&analyzed, &registry).unwrap();
    let ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &registry,
        graph.index_providers(),
    );
    let prefix = execute_with_test_policy(&plan, &ctx, BatchPolicy::new(2, 1 << 20).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(prefix.suffix_from, plan.pipeline.len());
    let projection = selene_algorithms::GraphProjection::build(
        ctx.snapshot(),
        &selene_algorithms::ProjectionConfig {
            name: "reference".into(),
            node_labels: vec![db_string("Person").unwrap()],
            edge_labels: vec![],
            weight_property: None,
        },
        None,
    )
    .unwrap();
    // Same WCC kernel; independent direct projection bypasses registry state.
    let expected: Vec<_> = selene_algorithms::wcc(&projection)
        .into_iter()
        .map(|(node, component)| vec![Value::NodeRef(node), Value::Uint(component)])
        .collect();
    assert_eq!(
        prefix
            .table
            .rows()
            .iter()
            .map(|r| r.values().to_vec())
            .collect::<Vec<_>>(),
        expected
    );
    let ids: Vec<_> = prefix
        .table
        .rows()
        .iter()
        .map(|r| match r.values()[0] {
            Value::NodeRef(id) => id.get(),
            _ => panic!("stable node reference required"),
        })
        .collect();
    assert_eq!(ids, [1, 2, 3, 5, 6, 7, 8]);
}
