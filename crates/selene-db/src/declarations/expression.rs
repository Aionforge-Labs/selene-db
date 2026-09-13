//! Analyzed scalar-index declaration through the existing catalog publication funnel.

use super::*;

#[cfg(test)]
mod tests;

impl Catalog {
    /// Create a node scalar-expression index atomically with its durable declaration.
    ///
    /// This Selene Rust extension accepts an expression over the sole binding `n`:
    /// `lower(n.name)`, `upper(n.name)`, `json_get_path_scalar(n.body, 'key')`, or
    /// explicit `json_get_path_text`, including bounded composition and constant
    /// selector-array documents. It does not add GQL grammar. Parameters, query
    /// clauses, nondeterministic/unknown functions, casts and multi-valued targets
    /// are rejected. The supplied kind selects ordinary typed scalar keys, never
    /// string coercion. Data that cannot be evaluated/keyed makes the accelerator
    /// scan-only; it does not change query or mutation error semantics.
    pub fn create_expression_index(
        &self,
        owner: &ObjectPath,
        name: &PathSegment,
        label: &str,
        source: &str,
        kind: ScalarIndexKind,
    ) -> Result<DeclarationDescriptor> {
        let expression = selene_gql::analyze::index_expression::analyze_source(source)
            .map_err(declaration_error)?;
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let descriptor =
                find_object(&base, owner)?.ok_or_else(|| Error::not_found(owner, "graph"))?;
            let CatalogObjectId::Graph(id) = descriptor.id() else {
                return Err(Error::wrong_kind(owner, "graph", descriptor.kind()));
            };
            if base.catalog.declaration(descriptor.id(), &name.0).is_some() {
                return Err(declaration_error("duplicate_expression_index_name"));
            }
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let mut transaction =
                CatalogTransaction::new(&draft.catalog).map_err(Error::from_catalog_invariant)?;
            let raw = next_id(draft.high_water.index, "expression index")?;
            let index = CatalogDescriptor::index(
                selene_catalog::IndexId::new(raw).map_err(Error::from_catalog_invariant)?,
                name.0.clone(),
                CatalogParent::Graph(id),
                transaction.generation(),
                CreationMetadata::new(transaction.generation(), None),
                IndexDeclaration {
                    metadata: DeclarationMetadata::new(DeclarationState::Ready),
                    target: PropertyTarget {
                        element: ElementKind::Node,
                        label: label.into(),
                        properties: vec![expression.property.clone()],
                    },
                    configuration: IndexConfiguration::Expression { expression, kind },
                },
            )
            .map_err(Error::from_catalog_invariant)?;
            let result = summary(&index).expect("index summary");
            transaction
                .insert(index)
                .map_err(Error::from_catalog_invariant)?;
            draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
            draft.high_water.index = raw;
            stage_owner_binding(&mut draft, &base, CatalogParent::Graph(id))?;
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(result)
        })
    }
}
