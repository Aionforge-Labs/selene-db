//! Foundation types for the selene-db ISO/IEC 39075:2024 GQL property graph
//! engine.
//!
//! This crate is the dependency root: every other selene crate transitively
//! depends on it. Per D8, `selene-core` has zero dependencies on other selene
//! crates. The mandatory data types of ISO GQL minimum conformance live here
//! (`STRING`, `BOOLEAN`, `INT`, `FLOAT`); composite, temporal, and reference
//! value types are also normatively defined in Spec 02 and implemented here.
//! The crate now also carries the composite containers,
//! schema model, origin metadata, and WAL change payload
//! types needed by downstream crates.
//!
//! See Spec 02 for the full data model specification.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod byte_string_type;
pub mod cancellation;
pub mod change_kind;
pub mod changeset;
mod changeset_variants;
mod character_string_type;
pub mod db_string;
mod decimal_type;
mod duration_type;
mod edge;
pub mod error;
pub mod extension_type_ids;
pub mod gqlstatus;
pub mod hlc;
pub mod identity;
mod json_patch;
mod json_value;
pub mod label_set;
pub mod metrics;
mod numeric_value;
pub mod origin;
pub mod property_map;
mod property_structural_type;
pub mod property_value_type;
pub mod reserved;
mod result_order;
pub mod schema;
mod stored_value;
mod structural_match;
mod structural_type;
mod type_assignment;
mod type_comparison;
pub mod value;
mod value_comparison;
pub mod vector;
pub mod vector_index;

pub use byte_string_type::{ByteStringType, MAX_BYTE_STRING_TYPE_LENGTH, byte_string_fits_type};
pub use cancellation::{CancellationCause, CancellationChecker, CancellationToken, NodeScanBudget};
pub use change_kind::ChangeKind;
pub use changeset::{
    Change, LabelDiff, PropertyDiff, SchemaChange, SchemaPropertyIndexKind, SchemaVectorIndexKind,
};
pub use character_string_type::{
    CharacterStringCoercionError, CharacterStringType, MAX_CHARACTER_STRING_TYPE_LENGTH,
    character_string_fits_type, coerce_character_string_to_type, is_truncating_whitespace,
};
pub use db_string::{DbString, db_string};
pub use decimal_type::{
    DecimalType, MAX_DECIMAL_PRECISION, MAX_DECIMAL_SCALE, decimal_fits_type, round_decimal_to_type,
};
pub use duration_type::{
    DurationOrderKey, DurationTypeQualifier, DurationValueFamily, duration_keys_comparable,
    duration_order_key, duration_value_family,
};
pub use edge::{EdgeDirectionality, EdgeRecordV1};
pub use error::{CoreError, CoreResult};
pub use extension_type_ids::{
    ExtensionTypeId, FIRST_PARTY_EXTENSION_TYPE_IDS, SELENE_RDF, SELENE_TIMESERIES,
};
pub use gqlstatus::{ALL_GQLSTATUS_NAMES, gqlstatus_name};
pub use hlc::HlcTimestamp;
pub use identity::{BindingTableId, EdgeId, GraphId, NodeId, RecordTypeId};
pub use json_value::{JsonPathSelector, JsonValue, JsonValueRef};
pub use label_set::LabelSet;
pub use numeric_value::NumericKey;
pub use origin::Origin;
pub use property_map::{PropertyMap, PropertyMapIter, PropertyMapKeys, PropertyMapValues};
pub use property_value_type::PropertyValueType;
pub use reserved::RESERVED_LABEL_PREFIX;
pub use result_order::{NullPlacement, ResultOrderKey, SortDirection};
/// Exact decimal payload shared by supported query APIs.
pub use rust_decimal::Decimal;
pub use schema::{
    EdgeEndpointDef, EdgeTypeDef, EdgeTypeDefV1, GraphType, GraphTypeId, KeyLabelSetPolicy,
    NodeKey, NodeTypeDef, NodeTypeDefV1, NodeTypeRef, PredefinedValueType, PropertyDef,
    PropertyDefV1, RecordFieldStructure, RecordFieldStructureDef, RecordFieldStructureType,
    RecordTypeDef, RecordTypeRef, ValidationMode, ValueType, ValueTypeCardinality,
};
pub use stored_value::{MAX_STORED_VALUE_DEPTH, StoredValue, StoredValueError};
pub use structural_type::{
    MAX_STRUCTURAL_TYPE_DEPTH, ScalarType, StructuralType, StructuralTypeError, TypeKind,
};
pub use type_comparison::ComparisonMode;
/// UUID payload shared by supported query APIs.
pub use uuid::Uuid;
pub use value::{
    EdgeDirection, MAX_VECTOR_DIMENSION, Path, PathSegment, Record, RecordTyped, Value, VectorValue,
};
pub use value_comparison::{ValueComparisonDomain, ValueComparisonError, comparison_leaf_type};
pub use vector::{
    TURBO_QUANT_BLOCK_ROWS, TurboQuantBitWidth, TurboQuantBlockedCodes, TurboQuantCodebook,
    TurboQuantCodebookKind, TurboQuantCodecError, TurboQuantCodecResult, TurboQuantPackedCodes,
    VectorMetric, VectorMetricQuery, VectorSearchHit, VectorTopK, exact_vector_top_k,
    vector_squared_norm,
};
pub use vector_index::{HnswIndexConfig, IvfIndexConfig};

#[cfg(test)]
mod serde_tests;
