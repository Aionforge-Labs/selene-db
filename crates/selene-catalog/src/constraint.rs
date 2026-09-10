//! Constraint declarations, distinct from complete implementation bindings.

use serde::{Deserialize, Serialize};

use crate::{
    CatalogError, CatalogResult, DeclarationMetadata, DeclarationState, IndexId, PropertyTarget,
};

/// Declarative constraint semantics.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ConstraintKind {
    /// Existing arity-one uniqueness; missing and null values do not conflict.
    Unique,
    /// Reserved named composite uniqueness. Activation belongs to F05-PR05.
    CompositeUnique,
    /// Reserved named key semantics. Activation belongs to F05-PR05.
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
    /// Optional future exact backing index; current arity-one enforcement uses whole-state validation.
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
        if self.kind == ConstraintKind::Unique && self.target.properties.len() != 1 {
            return Err(CatalogError::InvalidDeclaration {
                reason: "unique_arity",
            });
        }
        if self.metadata.state == DeclarationState::Ready
            && (self.kind != ConstraintKind::Unique || self.backing_index.is_some())
        {
            return Err(CatalogError::InvalidDeclaration {
                reason: "unsupported_constraint_activation",
            });
        }
        Ok(())
    }
}
