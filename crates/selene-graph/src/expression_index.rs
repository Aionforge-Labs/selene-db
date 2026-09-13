//! Snapshot-owned expression keys over the existing typed scalar index engine.

use std::{collections::BTreeMap, sync::Arc};

use selene_catalog::{
    CatalogDescriptor, CatalogObjectId, CatalogPayload, DeclarationState, IndexConfiguration,
};
use selene_core::{
    DbString, LabelSet, NodeId, PropertyMap, Value, db_string,
    scalar_index_expression::ScalarIndexExpression,
};

use crate::{SeleneGraph, TypedIndex, TypedIndexKind};

#[derive(Clone, Debug)]
pub(crate) struct ExpressionIndex {
    pub(crate) descriptor: CatalogDescriptor,
    label: DbString,
    expression: ScalarIndexExpression,
    index: Arc<TypedIndex>,
    rejected: u64,
}

pub(crate) type ExpressionIndexes = BTreeMap<u64, ExpressionIndex>;

impl ExpressionIndex {
    fn change(&mut self, properties: &PropertyMap, row: u32, insert: bool) {
        let valid = match self.expression.evaluate(properties) {
            Ok(Value::Null) => true,
            Ok(value) => {
                let index = Arc::make_mut(&mut self.index);
                if insert {
                    index.insert(&value, row)
                } else {
                    index.remove(&value, row)
                }
                .is_ok()
            }
            Err(_) => false,
        };
        if !valid {
            if insert {
                self.rejected += 1;
            } else {
                self.rejected -= 1;
            }
        }
    }

    fn usable(&self, graph: u64) -> bool {
        matches!(self.descriptor.payload(), CatalogPayload::Index(index)
            if index.metadata.state == DeclarationState::Ready)
            && self.rejected == 0
            && matches!(self.descriptor.parent(), selene_catalog::CatalogParent::Graph(owner) if owner.get() == graph)
    }
}

/// Incremental maintenance is inside the same unpublished graph as primary data.
pub(crate) fn update(
    indexes: &mut ExpressionIndexes,
    old: Option<(&LabelSet, &PropertyMap)>,
    new: Option<(&LabelSet, &PropertyMap)>,
    row: u32,
) {
    for index in indexes.values_mut() {
        if let Some((labels, properties)) = old
            && labels.contains(&index.label)
        {
            index.change(properties, row, false);
        }
        if let Some((labels, properties)) = new
            && labels.contains(&index.label)
        {
            index.change(properties, row, true);
        }
    }
}

impl SeleneGraph {
    pub(crate) fn matches_expression_declaration(
        &self,
        declaration: &selene_catalog::IndexDeclaration,
    ) -> bool {
        let IndexConfiguration::Expression { expression, kind } = &declaration.configuration else {
            return false;
        };
        if declaration.target.element != selene_catalog::ElementKind::Node
            || declaration.target.properties != [expression.property.clone()]
        {
            return false;
        }
        self.expression_indexes.values().any(|entry| {
            entry.label.as_str() == declaration.target.label
                && entry.expression == *expression
                && entry.index.kind() == kind_from(*kind)
        })
    }

    pub(crate) fn rebuild_expression_indexes(&mut self) -> selene_catalog::CatalogResult<()> {
        let declarations: Vec<_> = self.catalog_declarations().cloned().collect();
        self.expression_indexes.clear();
        self.bind_expression_indexes(&declarations)
    }

    /// Build only new or changed descriptor revisions. Existing snapshot-local
    /// entries have already followed data deltas; no read-time build is allowed.
    pub(crate) fn bind_expression_indexes(
        &mut self,
        declarations: &[CatalogDescriptor],
    ) -> selene_catalog::CatalogResult<()> {
        let mut next = BTreeMap::new();
        for descriptor in declarations {
            let (CatalogObjectId::Index(id), CatalogPayload::Index(declaration)) =
                (descriptor.id(), descriptor.payload())
            else {
                continue;
            };
            let IndexConfiguration::Expression { expression, kind } = &declaration.configuration
            else {
                continue;
            };
            if let Some(existing) = self.expression_indexes.get(&id.get())
                && existing.descriptor == *descriptor
            {
                next.insert(id.get(), existing.clone());
                continue;
            }
            if declaration.metadata.state != DeclarationState::Ready {
                continue;
            }
            let label = db_string(&declaration.target.label).map_err(|_| invalid())?;
            let mut entry = ExpressionIndex {
                descriptor: descriptor.clone(),
                label: label.clone(),
                expression: expression.clone(),
                index: Arc::new(TypedIndex::new(kind_from(*kind))),
                rejected: 0,
            };
            for row in self.node_store.alive.iter() {
                if self
                    .node_store
                    .labels
                    .get(row as usize)
                    .is_some_and(|labels| labels.contains(&label))
                {
                    let properties = self
                        .node_store
                        .properties
                        .get(row as usize)
                        .ok_or_else(invalid)?;
                    entry.change(properties, row, true);
                }
            }
            next.insert(id.get(), entry);
        }
        self.expression_indexes = next;
        Ok(())
    }

    /// Discover complete expression indexes against this exact immutable snapshot.
    /// Returned IDs are catalog identities, not row positions or persistent keys.
    #[doc(hidden)]
    pub fn scalar_expression_indexes(
        &self,
        label: &DbString,
    ) -> impl Iterator<Item = (u64, &ScalarIndexExpression, TypedIndexKind)> {
        self.expression_indexes
            .iter()
            .filter(move |(_, entry)| entry.label == *label && entry.usable(self.graph_id().get()))
            .map(|(id, entry)| (*id, &entry.expression, entry.index.kind()))
    }

    /// Exact candidate identities, or `None` when this expression cannot safely
    /// answer. The original predicate remains necessary for query evaluation.
    #[doc(hidden)]
    pub fn scalar_expression_candidates(
        &self,
        id: u64,
        expression: &ScalarIndexExpression,
        value: &Value,
    ) -> Option<Vec<NodeId>> {
        let entry = self.expression_indexes.get(&id)?;
        if !entry.usable(self.graph_id().get()) || entry.expression != *expression {
            return None;
        }
        let rows = entry.index.lookup_eq(value)?;
        Some(
            rows.iter()
                .filter_map(|row| {
                    self.node_id_for_node_row(crate::store::NodeRow::new(row))
                        .filter(|id| self.is_node_alive(*id))
                })
                .collect(),
        )
    }

    /// Exact candidate count without materializing identities.
    #[doc(hidden)]
    pub fn scalar_expression_cardinality(&self, id: u64, value: &Value) -> Option<u64> {
        let entry = self.expression_indexes.get(&id)?;
        entry
            .usable(self.graph_id().get())
            .then(|| entry.index.lookup_eq(value).map(|rows| rows.len()))
            .flatten()
    }
}

fn invalid() -> selene_catalog::CatalogError {
    selene_catalog::CatalogError::InvalidDeclaration {
        reason: "invalid_expression_index",
    }
}

fn kind_from(kind: selene_core::SchemaPropertyIndexKind) -> TypedIndexKind {
    use selene_core::SchemaPropertyIndexKind as K;
    match kind {
        K::Bool => TypedIndexKind::Bool,
        K::I64 => TypedIndexKind::I64,
        K::U64 => TypedIndexKind::U64,
        K::I128 => TypedIndexKind::I128,
        K::U128 => TypedIndexKind::U128,
        K::Decimal => TypedIndexKind::Decimal,
        K::F32 => TypedIndexKind::F32,
        K::F64 => TypedIndexKind::F64,
        K::String => TypedIndexKind::String,
        K::Date => TypedIndexKind::Date,
        K::LocalDateTime => TypedIndexKind::LocalDateTime,
        K::ZonedDateTime => TypedIndexKind::ZonedDateTime,
        K::LocalTime => TypedIndexKind::LocalTime,
        K::ZonedTime => TypedIndexKind::ZonedTime,
        K::Duration => TypedIndexKind::Duration,
        K::Uuid => TypedIndexKind::Uuid,
    }
}
