//! Constraint declarations, distinct from complete implementation bindings.

use serde::{Deserialize, Serialize};

use crate::{
    CatalogError, CatalogResult, DeclarationMetadata, DeclarationState, IndexId, PropertyTarget,
};

/// Declarative constraint semantics.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ConstraintKind {
    /// Tuple uniqueness; a tuple with any missing/null component is excluded.
    Unique,
    /// Composite spelling of the same tuple uniqueness semantics.
    CompositeUnique,
    /// Unique tuple with every component required and non-null.
    Key,
}

/// One graph- or graph-type-owned constraint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConstraintDeclaration {
    /// Profile, lifecycle, and dependency revisions.
    pub metadata: DeclarationMetadata,
    /// Analyzed property target.
    pub target: PropertyTarget,
    /// Exact declaring node/edge type name, not merely its label.
    pub declaring_type: String,
    /// Semantic constraint family.
    pub kind: ConstraintKind,
    /// Exact required backing. An active constraint must name complete backing.
    pub backing_index: Option<IndexId>,
}

impl ConstraintDeclaration {
    pub(crate) fn validate(&self) -> CatalogResult<()> {
        self.metadata.validate()?;
        self.target.validate()?;
        if self.declaring_type.is_empty()
            || self.declaring_type.len() > selene_core::db_string::MAX_DB_STRING_BYTES
        {
            return Err(CatalogError::InvalidDeclaration {
                reason: "invalid_declaring_type",
            });
        }
        // Format-2 catalogs written before F05-PR05 encode unary annotations
        // without a separate IndexId. Runtime admission builds the same complete
        // tuple index for that representation; it is not a scan fallback.
        let legacy_inline =
            self.kind == ConstraintKind::Unique && self.target.properties.len() == 1;
        if self.metadata.state == DeclarationState::Ready
            && self.backing_index.is_none()
            && !legacy_inline
        {
            return Err(CatalogError::InvalidDeclaration {
                reason: "unsupported_constraint_activation",
            });
        }
        Ok(())
    }
}
