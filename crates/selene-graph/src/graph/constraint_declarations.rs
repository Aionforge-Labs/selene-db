//! Normalize legacy annotations and admit catalog-owned exact constraints.

use selene_catalog::{
    ConstraintDeclaration, ConstraintKind, DeclarationMetadata, DeclarationState, ElementKind,
    PropertyTarget,
};

use crate::{GraphTypeDef, SeleneGraph};

impl SeleneGraph {
    /// Whether named-type obligations were admitted with this immutable state.
    #[doc(hidden)]
    #[must_use]
    pub fn named_constraints_match(&self, named: &GraphTypeDef) -> bool {
        self.named_constraints
            .as_ref()
            .is_some_and(|(definition, _)| definition.as_ref() == named)
    }

    /// Admit the catalog's named-type obligation separately from a potentially
    /// different instance schema, using the same complete tuple-index service.
    #[doc(hidden)]
    pub fn admit_named_constraints(
        &mut self,
        before: Option<&Self>,
        named: std::sync::Arc<GraphTypeDef>,
        changes: &[selene_core::Change],
    ) -> crate::GraphResult<()> {
        use selene_core::Change;
        let prior = before
            .and_then(|g| g.named_constraints.as_ref())
            .filter(|(ty, _)| ty == &named);
        let rebuild = prior.is_none()
            || changes.is_empty()
            || changes.iter().any(|c| {
                matches!(
                    c,
                    Change::SchemaChanged { .. }
                        | Change::NodesOfTypeTruncated { .. }
                        | Change::EdgesOfTypeTruncated { .. }
                        | Change::GraphReset { .. }
                )
            });
        if rebuild {
            crate::type_validator::validate_entity_shape(self, &named)?;
        }
        for change in changes {
            crate::type_validator::validate_change(change, self, &named)?;
        }
        let mut after_view = self.clone();
        after_view.meta.bound_type = Some(named.clone());
        let indexes = if rebuild {
            crate::type_validator::ConstraintIndexes::build(
                &after_view,
                after_view.unique_declarations(),
            )?
        } else {
            let mut before_view = before.expect("admitted predecessor").clone();
            before_view.meta.bound_type = Some(named.clone());
            prior
                .expect("admitted predecessor")
                .1
                .apply(changes, &before_view, &after_view)?
        };
        self.named_constraints = Some((named, indexes));
        Ok(())
    }

    /// Normalize the selected closed type's input annotations into logical rules.
    ///
    /// This is used only before schema publication. Once catalog-bound, these
    /// annotations are a derived implementation checked against the declarations.
    /// Backing identities are allocated by the facade before publication.
    #[doc(hidden)]
    #[must_use]
    pub fn unique_declarations(&self) -> Vec<ConstraintDeclaration> {
        let Some(definition) = &self.meta.bound_type else {
            return Vec::new();
        };
        let mut rules = Vec::new();
        for ty in &definition.node_types {
            for property in ty.properties.iter().filter(|property| property.unique) {
                for label in ty.key_labels.iter() {
                    rules.push(ConstraintDeclaration {
                        metadata: DeclarationMetadata::new(DeclarationState::Ready),
                        target: PropertyTarget {
                            element: ElementKind::Node,
                            label: label.to_string(),
                            properties: vec![property.name.to_string()],
                        },
                        declaring_type: ty.name.to_string(),
                        kind: ConstraintKind::Unique,
                        backing_index: None,
                    });
                }
            }
        }
        for ty in &definition.edge_types {
            for property in ty.properties.iter().filter(|property| property.unique) {
                rules.push(ConstraintDeclaration {
                    metadata: DeclarationMetadata::new(DeclarationState::Ready),
                    target: PropertyTarget {
                        element: ElementKind::Edge,
                        label: ty.label.to_string(),
                        properties: vec![property.name.to_string()],
                    },
                    declaring_type: ty.name.to_string(),
                    kind: ConstraintKind::Unique,
                    backing_index: None,
                });
            }
        }
        rules
    }

    pub(crate) fn constraint_rules(&self) -> Vec<ConstraintDeclaration> {
        let mut rules = self.unique_declarations();
        if self.meta.bound_type.is_none() {
            return rules;
        }
        for descriptor in self.catalog_declarations() {
            let selene_catalog::CatalogPayload::Constraint(rule) = descriptor.payload() else {
                continue;
            };
            if rule.metadata.state != DeclarationState::Ready {
                continue;
            }
            if let Some(existing) = rules.iter_mut().find(|existing| same_rule(existing, rule)) {
                *existing = rule.clone();
            } else if rule.kind != ConstraintKind::Unique
                && self.validate_constraint_target(rule).is_ok()
            {
                rules.push(rule.clone());
            }
        }
        rules
    }

    /// Verify the exact type and ordered property target, without activating it.
    #[doc(hidden)]
    pub fn validate_constraint_target(
        &self,
        rule: &ConstraintDeclaration,
    ) -> selene_catalog::CatalogResult<()> {
        let invalid = || selene_catalog::CatalogError::InvalidDeclaration {
            reason: "invalid_constraint_target",
        };
        let definition = self.meta.bound_type.as_deref().ok_or_else(invalid)?;
        let properties = match rule.target.element {
            ElementKind::Node => definition
                .node_types
                .iter()
                .find(|ty| {
                    ty.name.as_str() == rule.declaring_type
                        && ty
                            .key_labels
                            .iter()
                            .any(|l| l.as_str() == rule.target.label)
                })
                .map(|ty| &ty.properties),
            ElementKind::Edge => definition
                .edge_types
                .iter()
                .find(|ty| {
                    ty.name.as_str() == rule.declaring_type
                        && ty.label.as_str() == rule.target.label
                })
                .map(|ty| &ty.properties),
        }
        .ok_or_else(invalid)?;
        if rule.target.properties.is_empty()
            || rule
                .target
                .properties
                .iter()
                .any(|key| !properties.iter().any(|p| p.name.as_str() == key))
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(crate) fn rebuild_constraints(&mut self) -> crate::GraphResult<()> {
        let rules = self.constraint_rules();
        for rule in &rules {
            self.validate_constraint_target(rule).map_err(|error| {
                crate::GraphError::Inconsistent {
                    reason: error.to_string(),
                }
            })?;
        }
        self.constraints = crate::type_validator::ConstraintIndexes::build(self, rules)?;
        Ok(())
    }

    pub(crate) fn admit_replay_constraints(
        &mut self,
        before: Option<&Self>,
        catalog: &selene_catalog::CatalogSnapshot,
        changes: &[selene_core::Change],
    ) -> crate::GraphResult<()> {
        let mut rules = self.unique_declarations();
        let owner = selene_catalog::GraphId::new(self.graph_id().get()).expect("admitted graph ID");
        for descriptor in catalog.declarations(selene_catalog::CatalogObjectId::Graph(owner)) {
            let selene_catalog::CatalogPayload::Constraint(rule) = descriptor.payload() else {
                continue;
            };
            if rule.metadata.state != DeclarationState::Ready {
                continue;
            }
            if let Some(existing) = rules.iter_mut().find(|r| same_rule(r, rule)) {
                *existing = rule.clone();
            } else {
                rules.push(rule.clone());
            }
        }
        let incremental = before.is_some_and(|old| {
            old.meta.bound_type == self.meta.bound_type && old.constraints.matches_rules(&rules)
        }) && !changes.iter().any(|c| {
            matches!(
                c,
                selene_core::Change::NodesOfTypeTruncated { .. }
                    | selene_core::Change::EdgesOfTypeTruncated { .. }
                    | selene_core::Change::GraphReset { .. }
            )
        });
        self.constraints = if incremental {
            let before = before.expect("incremental predecessor");
            before.constraints.apply(changes, before, self)?
        } else {
            crate::type_validator::ConstraintIndexes::build(self, rules)?
        };
        Ok(())
    }
}

pub(crate) fn same_rule(a: &ConstraintDeclaration, b: &ConstraintDeclaration) -> bool {
    a.target == b.target && a.declaring_type == b.declaring_type && a.kind == b.kind
}
