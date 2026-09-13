//! Constraint activation holds the facade reservation from graph selection
//! through complete index construction and the ordinary publication cut-line.

use super::*;
#[cfg(test)]
mod tests;

impl Catalog {
    /// Create a named UNIQUE or key constraint on an existing closed graph.
    ///
    /// This Selene extension is a Rust catalog facility, not ISO GQL grammar.
    /// `Unique` and `CompositeUnique` exclude tuples with any missing/null
    /// component. `Key` requires every component to be present and non-null.
    /// All components use the engine's distinctness/equality domain and binary
    /// string collation. Targets are ordered exact property names within one
    /// declaring node or edge type. Arity one is not a separate implementation.
    ///
    /// The supplied declaration must be inactive with no backing identity. The
    /// engine allocates private complete backing and publishes both together.
    /// A duplicate name, invalid target, incompatible value domain, duplicate
    /// tuple, or publication failure leaves the old catalog/data/index intact.
    pub fn create_constraint(
        &self,
        owner: &ObjectPath,
        name: &PathSegment,
        mut rule: ConstraintDeclaration,
    ) -> Result<DeclarationDescriptor> {
        if rule.metadata.state != DeclarationState::Inactive || rule.backing_index.is_some() {
            return Err(declaration_error("caller_asserted_activation"));
        }
        // The legacy Unique spelling identifies a property annotation in stored
        // catalogs. Named declarations use one composite spelling at every arity.
        if rule.kind == ConstraintKind::Unique {
            rule.kind = ConstraintKind::CompositeUnique;
        }
        self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let descriptor =
                find_object(&base, owner)?.ok_or_else(|| Error::not_found(owner, "graph"))?;
            let CatalogObjectId::Graph(id) = descriptor.id() else {
                return Err(Error::wrong_kind(owner, "graph", descriptor.kind()));
            };
            if base.catalog.declaration(descriptor.id(), &name.0).is_some() {
                return Err(declaration_error("duplicate_constraint_name"));
            }
            let mut draft = DatabaseDraft::new(&base, &reservation);
            draft.pin_graph(&base, id)?;
            draft
                .selected_graph()?
                .validate_constraint_target(&rule)
                .map_err(Error::from_catalog_invariant)?;
            let mut transaction =
                CatalogTransaction::new(&draft.catalog).map_err(Error::from_catalog_invariant)?;
            rule.metadata.state = DeclarationState::Ready;
            crate::registration_stage::add_constraint_backing(
                &mut transaction,
                id,
                name.display(),
                &mut rule,
                &mut draft.high_water,
            )?;
            let raw = next_id(draft.high_water.constraint, "constraint")?;
            let constraint = CatalogDescriptor::constraint(
                selene_catalog::ConstraintId::new(raw).map_err(Error::from_catalog_invariant)?,
                name.0.clone(),
                CatalogParent::Graph(id),
                transaction.generation(),
                CreationMetadata::new(transaction.generation(), None),
                rule,
            )
            .map_err(Error::from_catalog_invariant)?;
            let result = summary(&constraint).expect("constraint summary");
            transaction
                .insert(constraint)
                .map_err(Error::from_catalog_invariant)?;
            draft.catalog = transaction.build().map_err(Error::from_catalog_invariant)?;
            draft.high_water.constraint = raw;
            stage_owner_binding(&mut draft, &base, CatalogParent::Graph(id))?;
            require_committed(self.inner.publish_database_draft(reservation, draft)?)?;
            Ok(result)
        })
    }
}
