use super::*;
use parking_lot::Mutex;
use selene_core::{Change, LabelSet, PropertyMap, PropertyValueType, SchemaChange, db_string};
use std::thread;
use std::time::{Duration, Instant};

use crate::index_provider::ProviderError;
use crate::typed_index::TypedIndexKind;

struct TestProvider {
    tag: ProviderTag,
    seen: Mutex<Vec<Change>>,
}

impl TestProvider {
    fn new(tag: ProviderTag) -> Self {
        Self {
            tag,
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl IndexProvider for TestProvider {
    fn provider_tag(&self) -> ProviderTag {
        self.tag
    }

    fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
        self.seen.lock().push(change.clone());
        Ok(())
    }
}

fn sample_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("shared.type").unwrap(),
        node_types: vec![crate::NodeTypeDef {
            name: db_string("shared.node").unwrap(),
            key_labels: LabelSet::single(db_string("SharedNode").unwrap()),
            properties: vec![crate::PropertyTypeDef {
                name: db_string("shared.name").unwrap(),
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
            validation_mode: crate::ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    }
}

mod basic;
mod builder;
mod concurrency;
mod from_graph;
mod schema_version;
