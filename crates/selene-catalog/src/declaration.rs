//! Logical registration data. No runtime handles or accelerator contents live here.

use serde::{Deserialize, Serialize};

use crate::{CatalogError, CatalogGeneration, CatalogObjectId, CatalogResult};

/// Durable declaration state, not a claim about the current accelerator's completeness.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DeclarationState {
    /// No implementation is bound.
    Inactive,
    /// Construction has not completed.
    Building,
    /// Admitted by an implementation; readers must still validate the current binding.
    Ready,
    /// Construction or validation failed.
    Failed,
}

/// Exact semantic profile coordinate under which a target was analyzed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclarationProfile {
    /// Generated profile identifier.
    pub id: String,
    /// Generated canonical profile hash.
    pub hash: String,
    /// Version of the native declaration semantics, independent of release version.
    pub semantics: u32,
}

impl DeclarationProfile {
    /// The currently implemented native declaration semantics.
    #[must_use]
    pub fn current() -> Self {
        Self {
            id: selene_profile::PROFILE_ID.to_owned(),
            hash: selene_profile::PROFILE_HASH.to_owned(),
            semantics: 1,
        }
    }
}

/// A dependency pins a stable identity and a particular descriptor revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclarationDependency {
    /// Referenced declaration, owner, or shared catalog object.
    pub id: CatalogObjectId,
    /// Required revision, not the object's numeric ID.
    pub generation: CatalogGeneration,
}

/// Common metadata for an index, constraint, or native registration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclarationMetadata {
    /// Declarative lifecycle; never sufficient to select a physical implementation.
    pub state: DeclarationState,
    /// Analysis/profile coordinate.
    pub profile: DeclarationProfile,
    /// Shared dependencies. Deleting a dependency is RESTRICT, never a cascade.
    pub dependencies: Vec<DeclarationDependency>,
}

impl DeclarationMetadata {
    /// Create metadata for the current profile without dependencies.
    #[must_use]
    pub fn new(state: DeclarationState) -> Self {
        Self {
            state,
            profile: DeclarationProfile::current(),
            dependencies: Vec::new(),
        }
    }

    pub(crate) fn validate(&self) -> CatalogResult<()> {
        if self.profile != DeclarationProfile::current() {
            return Err(CatalogError::InvalidDeclaration {
                reason: "incompatible_profile",
            });
        }
        if self.dependencies.len() > 256 {
            return Err(CatalogError::InvalidDeclaration {
                reason: "dependency_limit",
            });
        }
        let mut ids = std::collections::BTreeSet::new();
        if self
            .dependencies
            .iter()
            .any(|dependency| !ids.insert(dependency.id))
        {
            return Err(CatalogError::InvalidDeclaration {
                reason: "duplicate_dependency",
            });
        }
        Ok(())
    }
}
