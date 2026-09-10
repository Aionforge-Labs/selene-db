//! Kind-safe catalog object identities and generations.

use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Serialize};

use crate::{CatalogError, CatalogResult};

/// Supported catalog descriptor kinds.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogObjectKind {
    /// Catalog ownership root.
    Catalog,
    /// Synthetic root directory.
    Directory,
    /// Schema under the root directory.
    Schema,
    /// Named graph descriptor.
    Graph,
    /// Named graph-type descriptor.
    GraphType,
    /// Named binding-table descriptor.
    BindingTable,
    /// Named procedure descriptor.
    Procedure,
    /// Named index descriptor.
    Index,
    /// Named constraint descriptor.
    Constraint,
}

impl fmt::Display for CatalogObjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Catalog => "catalog",
            Self::Directory => "directory",
            Self::Schema => "schema",
            Self::Graph => "graph",
            Self::GraphType => "graph type",
            Self::BindingTable => "binding table",
            Self::Procedure => "procedure",
            Self::Index => "index",
            Self::Constraint => "constraint",
        })
    }
}

macro_rules! catalog_id {
    ($name:ident, $kind:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[repr(transparent)]
        pub struct $name(NonZeroU64);

        impl $name {
            #[doc = concat!("Construct a nonzero `", stringify!($name), "`.")]
            ///
            /// # Errors
            ///
            /// Returns [`CatalogError::ZeroIdentifier`] when `raw` is zero.
            pub const fn new(raw: u64) -> CatalogResult<Self> {
                match NonZeroU64::new(raw) {
                    Some(value) => Ok(Self(value)),
                    None => Err(CatalogError::ZeroIdentifier {
                        kind: CatalogObjectKind::$kind,
                    }),
                }
            }

            #[doc = concat!("Return the raw value of this `", stringify!($name), "`.")]
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}({})", stringify!($name), self.get())
            }
        }
    };
}

catalog_id!(CatalogId, Catalog, "Stable catalog identity.");
catalog_id!(
    DirectoryId,
    Directory,
    "Stable synthetic catalog-directory identity."
);
catalog_id!(SchemaId, Schema, "Stable catalog schema identity.");
catalog_id!(GraphId, Graph, "Stable catalog graph-object identity.");
catalog_id!(
    GraphTypeId,
    GraphType,
    "Stable catalog graph-type-object identity."
);
catalog_id!(
    BindingTableId,
    BindingTable,
    "Stable catalog binding-table-object identity."
);
catalog_id!(
    ProcedureId,
    Procedure,
    "Stable catalog procedure-object identity."
);
catalog_id!(IndexId, Index, "Stable catalog index-object identity.");
catalog_id!(
    ConstraintId,
    Constraint,
    "Stable catalog constraint-object identity."
);

/// A typed identity for any supported catalog descriptor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogObjectId {
    /// Catalog identity.
    Catalog(CatalogId),
    /// Directory identity.
    Directory(DirectoryId),
    /// Schema identity.
    Schema(SchemaId),
    /// Graph identity.
    Graph(GraphId),
    /// Graph-type identity.
    GraphType(GraphTypeId),
    /// Binding-table identity.
    BindingTable(BindingTableId),
    /// Procedure identity.
    Procedure(ProcedureId),
    /// Index identity.
    Index(IndexId),
    /// Constraint identity.
    Constraint(ConstraintId),
}

impl CatalogObjectId {
    /// Numeric value within this typed identity's own allocation domain.
    #[must_use]
    pub const fn get(self) -> u64 {
        match self {
            Self::Catalog(id) => id.get(),
            Self::Directory(id) => id.get(),
            Self::Schema(id) => id.get(),
            Self::Graph(id) => id.get(),
            Self::GraphType(id) => id.get(),
            Self::BindingTable(id) => id.get(),
            Self::Procedure(id) => id.get(),
            Self::Index(id) => id.get(),
            Self::Constraint(id) => id.get(),
        }
    }

    /// Return the kind encoded by this typed identity.
    #[must_use]
    pub const fn kind(self) -> CatalogObjectKind {
        match self {
            Self::Catalog(_) => CatalogObjectKind::Catalog,
            Self::Directory(_) => CatalogObjectKind::Directory,
            Self::Schema(_) => CatalogObjectKind::Schema,
            Self::Graph(_) => CatalogObjectKind::Graph,
            Self::GraphType(_) => CatalogObjectKind::GraphType,
            Self::BindingTable(_) => CatalogObjectKind::BindingTable,
            Self::Procedure(_) => CatalogObjectKind::Procedure,
            Self::Index(_) => CatalogObjectKind::Index,
            Self::Constraint(_) => CatalogObjectKind::Constraint,
        }
    }
}

impl fmt::Display for CatalogObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(id) => id.fmt(formatter),
            Self::Directory(id) => id.fmt(formatter),
            Self::Schema(id) => id.fmt(formatter),
            Self::Graph(id) => id.fmt(formatter),
            Self::GraphType(id) => id.fmt(formatter),
            Self::BindingTable(id) => id.fmt(formatter),
            Self::Procedure(id) => id.fmt(formatter),
            Self::Index(id) => id.fmt(formatter),
            Self::Constraint(id) => id.fmt(formatter),
        }
    }
}

/// Monotonic nonzero generation of an immutable catalog snapshot or descriptor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(transparent)]
pub struct CatalogGeneration(NonZeroU64);

impl CatalogGeneration {
    /// Construct a nonzero generation.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::ZeroGeneration`] when `raw` is zero.
    pub const fn new(raw: u64) -> CatalogResult<Self> {
        match NonZeroU64::new(raw) {
            Some(value) => Ok(Self(value)),
            None => Err(CatalogError::ZeroGeneration),
        }
    }

    /// Return the generation value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Return the checked next generation.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::GenerationOverflow`] at `u64::MAX`.
    pub const fn next(self) -> CatalogResult<Self> {
        match self.get().checked_add(1) {
            Some(raw) => Self::new(raw),
            None => Err(CatalogError::GenerationOverflow {
                current: self.get(),
            }),
        }
    }
}
