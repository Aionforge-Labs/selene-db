use std::sync::Arc;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use selene_core::{DbString, LabelSet, NodeId, PropertyMap, Value};

use super::TextIndex;
use crate::error::GraphResult;
use crate::graph::{SeleneGraph, TextIndexEntry};

type TextIndexMap = FxHashMap<(DbString, DbString), TextIndexEntry>;
type CandidateLabels<'a> = SmallVec<[&'a DbString; 4]>;
type CandidateProperties<'a> = SmallVec<[&'a DbString; 4]>;

pub(crate) fn apply_node_create(
    indexes: &mut TextIndexMap,
    labels: &LabelSet,
    props: &PropertyMap,
    row: u32,
    node_id: NodeId,
) {
    for label in labels.iter() {
        for (property, value) in props.iter() {
            insert_commit(
                indexes,
                label.clone(),
                property.clone(),
                value,
                row,
                node_id,
            );
        }
    }
}

pub(crate) fn apply_node_delete(
    indexes: &mut TextIndexMap,
    labels: &LabelSet,
    props: &PropertyMap,
    row: u32,
    node_id: NodeId,
) {
    for label in labels.iter() {
        for (property, value) in props.iter() {
            remove_commit(
                indexes,
                label.clone(),
                property.clone(),
                value,
                row,
                node_id,
            );
        }
    }
}

pub(crate) fn apply_node_update(
    indexes: &mut TextIndexMap,
    old_labels: &LabelSet,
    old_props: &PropertyMap,
    new_labels: &LabelSet,
    new_props: &PropertyMap,
    row: u32,
    node_id: NodeId,
) {
    if indexes.is_empty() {
        return;
    }
    let labels = candidate_labels(old_labels, new_labels);
    let properties = candidate_properties(old_props, new_props);
    for label in labels {
        for property in &properties {
            let key = (label.clone(), (*property).clone());
            let Some(entry) = indexes.get_mut(&key) else {
                continue;
            };
            match (
                indexable_text(old_labels, old_props, label, property),
                indexable_text(new_labels, new_props, label, property),
            ) {
                (Some(old_text), Some(new_text)) if old_text == new_text => {}
                (Some(_), Some(new_text)) => {
                    Arc::make_mut(&mut entry.index).insert_document(row, node_id, new_text);
                }
                (Some(_), None) => {
                    Arc::make_mut(&mut entry.index).remove_document(row, node_id);
                }
                (None, Some(new_text)) => {
                    Arc::make_mut(&mut entry.index).insert_document(row, node_id, new_text);
                }
                (None, None) => {}
            }
        }
    }
}

pub(crate) fn rebuild_text_indexes(graph: &mut SeleneGraph) -> GraphResult<()> {
    let registrations: Vec<((DbString, DbString), Option<DbString>)> = graph
        .text_index
        .iter()
        .map(|(key, entry)| (key.clone(), entry.name.clone()))
        .collect();
    let mut rebuilt = TextIndexMap::default();
    for ((label, property), name) in registrations {
        let index = TextIndex::build(graph, label.clone(), property.clone())?;
        rebuilt.insert((label, property), TextIndexEntry::new(index, name));
    }
    graph.text_index = rebuilt;
    Ok(())
}

fn indexable_text<'a>(
    labels: &LabelSet,
    props: &'a PropertyMap,
    label: &DbString,
    property: &DbString,
) -> Option<&'a str> {
    if !labels.contains(label) {
        return None;
    }
    match props.get(property) {
        Some(Value::String(text)) => Some(text.as_str()),
        _ => None,
    }
}

fn candidate_labels<'a>(old_labels: &'a LabelSet, new_labels: &'a LabelSet) -> CandidateLabels<'a> {
    let mut labels = CandidateLabels::new();
    for label in old_labels.iter() {
        push_unique(&mut labels, label);
    }
    for label in new_labels.iter() {
        push_unique(&mut labels, label);
    }
    labels
}

fn candidate_properties<'a>(
    old_props: &'a PropertyMap,
    new_props: &'a PropertyMap,
) -> CandidateProperties<'a> {
    let mut properties = CandidateProperties::new();
    for property in old_props.keys() {
        push_unique(&mut properties, property);
    }
    for property in new_props.keys() {
        push_unique(&mut properties, property);
    }
    properties
}

fn push_unique<'a>(values: &mut SmallVec<[&'a DbString; 4]>, value: &'a DbString) {
    if values.iter().all(|existing| *existing != value) {
        values.push(value);
    }
}

fn insert_commit(
    indexes: &mut TextIndexMap,
    label: DbString,
    property: DbString,
    value: impl TextValue,
    row: u32,
    node_id: NodeId,
) {
    let Some(text) = value.text() else {
        return;
    };
    if let Some(entry) = indexes.get_mut(&(label, property)) {
        Arc::make_mut(&mut entry.index).insert_document(row, node_id, text);
    }
}

fn remove_commit(
    indexes: &mut TextIndexMap,
    label: DbString,
    property: DbString,
    value: impl TextValue,
    row: u32,
    node_id: NodeId,
) {
    if value.text().is_none() {
        return;
    }
    if let Some(entry) = indexes.get_mut(&(label, property)) {
        Arc::make_mut(&mut entry.index).remove_document(row, node_id);
    }
}

trait TextValue {
    fn text(&self) -> Option<&str>;
}

impl TextValue for &Value {
    fn text(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text.as_str()),
            _ => None,
        }
    }
}

impl TextValue for &str {
    fn text(&self) -> Option<&str> {
        Some(self)
    }
}
