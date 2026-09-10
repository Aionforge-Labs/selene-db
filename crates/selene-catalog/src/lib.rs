//! Catalog ownership types for Selene DB.
//!
//! This lower crate is an advanced engine boundary, not part of the stable 2.x
//! embedding API. Applications should depend on `selene-db` instead.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod constraint;
mod declaration;
mod dependencies;
mod descriptor;
mod error;
mod identity;
mod index;
mod logical;
mod name;
mod native;
mod native_type;
mod snapshot;
mod transaction;

pub use constraint::{ConstraintDeclaration, ConstraintKind};
pub use declaration::{
    DeclarationDependency, DeclarationMetadata, DeclarationProfile, DeclarationState,
};
pub use descriptor::{CatalogDescriptor, CatalogParent, CatalogPayload, CreationMetadata};
pub use error::{CatalogError, CatalogResult};
pub use identity::{
    BindingTableId, CatalogGeneration, CatalogId, CatalogObjectId, CatalogObjectKind, ConstraintId,
    DirectoryId, GraphId, GraphTypeId, IndexId, ProcedureId, SchemaId,
};
pub use index::{
    ElementKind, IndexConfiguration, IndexDeclaration, IndexFamily, IndexJsonSelector,
    PropertyTarget, ReservedIndexExpression, generated_composite_index_name, generated_index_name,
};
pub use logical::{CatalogLogicalChange, CatalogLogicalRecords};
pub use name::{CATALOG_UNICODE_VERSION, CatalogName, IdentifierForm};
pub use native::{
    NativeBinding, NativeCandidateState, NativeDeclaration, NativeDefault, NativeEffect,
    NativeField, NativeParameter, NativeProcedure, NativeProjection, NativeType,
};
pub use snapshot::{CatalogMemoryAccounting, CatalogSnapshot, CatalogSnapshotBuilder};
pub use transaction::CatalogTransaction;
