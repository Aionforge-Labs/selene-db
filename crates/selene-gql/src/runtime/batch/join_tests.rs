//! F04-PR03 join kernels: paths, rules, and the relation oracle.
//!
//! Each test maps to one slice requirement:
//!
//! - repeated-key many-to-many joins preserve multiplicity (2x3 yields six,
//!   never deduplicated) on both execution paths;
//! - empty inputs and null join operands follow the selected join rules;
//! - hash and nested-loop paths agree exactly (rows, order, errors) while
//!   the nested loop stays the low-overhead choice for tiny inputs;
//! - mixed numerics, records, and references share one equality/hashing
//!   regime between the engine and the independent oracle;
//! - bounded budgets fail fanout with the typed resource error and no
//!   partial relation.
//!
//! Facade differentials against the row oracle live in
//! [`super::join_differentials`].

use selene_core::{DbString, GraphId, Value, db_string};
use selene_graph::SharedGraph;

use crate::{
    BuildSide,
    runtime::{Binding, ExecutorError},
};

use super::fixtures::{kernel_ctx, pair, pair_schema};
use super::join::{BatchHashJoin, hash_join_rows, nested_loop_join_rows};
use super::relation_model::{assert_same_multiset, key_self_valid, nested_join, values_equal};
use super::unit::BatchRowSource;
use super::{
    BatchBuffer, BatchCancel, BatchExecutionContext, BatchPolicy, MemoryBudget, OperatorState,
    PhysicalOperator, assert_same_rows,
};

/// Run both engine paths over one join input and require identical tables.
fn assert_paths_agree(
    build: &[Binding],
    probe: &[Binding],
    key: &[usize],
    build_is_left: bool,
    schema: &crate::plan::BindingTableSchema,
    what: &str,
) -> Vec<Vec<Value>> {
    let mut hash_ctx = kernel_ctx(MemoryBudget::unlimited());
    let (hash_rows, hash_reserved) =
        hash_join_rows(build, probe, key, build_is_left, schema, &mut hash_ctx)
            .unwrap_or_else(|err| panic!("{what}: hash path failed: {err:?}"));
    hash_ctx.budget_mut().release(hash_reserved);
    let mut loop_ctx = kernel_ctx(MemoryBudget::unlimited());
    let (loop_rows, loop_reserved) =
        nested_loop_join_rows(build, probe, key, build_is_left, schema, &mut loop_ctx)
            .unwrap_or_else(|err| panic!("{what}: nested-loop path failed: {err:?}"));
    loop_ctx.budget_mut().release(loop_reserved);
    assert_same_rows(&hash_rows, &loop_rows, what);
    hash_rows
}

#[test]
fn repeated_key_join_yields_six_bindings_both_paths() {
    // The headline multiplicity case: two build rows and three probe rows on
    // one repeated key produce six bindings, never deduplicated.
    let schema = pair_schema();
    let build = vec![
        pair(Value::Int(1), Value::Int(10)),
        pair(Value::Int(1), Value::Int(20)),
    ];
    let probe = vec![
        pair(Value::Int(1), Value::Int(100)),
        pair(Value::Int(1), Value::Int(200)),
        pair(Value::Int(1), Value::Int(300)),
    ];
    let rows = assert_paths_agree(&build, &probe, &[0], true, &schema, "2x3 join");
    assert_eq!(rows.len(), 6, "two-by-three fanout keeps every pair");
    // Probe-major with build insertion order: each probe pairs (10, 20).
    // Merge prefers the build (left) side, so probe values never surface.
    assert_same_rows(
        &rows,
        &[
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(1), Value::Int(20)],
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(1), Value::Int(20)],
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(1), Value::Int(20)],
        ],
        "2x3 join order",
    );
    // The independent oracle agrees as a multiset (its order rules are its
    // own; row-order identity is proven against the row oracle at facade).
    let oracle = nested_join(
        &[
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(1), Value::Int(20)],
        ],
        &[
            vec![Value::Int(1), Value::Int(100)],
            vec![Value::Int(1), Value::Int(200)],
            vec![Value::Int(1), Value::Int(300)],
        ],
        &[0],
        true,
        2,
    );
    assert_same_multiset(&oracle, &rows, "2x3 oracle");
    assert_eq!(oracle.len(), 6);
}

#[test]
fn null_keys_never_match_and_empty_sides_stay_empty() {
    let schema = pair_schema();
    // Null build keys and null probe keys never meet, per the selected
    // rule (null is the unbound sentinel, not a value).
    let build = vec![
        pair(Value::Null, Value::Int(1)),
        pair(Value::Int(7), Value::Int(2)),
    ];
    let probe = vec![
        pair(Value::Null, Value::Int(3)),
        pair(Value::Int(7), Value::Int(4)),
    ];
    let rows = assert_paths_agree(&build, &probe, &[0], true, &schema, "null keys");
    assert_same_rows(
        &rows,
        &[vec![Value::Int(7), Value::Int(2)]],
        "only the bound key joins",
    );
    // Empty build or probe sides join to empty on both paths.
    for (build, probe) in [
        (vec![], probe.clone()),
        (build.clone(), vec![]),
        (vec![], vec![]),
    ] {
        let rows = assert_paths_agree(&build, &probe, &[0], true, &schema, "empty side");
        assert!(rows.is_empty(), "empty side joins to empty");
    }
}

#[test]
fn mixed_numeric_record_and_reference_keys_share_one_regime() {
    use selene_core::{Record, VectorValue};
    let schema = pair_schema();
    let name = |s: &str| db_string(s).unwrap();
    let rec = |pairs: Vec<(DbString, Value)>| {
        Value::Record(Box::new(Record::Open(pairs.into_iter().collect())))
    };
    // One comparable family per join: the language rejects cross-family key
    // positions (see the error-parity test below), while compatible values
    // inside a family share one equality/hashing regime.
    let numeric_build = vec![pair(Value::Int(1), Value::Int(1))];
    let numeric_probe = vec![
        pair(Value::Float(1.0), Value::Int(10)),
        pair(Value::Uint(1), Value::Int(11)),
        pair(Value::Decimal("1".parse().unwrap()), Value::Int(12)),
        pair(Value::Int128(1), Value::Int(13)),
        pair(Value::Float(1.5), Value::Int(14)),
    ];
    let rows = assert_paths_agree(
        &numeric_build,
        &numeric_probe,
        &[0],
        true,
        &schema,
        "numeric keys",
    );
    assert_eq!(rows.len(), 4, "cross-type numerics collapse, 1.5 does not");
    let oracle = nested_join(
        &[vec![Value::Int(1), Value::Int(1)]],
        &[
            vec![Value::Float(1.0), Value::Int(10)],
            vec![Value::Uint(1), Value::Int(11)],
            vec![Value::Decimal("1".parse().unwrap()), Value::Int(12)],
            vec![Value::Int128(1), Value::Int(13)],
            vec![Value::Float(1.5), Value::Int(14)],
        ],
        &[0],
        true,
        2,
    );
    assert_same_multiset(&oracle, &rows, "numeric oracle");
    // Permuted records with cross-type numeric fields collapse; a
    // different field shape is a language-level domain error (asserted in
    // the error-parity test), never a silent non-match.
    let record_build = vec![pair(
        rec(vec![(name("a"), Value::Int(1)), (name("b"), Value::Int(2))]),
        Value::Int(1),
    )];
    let record_probe = vec![pair(
        rec(vec![
            (name("b"), Value::Float(2.0)),
            (name("a"), Value::Int(1)),
        ]),
        Value::Int(10),
    )];
    let rows = assert_paths_agree(
        &record_build,
        &record_probe,
        &[0],
        true,
        &schema,
        "record keys",
    );
    assert_eq!(rows.len(), 1, "only the same-shaped record joins");
    // References and strings match by identity and contents, one family
    // per join.
    let ref_build = vec![pair(Value::String(name("same")), Value::Int(1))];
    let ref_probe = vec![
        pair(Value::String(db_string("same").unwrap()), Value::Int(10)),
        pair(Value::String(name("other")), Value::Int(12)),
    ];
    let rows = assert_paths_agree(&ref_build, &ref_probe, &[0], true, &schema, "string keys");
    assert_eq!(rows.len(), 1);
    let rows = assert_paths_agree(
        &[pair(Value::Bool(true), Value::Int(1))],
        &[pair(Value::Bool(true), Value::Int(10))],
        &[0],
        true,
        &schema,
        "bool keys",
    );
    assert_eq!(rows.len(), 1);
    // Exact vectors match; near misses do not.
    let vector = || Value::Vector(VectorValue::new(vec![1.0, 2.0]).unwrap());
    let rows = assert_paths_agree(
        &[pair(vector(), Value::Int(1))],
        &[
            pair(vector(), Value::Int(10)),
            pair(
                Value::Vector(VectorValue::new(vec![1.0, 3.0]).unwrap()),
                Value::Int(11),
            ),
        ],
        &[0],
        true,
        &schema,
        "vector keys",
    );
    assert_eq!(rows.len(), 1);
    // Every joined pair agrees with the oracle's independent equality.
    for row in &rows {
        assert!(values_equal(&row[0], &vector()));
    }
}

#[test]
fn nan_and_null_composite_keys_never_match() {
    use selene_core::Record;
    use smallvec::smallvec;
    let schema = pair_schema();
    let nan_build = vec![pair(Value::Float(f64::NAN), Value::Int(1))];
    let nan_probe = vec![pair(Value::Float(f64::NAN), Value::Int(2))];
    for build_is_left in [true, false] {
        let rows = assert_paths_agree(
            &nan_build,
            &nan_probe,
            &[0],
            build_is_left,
            &schema,
            "nan keys",
        );
        assert!(rows.is_empty(), "NaN keys never join");
    }
    let key = db_string("x").unwrap();
    let null_rec = || {
        Value::Record(Box::new(Record::Open(smallvec![(
            key.clone(),
            Value::Null
        )])))
    };
    let rows = assert_paths_agree(
        &[pair(null_rec(), Value::Int(1))],
        &[pair(null_rec(), Value::Int(2))],
        &[0],
        true,
        &schema,
        "null-leaf record keys",
    );
    assert!(rows.is_empty(), "null-containing keys never join");
}

#[test]
fn oracle_parity_with_engine_keys_over_value_corpus() {
    use selene_core::{Record, VectorValue};
    use smallvec::smallvec;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use crate::runtime::pattern::key_values_equal;
    use crate::runtime::value_key::RuntimeEqKey;

    fn key_hash(key: &RuntimeEqKey) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    let name = |s: &str| db_string(s).unwrap();
    let corpus = vec![
        Value::Null,
        Value::Bool(true),
        Value::Int(-3),
        Value::Int(1),
        Value::Uint(1),
        Value::Float(1.0),
        Value::Float(-0.0),
        Value::Float32(1.0),
        Value::Int128(1),
        Value::Decimal("1".parse().unwrap()),
        Value::Decimal("0.5".parse().unwrap()),
        Value::Float(0.5),
        Value::String(name("same")),
        Value::List(vec![Value::Int(1), Value::Null]),
        Value::List(vec![Value::Int(1), Value::Int(2)]),
        Value::Record(Box::new(Record::Open(smallvec![
            (name("a"), Value::Int(1)),
            (name("b"), Value::Int(2)),
        ]))),
        Value::Record(Box::new(Record::Open(smallvec![
            (name("b"), Value::Float(2.0)),
            (name("a"), Value::Int(1)),
        ]))),
        Value::Vector(VectorValue::new(vec![1.0, 2.0]).unwrap()),
    ];
    // The oracle's equality agrees with the engine's hash-key equality on
    // every corpus pair, and equal values hash identically (the invariant
    // the hash join depends on).
    for lhs in &corpus {
        for rhs in &corpus {
            let oracle = values_equal(lhs, rhs);
            let engine = RuntimeEqKey::from_row(vec![lhs.clone()])
                == RuntimeEqKey::from_row(vec![rhs.clone()]);
            assert_eq!(
                oracle, engine,
                "oracle/engine equality diverged for {lhs:?} vs {rhs:?}"
            );
            if oracle {
                assert_eq!(
                    key_hash(&RuntimeEqKey::from_row(vec![lhs.clone()])),
                    key_hash(&RuntimeEqKey::from_row(vec![rhs.clone()])),
                    "equal keys hash differently for {lhs:?} vs {rhs:?}"
                );
            }
        }
    }
    // The oracle's join agrees with the engine join as a multiset over
    // duplicate-heavy comparable families (order identity is proven
    // separately against the row oracle). Each family joins separately:
    // the language rejects cross-family key positions (see the
    // error-parity test), so the oracle models families, not mixtures.
    let bindings = |rows: &[Vec<Value>]| {
        rows.iter()
            .map(|row| Binding::new(row.clone()))
            .collect::<Vec<_>>()
    };
    let rows_of = |values: &[Value]| {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| vec![value.clone(), Value::Int(index as i64)])
            .collect::<Vec<_>>()
    };
    for (name, build_values, probe_values) in [
        (
            "numerics",
            vec![Value::Int(1), Value::Float(1.0), Value::Int(1), Value::Null],
            vec![
                Value::Uint(1),
                Value::Null,
                Value::Decimal("1".parse().unwrap()),
            ],
        ),
        (
            "strings",
            vec![
                Value::String(name("same")),
                Value::String(name("same")),
                Value::String(name("other")),
            ],
            vec![Value::String(name("same")), Value::Null],
        ),
        (
            "int-lists",
            vec![
                Value::List(vec![Value::Int(1)]),
                Value::List(vec![Value::Int(1), Value::Int(2)]),
            ],
            vec![
                Value::List(vec![Value::Int(1)]),
                Value::List(vec![Value::Int(9)]),
            ],
        ),
    ] {
        let build = rows_of(&build_values);
        let probe = rows_of(&probe_values);
        let oracle = nested_join(&build, &probe, &[0], true, 2);
        let engine = assert_paths_agree(
            &bindings(&build),
            &bindings(&probe),
            &[0],
            true,
            &pair_schema(),
            name,
        );
        assert_same_multiset(&oracle, &engine, name);
    }
    // The engine's own probe self-check agrees with the oracle's validity
    // rule on the corpus: both admit exactly the matchable keys.
    for value in &corpus {
        let engine = key_values_equal(std::slice::from_ref(value), std::slice::from_ref(value))
            .expect("corpus keys stay comparable");
        assert_eq!(
            engine,
            key_self_valid(value),
            "validity diverged for {value:?}"
        );
    }
}

#[test]
fn bounded_budget_fails_join_fanout_without_partial_output() {
    let schema = pair_schema();
    let build = (0..100)
        .map(|index| pair(Value::Int(9), Value::Int(index)))
        .collect::<Vec<_>>();
    let probe = (0..100)
        .map(|index| pair(Value::Int(9), Value::Int(1_000 + index)))
        .collect::<Vec<_>>();
    // Ten thousand pairs over a byte-sized budget: both paths fail with the
    // typed resource error, never a truncated relation.
    for path in ["hash", "nested"] {
        let mut ctx = kernel_ctx(MemoryBudget::new(64));
        let outcome = if path == "hash" {
            hash_join_rows(&build, &probe, &[0], true, &schema, &mut ctx)
        } else {
            nested_loop_join_rows(&build, &probe, &[0], true, &schema, &mut ctx)
        };
        let err = outcome.expect_err("bounded fanout must fail");
        assert!(
            matches!(err, ExecutorError::ProgramLimitExceeded { .. }),
            "{path}: expected a typed resource error, got {err:?}"
        );
        assert_eq!(err.gqlstatus().as_str(), "5GQL1");
    }
    // The same fanout succeeds unbounded with the exact count.
    let rows = assert_paths_agree(&build, &probe, &[0], true, &schema, "100x100 fanout");
    assert_eq!(rows.len(), 10_000);
}

#[test]
fn join_operator_serves_slices_and_releases_budget() {
    let schema = pair_schema();
    let key = db_string("k").unwrap();
    let build_table = crate::runtime::BindingTable::new(
        schema.clone(),
        vec![
            pair(Value::Int(1), Value::Int(10)),
            pair(Value::Int(1), Value::Int(20)),
        ],
    );
    let probe_table = crate::runtime::BindingTable::new(
        schema.clone(),
        vec![
            pair(Value::Int(1), Value::Int(100)),
            pair(Value::Int(1), Value::Int(200)),
            pair(Value::Int(1), Value::Int(300)),
        ],
    );
    // Tiny batches force multi-pull serving of the six joined rows.
    let policy = BatchPolicy::new(2, 1 << 20).unwrap();
    let left = Box::new(BatchRowSource::new(build_table, policy));
    let right = Box::new(BatchRowSource::new(probe_table, policy));
    let mut join = BatchHashJoin::new(
        left,
        right,
        std::slice::from_ref(&key),
        BuildSide::Left,
        schema.clone(),
        policy,
    );
    assert_eq!(join.state(), OperatorState::Created);
    let graph = SharedGraph::new(GraphId::new(43_010));
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    );
    join.init(&mut ctx).expect("join inits");
    assert_eq!(join.state(), OperatorState::Open);
    let mut buffer = BatchBuffer::new();
    let mut rows = Vec::new();
    while let Some(batch) = join
        .next_batch(&mut ctx, &mut buffer)
        .expect("pulls succeed")
    {
        rows.extend(batch.logical_rows_vec());
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    assert_eq!(join.state(), OperatorState::Exhausted);
    assert_eq!(rows.len(), 6, "operator keeps every pair");
    assert!(join.batches_produced() > 1, "output splits across pulls");
    join.close(&mut ctx);
    assert_eq!(join.state(), OperatorState::Closed);
    assert_eq!(ctx.budget_used(), 0, "close releases the output claim");
    assert!(ctx.budget_peak() > 0, "peak records the join build");
}

#[test]
fn cross_family_keys_error_identically_both_paths() {
    // A record key against an integer build domain is a language-level data
    // exception on both paths, never a silent empty join.
    use selene_core::Record;
    use smallvec::smallvec;
    let schema = pair_schema();
    let key = db_string("x").unwrap();
    let record = Value::Record(Box::new(Record::Open(smallvec![(key, Value::Int(1))])));
    for build_is_left in [true, false] {
        for name in ["hash", "nested"] {
            let mut ctx = kernel_ctx(MemoryBudget::unlimited());
            let outcome = if name == "hash" {
                hash_join_rows(
                    &[pair(Value::Int(1), Value::Int(1))],
                    &[pair(record.clone(), Value::Int(2))],
                    &[0],
                    build_is_left,
                    &schema,
                    &mut ctx,
                )
            } else {
                nested_loop_join_rows(
                    &[pair(Value::Int(1), Value::Int(1))],
                    &[pair(record.clone(), Value::Int(2))],
                    &[0],
                    build_is_left,
                    &schema,
                    &mut ctx,
                )
            };
            let err = outcome.expect_err("cross-family keys must fail");
            assert!(
                matches!(
                    err,
                    ExecutorError::DataException {
                        subclass: crate::runtime::DataExceptionSubclass::ValuesNotComparable,
                        ..
                    }
                ),
                "{name}: expected values-not-comparable, got {err:?}"
            );
        }
    }
}

#[test]
fn incomparable_key_families_error_identically_both_paths() {
    // A build side of integers against a probe side of strings fails the
    // cross-input domain check with the same data exception on both paths.
    let schema = pair_schema();
    let build = vec![pair(Value::Int(1), Value::Int(1))];
    let probe = vec![pair(
        Value::String(db_string("one").unwrap()),
        Value::Int(2),
    )];
    let key = [0usize];
    for name in ["hash", "nested"] {
        let mut ctx = kernel_ctx(MemoryBudget::unlimited());
        let outcome = if name == "hash" {
            hash_join_rows(&build, &probe, &key, true, &schema, &mut ctx)
        } else {
            nested_loop_join_rows(&build, &probe, &key, true, &schema, &mut ctx)
        };
        let err = outcome.expect_err("incomparable keys must fail");
        assert!(
            matches!(
                err,
                ExecutorError::DataException {
                    subclass: crate::runtime::DataExceptionSubclass::ValuesNotComparable,
                    ..
                }
            ),
            "{name}: expected values-not-comparable, got {err:?}"
        );
    }
}
