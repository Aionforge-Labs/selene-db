//! Structured catalog validation errors.

use std::{error::Error, fmt};

use crate::{CatalogGeneration, CatalogObjectId, CatalogObjectKind};

/// Result type for catalog identity, name, descriptor, and snapshot validation.
pub type CatalogResult<T> = Result<T, CatalogError>;

/// Failure returned while constructing or validating catalog metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CatalogError {
    /// Malformed or unsupported logical declaration, with a stable reason code.
    InvalidDeclaration {
        /// Stable machine-readable reason.
        reason: &'static str,
    },
    /// A declaration dependency failed whole-snapshot validation.
    InvalidDependency {
        /// Dependent identity.
        object: CatalogObjectId,
        /// Required identity.
        target: CatalogObjectId,
        /// Stable machine-readable reason.
        reason: &'static str,
    },
    /// Zero is outside every catalog object ID domain.
    ZeroIdentifier {
        /// Kind of ID that rejected zero.
        kind: CatalogObjectKind,
    },
    /// Catalog generations start at one.
    ZeroGeneration,
    /// The current catalog generation cannot be incremented.
    GenerationOverflow {
        /// Generation that could not be incremented.
        current: u64,
    },
    /// A user object identifier had an empty decoded spelling.
    EmptyIdentifier,
    /// A private-use scalar occurred in an identifier.
    PrivateUseCharacter {
        /// Rejected scalar.
        character: char,
    },
    /// The first regular-identifier scalar was outside the selected profile.
    InvalidRegularIdentifierStart {
        /// Rejected scalar.
        character: char,
    },
    /// A continuation scalar was outside the selected profile.
    InvalidRegularIdentifierContinue {
        /// Zero-based scalar position.
        index: usize,
        /// Rejected scalar.
        character: char,
    },
    /// Serialized name fields did not reproduce the selected canonical form.
    InvalidSerializedName,
    /// Descriptor ID, declared kind, and payload kind disagreed.
    DescriptorKindMismatch {
        /// Kind declared by the descriptor.
        declared: CatalogObjectKind,
        /// Kind carried by the typed ID.
        identifier: CatalogObjectKind,
        /// Kind carried by the payload.
        payload: CatalogObjectKind,
    },
    /// A descriptor used a parent variant not permitted for its kind.
    InvalidParentKind {
        /// Descriptor kind.
        object: CatalogObjectKind,
        /// Stable spelling of the observed parent variant.
        parent: &'static str,
    },
    /// A user descriptor attempted to reuse the synthetic root name.
    UserNameRequired {
        /// Descriptor kind requiring a user name.
        kind: CatalogObjectKind,
    },
    /// A root directory did not carry the encapsulated synthetic name.
    SyntheticRootNameRequired,
    /// Creation generation was after the descriptor generation.
    CreationGenerationAfterDescriptor {
        /// Creation generation.
        creation: CatalogGeneration,
        /// Descriptor generation.
        descriptor: CatalogGeneration,
    },
    /// The builder received one typed ID more than once.
    DuplicateIdentifier {
        /// Repeated typed identity.
        id: CatalogObjectId,
    },
    /// Two children used the same canonical name in one namespace.
    DuplicateCanonicalName {
        /// Existing descriptor identity.
        existing: CatalogObjectId,
        /// Conflicting descriptor identity.
        incoming: CatalogObjectId,
        /// Canonical spelling shared by both descriptors.
        canonical: String,
    },
    /// A descriptor named a parent absent from the snapshot.
    MissingParent {
        /// Child descriptor identity.
        object: CatalogObjectId,
        /// Missing parent identity.
        parent: CatalogObjectId,
    },
    /// A payload named a descriptor absent from the snapshot.
    MissingPayloadReference {
        /// Descriptor containing the reference.
        object: CatalogObjectId,
        /// Missing target identity.
        target: CatalogObjectId,
    },
    /// A graph referenced a graph type from another schema.
    CrossSchemaPayloadReference {
        /// Graph carrying the reference.
        object: CatalogObjectId,
        /// Referenced graph type.
        target: CatalogObjectId,
    },
    /// A descriptor generation was newer than its snapshot.
    DescriptorGenerationAfterSnapshot {
        /// Descriptor identity.
        object: CatalogObjectId,
        /// Descriptor generation.
        descriptor: CatalogGeneration,
        /// Snapshot generation.
        snapshot: CatalogGeneration,
    },
    /// The snapshot did not contain exactly one catalog and one root directory.
    InvalidRootCardinality {
        /// Number of catalog descriptors.
        catalogs: usize,
        /// Number of directory descriptors.
        directories: usize,
    },
    /// The synthetic root relationship did not match the builder's root IDs.
    InvalidSyntheticRoot,
    /// Child directories are outside the selected profile.
    UnsupportedDirectoryDepth {
        /// Selected maximum child-directory depth.
        maximum_depth: u8,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeclaration { reason } => {
                write!(formatter, "invalid declaration: {reason}")
            }
            Self::InvalidDependency {
                object,
                target,
                reason,
            } => write!(formatter, "{object} depends on {target}: {reason}"),
            Self::ZeroIdentifier { kind } => write!(formatter, "{kind} ID must be nonzero"),
            Self::ZeroGeneration => formatter.write_str("catalog generation must be nonzero"),
            Self::GenerationOverflow { current } => {
                write!(
                    formatter,
                    "catalog generation {current} cannot be incremented"
                )
            }
            Self::EmptyIdentifier => formatter.write_str("catalog identifier must not be empty"),
            Self::PrivateUseCharacter { character } => write!(
                formatter,
                "private-use character U+{:04X} is not permitted in a catalog identifier",
                *character as u32
            ),
            Self::InvalidRegularIdentifierStart { character } => write!(
                formatter,
                "character U+{:04X} is not a regular identifier start",
                *character as u32
            ),
            Self::InvalidRegularIdentifierContinue { index, character } => write!(
                formatter,
                "character U+{:04X} at scalar {index} is not a regular identifier continuation",
                *character as u32
            ),
            Self::InvalidSerializedName => {
                formatter.write_str("serialized catalog name is not in canonical form")
            }
            Self::DescriptorKindMismatch {
                declared,
                identifier,
                payload,
            } => write!(
                formatter,
                "descriptor kind {declared} does not match ID kind {identifier} and payload kind {payload}"
            ),
            Self::InvalidParentKind { object, parent } => {
                write!(formatter, "{object} descriptor cannot have {parent} parent")
            }
            Self::UserNameRequired { kind } => {
                write!(formatter, "{kind} descriptor requires a nonempty user name")
            }
            Self::SyntheticRootNameRequired => {
                formatter.write_str("root directory requires the synthetic zero-length name")
            }
            Self::CreationGenerationAfterDescriptor {
                creation,
                descriptor,
            } => write!(
                formatter,
                "creation generation {} is after descriptor generation {}",
                creation.get(),
                descriptor.get()
            ),
            Self::DuplicateIdentifier { id } => write!(formatter, "duplicate catalog ID {id}"),
            Self::DuplicateCanonicalName {
                existing,
                incoming,
                canonical,
            } => write!(
                formatter,
                "canonical name {canonical:?} conflicts between {existing} and {incoming}"
            ),
            Self::MissingParent { object, parent } => {
                write!(
                    formatter,
                    "catalog object {object} has missing parent {parent}"
                )
            }
            Self::MissingPayloadReference { object, target } => write!(
                formatter,
                "catalog object {object} has missing payload reference {target}"
            ),
            Self::CrossSchemaPayloadReference { object, target } => write!(
                formatter,
                "catalog object {object} references {target} in another schema"
            ),
            Self::DescriptorGenerationAfterSnapshot {
                object,
                descriptor,
                snapshot,
            } => write!(
                formatter,
                "catalog object {object} generation {} is after snapshot generation {}",
                descriptor.get(),
                snapshot.get()
            ),
            Self::InvalidRootCardinality {
                catalogs,
                directories,
            } => write!(
                formatter,
                "catalog snapshot requires one catalog and one directory; found {catalogs} and {directories}"
            ),
            Self::InvalidSyntheticRoot => {
                formatter.write_str("catalog snapshot has an invalid synthetic root relationship")
            }
            Self::UnsupportedDirectoryDepth { maximum_depth } => write!(
                formatter,
                "child directories are unsupported by the selected profile (maximum depth {maximum_depth})"
            ),
        }
    }
}

impl Error for CatalogError {}
