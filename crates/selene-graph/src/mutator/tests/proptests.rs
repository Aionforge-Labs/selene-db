use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use roaring::RoaringBitmap;

use super::*;

proptest! {
    #[test]
    fn create_delete_sequence_preserves_alive_count(ops in proptest::collection::vec(any::<bool>(), 1..64)) {
        let shared = SharedGraph::new(GraphId::new(1));
        let mut txn = shared.begin_write();
        let mut expected_alive = BTreeSet::new();
        let mut created = Vec::new();
        {
            let mut mutator = txn.mutator();
            for delete_previous in ops {
                let id = mutator
                    .create_node(LabelSet::new(), PropertyMap::new())
                    .expect("create_node ok");
                expected_alive.insert(id);
                created.push(id);
                if delete_previous
                    && let Some(to_delete) = created.first().copied()
                    && expected_alive.remove(&to_delete)
                {
                    mutator.delete_node(to_delete).unwrap();
                }
            }
            prop_assert_eq!(mutator.read().node_count(), expected_alive.len());
            prop_assert_eq!(
                mutator.read().meta.next_node_id,
                1,
                "working meta is updated at commit, allocator advances during mutation"
            );
        }
        let outcome = txn.commit().unwrap();
        prop_assert_eq!(shared.read().node_count(), expected_alive.len());
        prop_assert_eq!(outcome.next_node_id as usize, created.len() + 1);
    }
}

proptest! {
    #[test]
    fn label_index_matches_alive_labeled_nodes(
        ops in proptest::collection::vec((0_u8..3, 0_usize..4, 0_usize..64), 1..64)
    ) {
        let shared = SharedGraph::new(GraphId::new(1));
        let labels = [
            db_string("prop.label.a").unwrap(),
            db_string("prop.label.b").unwrap(),
            db_string("prop.label.c").unwrap(),
            db_string("prop.label.d").unwrap(),
        ];
        let mut txn = shared.begin_write();
        let mut alive: BTreeMap<NodeId, BTreeSet<DbString>> = BTreeMap::new();
        let mut created = Vec::new();
        {
            let mut mutator = txn.mutator();
            for (op, label_index, node_index) in ops {
                match op {
                    0 => {
                        let label = labels[label_index % labels.len()].clone();
                        let id = mutator
                            .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                            .expect("create_node ok");
                        created.push(id);
                        alive.insert(id, BTreeSet::from([label]));
                    }
                    1 if !alive.is_empty() => {
                        let id = *alive.keys().nth(node_index % alive.len()).unwrap();
                        let label = labels[label_index % labels.len()].clone();
                        let current = alive.get_mut(&id).unwrap();
                        let (added, removed) = if current.contains(&label) {
                            current.remove(&label);
                            (Vec::new(), vec![label])
                        } else {
                            current.insert(label.clone());
                            (vec![label], Vec::new())
                        };
                        mutator
                            .update_node(
                                id,
                                LabelDiff::new(added, removed).unwrap(),
                                PropertyDiff::new([], []).unwrap(),
                            )
                            .unwrap();
                    }
                    2 if !alive.is_empty() => {
                        let id = *alive.keys().nth(node_index % alive.len()).unwrap();
                        mutator.delete_node(id).unwrap();
                        alive.remove(&id);
                    }
                    _ => {}
                }
            }

            let mut expected: BTreeMap<DbString, RoaringBitmap> = BTreeMap::new();
            for (id, node_labels) in &alive {
                // BRIEF-Item-4c: rows are append-assigned, so resolve each id's row
                // through the authoritative map rather than `id - 1` arithmetic.
                let row = mutator.read().node_row_for_id(*id).unwrap().get();
                for label in node_labels {
                    expected.entry(label.clone()).or_default().insert(row);
                }
            }
            for label in labels {
                let expected_bitmap = expected.get(&label);
                let actual_bitmap = mutator.read().nodes_with_label(&label);
                prop_assert_eq!(actual_bitmap, expected_bitmap);
            }
            prop_assert_eq!(
                mutator.read().label_count(),
                expected.values().filter(|bitmap| !bitmap.is_empty()).count()
            );
        }
        let outcome = txn.commit().unwrap();
        prop_assert_eq!(outcome.next_node_id as usize, created.len() + 1);
    }
}
