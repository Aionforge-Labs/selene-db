//! Existing arity-one enforcement as a checked derived catalog adapter.

use selene_catalog::{
    ConstraintDeclaration, ConstraintKind, DeclarationMetadata, DeclarationState, ElementKind,
    PropertyTarget,
};

use crate::SeleneGraph;

impl SeleneGraph {
    /// Normalize the selected closed type's input annotations into logical rules.
    ///
    /// This is used only before schema publication. Once catalog-bound, these
    /// annotations are a derived implementation checked against the declarations.
    /// No property index is created or required by the current unique validator.
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
}
