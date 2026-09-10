//! Exact numeric key construction, retained lookup, and comparison fixtures.

use std::{cmp::Ordering, hint::black_box, mem::size_of};

use criterion::{BenchmarkId, Criterion, Throughput};
use selene_core::{NumericKey, Value};

fn value(n: i64, family: usize) -> Value {
    match family % 7 {
        0 => Value::Int(n),
        1 => Value::Uint(n as u64),
        2 => Value::Int128(i128::from(n)),
        3 => Value::Uint128(n as u128),
        4 => Value::Float(n as f64),
        5 => Value::Float32(n as f32),
        _ => Value::Decimal(rust_decimal::Decimal::new(n * 100, 2)),
    }
}

#[allow(clippy::print_stderr)]
pub(super) fn bench_numeric_keys(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_numeric_keys");
    for count in [1, 1_024] {
        let inputs: Vec<_> = (0..count).map(|i| value(i as i64 + 1, i)).collect();
        let keys: Vec<_> = inputs.iter().map(|v| NumericKey::of(v).unwrap()).collect();
        let peers: Vec<_> = (0..count).map(|i| value(i as i64 + 1, i + 1)).collect();
        for (key, peer) in keys.iter().zip(&peers) {
            assert_eq!(
                key.predicate_cmp(NumericKey::of(peer).unwrap()),
                Some(Ordering::Equal)
            );
        }
        eprintln!(
            "[core_numeric_keys] count={count} Value={}B NumericKey={}B retained_key_capacity={}B (layout, not allocator/RSS measurement)",
            size_of::<Value>(),
            size_of::<NumericKey>(),
            keys.capacity() * size_of::<NumericKey>(),
        );
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(BenchmarkId::new("construct", count), |b| {
            b.iter(|| {
                for value in black_box(&inputs) {
                    black_box(NumericKey::of(value).unwrap());
                }
            })
        });
        group.bench_function(BenchmarkId::new("lookup", count), |b| {
            b.iter(|| {
                for key in black_box(&keys) {
                    black_box(*key);
                }
            })
        });
        group.bench_function(BenchmarkId::new("compare_mixed", count), |b| {
            b.iter(|| {
                for (lhs, rhs) in black_box(&inputs).iter().zip(black_box(&peers)) {
                    black_box(
                        NumericKey::of(lhs)
                            .unwrap()
                            .predicate_cmp(NumericKey::of(rhs).unwrap()),
                    );
                }
            })
        });
    }
    group.finish();
}
