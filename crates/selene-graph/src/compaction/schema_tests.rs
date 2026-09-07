//! BRIEF-Item-4b compaction tests for index + schema-binding rebuild paths.
//!
//! Split from `compaction::tests` (the storage-layout/renumber suite) to keep
//! both files well under the 700-LOC cap. These exercise the parts of
//! `compact_core` that copy index *registrations* and `graph.meta` (the GG02
//! `bound_type`) and then rebuild row-keyed index state from the dense columns —
//! the most row-renumber-sensitive surfaces in 4b. A remap bug here points a
//! value at the wrong external id, resurrects a reclaimed key, or drops the
//! closed-graph binding.

use selene_core::{
    DbString, GraphId, LabelSet, NodeId, PropertyMap, PropertyValueType, Value, db_string,
};
use smallvec::{SmallVec, smallvec};

use super::compact_core;
use crate::store::NodeRow;
use crate::{
    EdgeEndpointDef, EdgeTypeDef, GraphTypeDef, NodeTypeDef, PropertyTypeDef, SeleneGraph,
    SharedGraph, TypedIndexKind, ValidationMode,
};

fn prop(key: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(key).unwrap(), value)]).unwrap()
}

/// A graph carrying a registered property index (`cmp.idx`.`name`, kind I64),
/// 3 indexed nodes (values 11/22/33), then a delete of the middle node — so the
/// post-compaction index rebuild must drop the reclaimed node's bucket and point
/// the survivors at their renumbered rows.
fn graph_with_indexed_deletion() -> (SharedGraph, DbString, DbString) {
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("cmp.idx").unwrap();
    let name = db_string("name").unwrap();
    shared
        .create_property_index(la.clone(), name.clone(), TypedIndexKind::I64)
        .unwrap();
    let mut txn = shared.begin_write();
    let n2 = {
        let mut m = txn.mutator();
        m.create_node(LabelSet::single(la.clone()), prop("name", Value::Int(11)))
            .unwrap();
        let n2 = m
            .create_node(LabelSet::single(la.clone()), prop("name", Value::Int(22)))
            .unwrap();
        m.create_node(LabelSet::single(la.clone()), prop("name", Value::Int(33)))
            .unwrap();
        m.delete_node(n2).unwrap();
        n2
    };
    txn.commit().unwrap();
    assert_eq!(n2, NodeId::new(2));
    (shared, la, name)
}

#[test]
fn compaction_rebuilds_property_index_dropping_reclaimed_rows() {
    // The single most row-renumber-sensitive part of 4b: the row-keyed property
    // index is dropped and rebuilt from the dense columns. A remap bug here would
    // point a value at the wrong external id, or resurrect a reclaimed value.
    let (shared, la, name) = graph_with_indexed_deletion();
    let before = shared.read();
    let compacted = compact_core(&before).unwrap();
    let g = &compacted.graph;

    // Registration survives the renumber.
    assert!(
        g.property_index_for(&la, &name).is_some(),
        "the property-index registration must survive compaction"
    );

    let resolve = |graph: &SeleneGraph, v: i64| -> Vec<NodeId> {
        graph
            .node_candidates_with_property_eq(&la, &name, &Value::Int(v))
            .unwrap()
            .map(|candidates| candidates.iter().collect())
            .unwrap_or_default()
    };

    // Survivors resolve to the RIGHT external ids via their renumbered rows.
    assert_eq!(resolve(g, 11), vec![NodeId::new(1)]);
    assert_eq!(resolve(g, 33), vec![NodeId::new(3)]);
    // The reclaimed node's indexed value is gone from the rebuilt index.
    assert!(
        resolve(g, 22).is_empty(),
        "reclaimed node 2's indexed value must not survive the rebuild"
    );
    // Sanity: the delete already dropped it pre-compaction too.
    assert!(resolve(&before, 22).is_empty());
}

/// A graph with a registered *composite* index (`cmp.cidx`.(`zone`,`rank`),
/// kinds String+I64), 3 indexed nodes, then a delete of the middle node — so
/// the post-compaction composite rebuild must drop the reclaimed key and point
/// survivors at their renumbered rows. The composite-registration-copy path in
/// `compact_core` is otherwise exercised by no test.
fn graph_with_composite_indexed_deletion() -> (SharedGraph, DbString, SmallVec<[DbString; 4]>) {
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("cmp.cidx").unwrap();
    let zone = db_string("zone").unwrap();
    let rank = db_string("rank").unwrap();
    let props: SmallVec<[DbString; 4]> = smallvec![zone.clone(), rank.clone()];
    let mk = |z: &str, r: i64| {
        PropertyMap::from_pairs([
            (zone.clone(), Value::String(db_string(z).unwrap())),
            (rank.clone(), Value::Int(r)),
        ])
        .unwrap()
    };
    let mut txn = shared.begin_write();
    let n2 = {
        let mut m = txn.mutator();
        m.create_composite_property_index_named(
            la.clone(),
            props.clone(),
            smallvec![TypedIndexKind::String, TypedIndexKind::I64],
            None,
        )
        .unwrap();
        m.create_node(LabelSet::single(la.clone()), mk("north", 1))
            .unwrap(); // id 1
        let n2 = m
            .create_node(LabelSet::single(la.clone()), mk("south", 2))
            .unwrap(); // id 2
        m.create_node(LabelSet::single(la.clone()), mk("north", 3))
            .unwrap(); // id 3
        m.delete_node(n2).unwrap();
        n2
    };
    txn.commit().unwrap();
    assert_eq!(n2, NodeId::new(2));
    (shared, la, props)
}

#[test]
fn compaction_rebuilds_composite_property_index() {
    let (shared, la, props) = graph_with_composite_indexed_deletion();
    let before = shared.read();
    let compacted = compact_core(&before).unwrap();
    let g = &compacted.graph;

    // Registration (declared properties) survives the renumber.
    let entry = g
        .composite_property_index_entry_for(&la, &props)
        .expect("composite registration must survive compaction");
    assert_eq!(entry.declared_properties.as_slice(), props.as_slice());

    let resolve = |graph: &SeleneGraph, z: &str, r: i64| -> Vec<NodeId> {
        let Some(entry) = graph.composite_property_index_entry_for(&la, &props) else {
            return Vec::new();
        };
        let zone_v = Value::String(db_string(z).unwrap());
        let rank_v = Value::Int(r);
        let refs: Vec<&Value> = vec![&zone_v, &rank_v];
        let Ok(key) = entry.index.key_from_values(&refs) else {
            return Vec::new();
        };
        entry
            .index
            .lookup_key(&key)
            .map(|bitmap| {
                bitmap
                    .iter()
                    .map(|row| graph.node_id_for_node_row(NodeRow::new(row)).unwrap())
                    .collect()
            })
            .unwrap_or_default()
    };

    // Survivors resolve to the RIGHT external ids via their renumbered rows.
    assert_eq!(resolve(g, "north", 1), vec![NodeId::new(1)]);
    assert_eq!(resolve(g, "north", 3), vec![NodeId::new(3)]);
    // The reclaimed node's composite key is gone from the rebuilt index.
    assert!(
        resolve(g, "south", 2).is_empty(),
        "reclaimed node 2's composite key must not survive the rebuild"
    );
}

/// Minimal GG02 closed graph: one `Person` node type (required `name:STRING`)
/// + one `KNOWS` Person→Person edge type, both `Strict`.
fn person_only_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("cmp.closed.graph").unwrap(),
        node_types: vec![NodeTypeDef {
            name: db_string("cmp.closed.person").unwrap(),
            key_labels: LabelSet::single(db_string("Person").unwrap()),
            properties: vec![PropertyTypeDef {
                name: db_string("name").unwrap(),
                value_type: PropertyValueType::String,
                list_element_type: None,
                required: true,
                default: None,
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: vec![EdgeTypeDef {
            name: db_string("cmp.closed.knows").unwrap(),
            label: db_string("KNOWS").unwrap(),
            source_node_type: EdgeEndpointDef::NodeType(0),
            target_node_type: EdgeEndpointDef::NodeType(0),
            properties: vec![],
            validation_mode: ValidationMode::Strict,
        }],
    }
}

#[test]
fn compaction_preserves_closed_graph_binding_and_revalidates() {
    // compact_core carries graph.meta (incl. the GG02 bound_type) through
    // verbatim. Assert the binding survives BOTH structurally and functionally:
    // the republished compacted graph still validates writes against the type.
    let shared = SharedGraph::builder(GraphId::new(77))
        .bound_to(person_only_graph_type())
        .unwrap()
        .build()
        .unwrap();
    let person = db_string("Person").unwrap();
    let knows = db_string("KNOWS").unwrap();
    let name = db_string("name").unwrap();
    let mk = |n: &str| {
        PropertyMap::from_pairs([(name.clone(), Value::String(db_string(n).unwrap()))]).unwrap()
    };

    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        let p1 = m
            .create_node(LabelSet::single(person.clone()), mk("ann"))
            .unwrap();
        let p2 = m
            .create_node(LabelSet::single(person.clone()), mk("bob"))
            .unwrap();
        let p3 = m
            .create_node(LabelSet::single(person.clone()), mk("cy"))
            .unwrap();
        m.create_edge(knows.clone(), p1, p3, PropertyMap::new())
            .unwrap(); // survives
        m.create_edge(knows, p1, p2, PropertyMap::new()).unwrap(); // cascade-deleted
        m.delete_node(p2).unwrap();
        assert_eq!(
            (p1, p2, p3),
            (NodeId::new(1), NodeId::new(2), NodeId::new(3))
        );
    }
    txn.commit().unwrap();

    let before = shared.read();
    let compacted = compact_core(&before).unwrap();

    // Structural: the GG02 binding carries through compaction unchanged.
    assert!(compacted.graph.meta.bound_type.is_some());
    assert_eq!(
        compacted
            .graph
            .meta
            .bound_type
            .as_ref()
            .map(|t| t.name.clone()),
        before.meta.bound_type.as_ref().map(|t| t.name.clone()),
    );

    // Functional: after republication the bound type still validates writes.
    let republished = SharedGraph::from_graph(compacted.graph);
    {
        let mut txn = republished.begin_write();
        {
            let mut m = txn.mutator();
            m.create_node(LabelSet::single(person.clone()), mk("dot"))
                .unwrap();
        }
        txn.commit().expect("a conforming insert must still commit");
    }
    {
        let mut txn = republished.begin_write();
        {
            let mut m = txn.mutator();
            // Missing the required `name` — must be rejected at commit.
            let _ = m.create_node(LabelSet::single(person), PropertyMap::new());
        }
        assert!(
            txn.commit().is_err(),
            "the bound type must still reject a non-conforming node after compaction"
        );
    }

    let g = republished.read();
    assert!(g.is_node_alive(NodeId::new(1)));
    assert!(g.is_node_alive(NodeId::new(3)));
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
}
