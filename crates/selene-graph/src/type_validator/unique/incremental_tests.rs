use super::*;
use crate::{GraphTypeDef, NodeTypeDef, PropertyTypeDef, SharedGraph, ValidationMode};
use proptest::{prelude::*, strategy::ValueTree, test_runner::TestRunner};
use selene_catalog::{DeclarationMetadata, DeclarationState, PropertyTarget};
use selene_core::{
    GraphId, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyValueType, db_string,
};
use std::sync::Arc;

fn name(text: &str) -> DbString {
    db_string(text).unwrap()
}
fn property(key: &str, kind: PropertyValueType) -> PropertyTypeDef {
    PropertyTypeDef {
        name: name(key),
        value_type: kind,
        list_element_type: None,
        required: false,
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: None,
    }
}
fn graph(kind: PropertyValueType) -> SharedGraph {
    let mut graph = SeleneGraph::new(GraphId::new(101));
    graph.meta.bound_type = Some(Arc::new(GraphTypeDef {
        name: name("Shape"),
        node_types: vec![NodeTypeDef {
            name: name("Item"),
            key_labels: LabelSet::single(name("Item")),
            properties: vec![property("a", kind), property("b", PropertyValueType::Int)],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: vec![],
    }));
    SharedGraph::from_graph(graph)
}
fn rule(arity: usize, kind: ConstraintKind) -> ConstraintDeclaration {
    ConstraintDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Ready),
        target: PropertyTarget {
            element: ElementKind::Node,
            label: "Item".into(),
            properties: ["a", "b"][..arity].iter().map(|s| s.to_string()).collect(),
        },
        declaring_type: "Item".into(),
        kind,
        backing_index: None,
    }
}
fn props(a: i64, b: i64) -> PropertyMap {
    PropertyMap::from_pairs([(name("a"), Value::Int(a)), (name("b"), Value::Int(b))]).unwrap()
}

#[test]
fn independent_final_state_model_agrees_for_mixed_deltas_and_rollbacks() {
    // Independent oracle: ordinary integer tuples and pairwise equality. No
    // production tuple encoding, domain service or index is used by the model.
    for arity in [1, 2] {
        let shared = graph(PropertyValueType::Int);
        let mut index =
            ConstraintIndexes::build(&shared.read(), vec![rule(arity, ConstraintKind::Unique)])
                .unwrap();
        let mut model: Vec<(NodeId, [i64; 2])> = Vec::new();
        let operations = proptest::collection::vec((0..3u8, 0..9i64, 0..5i64, any::<usize>()), 600)
            .new_tree(&mut TestRunner::deterministic())
            .unwrap()
            .current();
        for round in 0..200 {
            let before = shared.read();
            let mut candidate = model.clone();
            let mut tx = shared.begin_write();
            for &(operation, a, b, selection) in &operations[round * 3..round * 3 + 3] {
                let values = [a, b];
                match operation {
                    0 if !candidate.is_empty() => {
                        let pos = selection % candidate.len();
                        let id = candidate[pos].0;
                        tx.mutator()
                            .update_node(
                                id,
                                LabelDiff::new([], []).unwrap(),
                                PropertyDiff::new(
                                    [
                                        (name("a"), Value::Int(values[0])),
                                        (name("b"), Value::Int(values[1])),
                                    ],
                                    [],
                                )
                                .unwrap(),
                            )
                            .unwrap();
                        candidate[pos].1 = values;
                    }
                    1 if !candidate.is_empty() => {
                        let pos = selection % candidate.len();
                        let id = candidate.remove(pos).0;
                        tx.mutator().delete_node(id).unwrap();
                    }
                    _ => {
                        let id = tx
                            .mutator()
                            .create_node(
                                LabelSet::single(name("Item")),
                                props(values[0], values[1]),
                            )
                            .unwrap();
                        candidate.push((id, values));
                    }
                }
            }
            let valid = candidate.iter().enumerate().all(|(i, (_, value))| {
                candidate[..i]
                    .iter()
                    .all(|(_, prior)| prior[..arity] != value[..arity])
            });
            let next = index.apply(&tx.changes, &before, tx.read());
            assert_eq!(next.is_ok(), valid, "arity={arity} round={round}");
            if valid && round % 7 != 0 {
                index = next.unwrap();
                model = candidate;
                tx.commit().unwrap();
            } else {
                if !valid {
                    assert!(matches!(
                        next,
                        Err(TypeViolation::UniquePropertyDuplicate { .. })
                    ));
                }
                tx.rollback();
            }
            assert_eq!(shared.read().node_count(), model.len());
        }
    }
}

#[test]
fn one_element_constraint_work_is_bounded_at_multiple_sizes_and_arities() {
    for size in [10, 10_000] {
        for arity in [1, 2] {
            let shared = graph(PropertyValueType::Int);
            let mut tx = shared.begin_write();
            for i in 0..size {
                tx.mutator()
                    .create_node(LabelSet::single(name("Item")), props(i, i))
                    .unwrap();
            }
            tx.commit().unwrap();
            let before = shared.read();
            let index =
                ConstraintIndexes::build(&before, vec![rule(arity, ConstraintKind::Key)]).unwrap();
            assert_eq!(index.visited, size as usize);
            let mut tx = shared.begin_write();
            tx.mutator()
                .update_node(
                    NodeId::new(1),
                    LabelDiff::new([], []).unwrap(),
                    PropertyDiff::new([(name("a"), Value::Int(size + 10))], []).unwrap(),
                )
                .unwrap();
            let next = index.apply(&tx.changes, &before, tx.read()).unwrap();
            assert_eq!(next.visited, 2, "only old and new affected entity");
            assert_eq!(next.indexes[0].entries.len(), size as usize);
            assert_eq!(
                index.indexes[0].entries.len(),
                size as usize,
                "old snapshot immutable"
            );
        }
    }
}

#[test]
fn tuple_components_are_typed_and_delimiter_safe_with_numeric_equivalence() {
    let make = |values: &[Value]| {
        values
            .iter()
            .map(|value| {
                let mut out = vec![];
                key::write(value, &mut out, 1).unwrap();
                out
            })
            .collect::<Vec<_>>()
    };
    assert_ne!(
        make(&[Value::String(name("a/b")), Value::String(name("c"))]),
        make(&[Value::String(name("a")), Value::String(name("b/c"))])
    );
    assert_ne!(
        make(&[Value::Int(1), Value::String(name("2"))]),
        make(&[Value::String(name("1")), Value::Int(2)])
    );
    assert_eq!(
        make(&[Value::Int(1), Value::Float(-0.0)]),
        make(&[Value::Decimal("1.00".parse().unwrap()), Value::Uint(0)])
    );
}

#[test]
fn deleting_last_domain_witness_allows_a_new_duration_family() {
    let mut domains = domain::Domains::default();
    let months = Value::Duration(Box::new("P1M".parse().unwrap()));
    let hours = Value::Duration(Box::new("PT1H".parse().unwrap()));
    let zero = Value::Duration(Box::new("PT0S".parse().unwrap()));
    domains.change(&[&zero], true).unwrap();
    domains.change(&[&months], true).unwrap();
    assert!(domains.clone().change(&[&hours], true).is_err());
    domains.change(&[&months], false).unwrap();
    domains.change(&[&hours], true).unwrap();
}

#[test]
fn compaction_rebuilds_native_constraint_backing_without_catalog_binding() {
    let mut snapshot = graph(PropertyValueType::Int).read().as_ref().clone();
    Arc::make_mut(snapshot.meta.bound_type.as_mut().unwrap()).node_types[0].properties[0].unique =
        true;
    let shared = SharedGraph::from_graph(snapshot);
    let mut tx = shared.begin_write();
    tx.mutator()
        .create_node(LabelSet::single(name("Item")), props(1, 1))
        .unwrap();
    tx.commit().unwrap();
    shared.compact().unwrap();
    let before = shared.read();
    let mut tx = shared.begin_write();
    tx.mutator()
        .create_node(LabelSet::single(name("Item")), props(1, 2))
        .unwrap();
    assert!(matches!(
        tx.commit(),
        Err(crate::GraphError::TypeViolation(
            TypeViolation::UniquePropertyDuplicate { .. }
        ))
    ));
    assert_eq!(before.node_count(), 1);
    assert_eq!(shared.read().node_count(), 1);
}

#[test]
fn native_writes_keep_named_uniqueness_when_instance_schema_is_relaxed() {
    let mut snapshot = graph(PropertyValueType::Int).read().as_ref().clone();
    let mut named = snapshot.meta.bound_type.as_deref().unwrap().clone();
    named.node_types[0].properties[0].unique = true;
    snapshot
        .admit_named_constraints(None, Arc::new(named), &[])
        .unwrap();
    let shared = SharedGraph::from_graph(snapshot);
    let mut tx = shared.begin_write();
    tx.mutator()
        .create_node(LabelSet::single(name("Item")), props(1, 1))
        .unwrap();
    tx.commit().unwrap();
    let mut tx = shared.begin_write();
    tx.mutator()
        .create_node(LabelSet::single(name("Item")), props(1, 2))
        .unwrap();
    assert!(matches!(
        tx.commit(),
        Err(crate::GraphError::TypeViolation(
            TypeViolation::UniquePropertyDuplicate { .. }
        ))
    ));
    assert_eq!(shared.read().node_count(), 1);
}
