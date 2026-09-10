//! Normalized, owned semantic value types. No source spelling, arena ID, or
//! process-global intern pool is part of a descriptor's identity.

use std::sync::Arc;

use crate::{ByteStringType, CharacterStringType, DbString, DecimalType, DurationTypeQualifier};

/// Maximum nesting of a structural descriptor, including its root.
pub const MAX_STRUCTURAL_TYPE_DEPTH: usize = 256;

/// One selected scalar representation, without source-language synonyms.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ScalarType {
    /// Boolean.
    Boolean,
    /// Signed 8-bit envelope over the signed integer representation.
    Int8,
    /// Signed 16-bit envelope.
    Int16,
    /// Signed 32-bit envelope.
    Int32,
    /// Signed 64-bit integer.
    Int64,
    /// Signed 128-bit integer.
    Int128,
    /// Unsigned 8-bit envelope.
    Uint8,
    /// Unsigned 16-bit envelope.
    Uint16,
    /// Unsigned 32-bit envelope.
    Uint32,
    /// Unsigned 64-bit integer.
    Uint64,
    /// Unsigned 128-bit integer.
    Uint128,
    /// Either supported binary floating-point representation.
    Float,
    /// IEEE binary32.
    Float32,
    /// IEEE binary64.
    Float64,
    /// Decimal, optionally constrained by precision and scale.
    Decimal(Option<DecimalType>),
    /// Character string, optionally constrained by character length.
    String(Option<CharacterStringType>),
    /// Byte string, optionally constrained by byte length.
    Bytes(Option<ByteStringType>),
    /// UUID.
    Uuid,
    /// Native canonical JSON; equality-only, not order-comparable.
    Json,
    /// Native finite nonempty dense vector.
    Vector,
    /// Civil date.
    Date,
    /// Local datetime.
    LocalDateTime,
    /// Zoned datetime.
    ZonedDateTime,
    /// Local time.
    LocalTime,
    /// Zoned time.
    ZonedTime,
    /// Duration, optionally restricted to one unit group.
    Duration(Option<DurationTypeQualifier>),
}

/// A structural type's non-null family. Fields are normalized by
/// [`StructuralType::new`], not interpreted as source type spellings.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TypeKind {
    /// Explicit analysis uncertainty, not admission of optional `ANY` syntax.
    Dynamic,
    /// Implementation-supported property values, recursively checked.
    Property,
    /// A selected scalar representation.
    Scalar(ScalarType),
    /// List element descriptor and optional maximum cardinality.
    List {
        /// Element type, including element nullability.
        element: Box<StructuralType>,
        /// Maximum cardinality; `None` is unbounded.
        max_len: Option<u64>,
    },
    /// Record fields, sorted by exact field name; `None` is an open record.
    Record(Option<Arc<[(DbString, StructuralType)]>>),
    /// Closed set of non-null component types; nullability is carried once by
    /// the containing descriptor.
    Union(Arc<[StructuralType]>),
    /// Query-only node reference.
    NodeRef,
    /// Query-only edge reference.
    EdgeRef,
    /// Query-only path.
    Path,
    /// Query-only graph reference; does not select optional graph type syntax.
    GraphRef,
    /// Query-only table reference; optional named field descriptor.
    TableRef(Option<Arc<[(DbString, StructuralType)]>>),
    /// The null value only, for analysis, not optional source-type admission.
    Null,
    /// No values, for analysis, not optional source-type admission.
    Empty,
}

/// A normalized structural descriptor with explicit nullability.
///
/// Descriptors own their recursive structure. Cloning shares named field sets;
/// dropping a query or database releases its descriptors. Equality and hashing
/// concern type identity, not value predicate equality or grouping.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StructuralType {
    kind: TypeKind,
    nullable: bool,
}

/// Invalid structural descriptor construction.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StructuralTypeError {
    /// Repeated exact field name.
    #[error("duplicate structural field {0}")]
    DuplicateField(DbString),
    /// Invalid scalar bounds.
    #[error("invalid structural scalar bounds")]
    InvalidBounds,
    /// Excessively nested descriptor.
    #[error("structural descriptor exceeds {MAX_STRUCTURAL_TYPE_DEPTH} levels")]
    DepthLimit,
    /// A source type belongs to an unsupported optional capability.
    #[error("unsupported value type capability {0}")]
    Unsupported(&'static str),
}

impl StructuralType {
    /// Nullable boolean.
    pub const BOOLEAN: Self = Self::scalar(ScalarType::Boolean);
    /// Nullable signed 64-bit integer (also the default integer type).
    pub const INT64: Self = Self::scalar(ScalarType::Int64);
    /// Nullable signed 128-bit integer.
    pub const INT128: Self = Self::scalar(ScalarType::Int128);
    /// Nullable unsigned 64-bit integer.
    pub const UINT64: Self = Self::scalar(ScalarType::Uint64);
    /// Nullable binary64 float.
    pub const FLOAT64: Self = Self::scalar(ScalarType::Float64);
    /// Nullable unconstrained character string.
    pub const STRING: Self = Self::scalar(ScalarType::String(None));
    /// Nullable native JSON.
    pub const JSON: Self = Self::scalar(ScalarType::Json);
    /// Nullable native vector.
    pub const VECTOR: Self = Self::scalar(ScalarType::Vector);
    /// Nullable query-only node reference.
    pub const NODE: Self = Self {
        kind: TypeKind::NodeRef,
        nullable: true,
    };
    /// Nullable query-only edge reference.
    pub const EDGE: Self = Self {
        kind: TypeKind::EdgeRef,
        nullable: true,
    };
    /// Nullable query-only path.
    pub const PATH: Self = Self {
        kind: TypeKind::Path,
        nullable: true,
    };
    /// Explicit unresolved analysis type.
    pub const DYNAMIC: Self = Self {
        kind: TypeKind::Dynamic,
        nullable: true,
    };
    /// Null-only analysis type.
    pub const NULL: Self = Self {
        kind: TypeKind::Null,
        nullable: true,
    };
    /// Empty analysis type.
    pub const EMPTY: Self = Self {
        kind: TypeKind::Empty,
        nullable: false,
    };

    const fn scalar(scalar: ScalarType) -> Self {
        Self {
            kind: TypeKind::Scalar(scalar),
            nullable: true,
        }
    }

    /// Validate and normalize a kind, preserving exact field names.
    pub fn new(mut kind: TypeKind, nullable: bool) -> Result<Self, StructuralTypeError> {
        match &mut kind {
            TypeKind::List {
                max_len: Some(0), ..
            } => return Err(StructuralTypeError::InvalidBounds),
            TypeKind::Union(members) => {
                return Self::union(members.iter().cloned())
                    .map(|ty| ty.with_nullability(nullable));
            }
            TypeKind::Record(Some(fields)) | TypeKind::TableRef(Some(fields)) => {
                let mut sorted = fields.to_vec();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                if let Some(pair) = sorted.windows(2).find(|pair| pair[0].0 == pair[1].0) {
                    return Err(StructuralTypeError::DuplicateField(pair[0].0.clone()));
                }
                *fields = sorted.into();
            }
            TypeKind::Scalar(ScalarType::String(Some(bounds)))
                if CharacterStringType::new(bounds.min_len, bounds.max_len).is_none() =>
            {
                return Err(StructuralTypeError::InvalidBounds);
            }
            TypeKind::Scalar(ScalarType::Bytes(Some(bounds)))
                if ByteStringType::new(bounds.min_len, bounds.max_len).is_none() =>
            {
                return Err(StructuralTypeError::InvalidBounds);
            }
            _ => {}
        }
        let result = Self { kind, nullable }.with_nullability(nullable);
        if result.depth() > MAX_STRUCTURAL_TYPE_DEPTH {
            return Err(StructuralTypeError::DepthLimit);
        }
        Ok(result)
    }

    /// Construct a nullable scalar descriptor, validating any bounds.
    pub fn from_scalar(scalar: ScalarType) -> Result<Self, StructuralTypeError> {
        Self::new(TypeKind::Scalar(scalar), true)
    }

    /// Construct a nullable list with an optional cardinality envelope.
    pub fn list(element: Self, max_len: Option<u64>) -> Result<Self, StructuralTypeError> {
        Self::new(
            TypeKind::List {
                element: Box::new(element),
                max_len,
            },
            true,
        )
    }

    /// Construct a nullable closed record, independent of input field order.
    pub fn record(
        fields: impl IntoIterator<Item = (DbString, Self)>,
    ) -> Result<Self, StructuralTypeError> {
        Self::new(TypeKind::Record(Some(fields.into_iter().collect())), true)
    }

    /// Normalize a closed union without replacing its component information by
    /// a dynamic catch-all. Source capability admission remains separate.
    pub fn union(members: impl IntoIterator<Item = Self>) -> Result<Self, StructuralTypeError> {
        let mut components = Vec::new();
        let mut nullable = false;
        for member in members {
            nullable |= member.is_nullable();
            match member.kind() {
                TypeKind::Dynamic => {
                    return Err(StructuralTypeError::Unsupported("dynamic union component"));
                }
                TypeKind::Null | TypeKind::Empty => {}
                TypeKind::Union(inner) => components.extend(inner.iter().cloned()),
                _ => components.push(member.with_nullability(false)),
            }
        }
        // Type-only structural text contains no runtime addresses or arena IDs.
        // This is an internal canonical set order, not a persisted encoding or
        // an ordering operation over values.
        components.sort_by_cached_key(|ty| format!("{ty:?}"));
        components.dedup();
        let kind = match components.len() {
            0 => return Ok(Self::EMPTY.with_nullability(nullable)),
            1 => return Ok(components.remove(0).with_nullability(nullable)),
            _ => TypeKind::Union(components.into()),
        };
        let result = Self { kind, nullable };
        if result.depth() > MAX_STRUCTURAL_TYPE_DEPTH {
            return Err(StructuralTypeError::DepthLimit);
        }
        Ok(result)
    }

    /// Borrow the canonical kind.
    #[must_use]
    pub const fn kind(&self) -> &TypeKind {
        &self.kind
    }

    /// Whether null belongs to this type.
    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    /// Change nullability without adding wrappers. Null-only and empty types
    /// normalize to each other when their nullability changes.
    #[must_use]
    pub fn with_nullability(mut self, nullable: bool) -> Self {
        if matches!(self.kind, TypeKind::Null | TypeKind::Empty) {
            return if nullable { Self::NULL } else { Self::EMPTY };
        }
        self.nullable = nullable;
        self
    }

    /// Maximum nesting including this descriptor.
    #[must_use]
    pub fn depth(&self) -> usize {
        1 + match &self.kind {
            TypeKind::List { element, .. } => element.depth(),
            TypeKind::Union(members) => members.iter().map(Self::depth).max().unwrap_or(0),
            TypeKind::Record(Some(fields)) | TypeKind::TableRef(Some(fields)) => {
                fields.iter().map(|(_, ty)| ty.depth()).max().unwrap_or(0)
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
#[path = "structural_type_tests.rs"]
mod tests;
