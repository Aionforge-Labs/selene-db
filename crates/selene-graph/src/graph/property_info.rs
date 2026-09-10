//! Checked property-index metadata for required-index filters, without row APIs.

use super::{IndexedEntity, PropertyIndexEntry, SeleneGraph, TypedIndexKind};
use selene_catalog::{ElementKind, IndexFamily};
use selene_core::{DbString, Value};

/// Query-eligible registration metadata. Incomplete indexes still admit values
/// according to their declared kind; callers may then use their documented scan
/// fallback. Ineligible catalog bindings never produce this view.
#[derive(Clone, Copy, Debug)]
pub struct PropertyIndexReadInfo<'a>(&'a PropertyIndexEntry);

impl PropertyIndexReadInfo<'_> {
    /// The registered scalar kind, independent of current drift.
    #[must_use]
    pub fn kind(self) -> TypedIndexKind {
        self.0.kind()
    }

    /// Whether every query-relevant current value is represented by the index.
    #[must_use]
    pub fn is_complete(self) -> bool {
        self.0.is_complete()
    }

    /// Whether the registered kind accepts this argument, independent of drift.
    #[must_use]
    pub fn admits(self, value: &Value) -> bool {
        self.0.admits(value)
    }
}

impl SeleneGraph {
    /// Inspect a query-eligible property registration without exposing physical
    /// rows or an unchecked index handle. `None` also means ineligible binding.
    #[must_use]
    pub fn property_index_read_info(
        &self,
        entity: IndexedEntity,
        label: &DbString,
        property: &DbString,
    ) -> Option<PropertyIndexReadInfo<'_>> {
        let element = match entity {
            IndexedEntity::Node => ElementKind::Node,
            IndexedEntity::Edge => ElementKind::Edge,
        };
        self.query_property_entry(element, label, property)
            .map(PropertyIndexReadInfo)
    }

    pub(super) fn query_property_entry(
        &self,
        element: ElementKind,
        label: &DbString,
        property: &DbString,
    ) -> Option<&PropertyIndexEntry> {
        if !self.catalog_index_usable(
            element,
            label,
            std::slice::from_ref(property),
            IndexFamily::Property,
        ) {
            return None;
        }
        let entries = match element {
            ElementKind::Node => &self.property_index,
            ElementKind::Edge => &self.edge_property_index,
        };
        entries.get(&(label.clone(), property.clone()))
    }
}
