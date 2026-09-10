//! Analyzed native index declarations. Target keys are not catalog identifiers.

use selene_core::{
    HnswIndexConfig, IvfIndexConfig, SchemaPropertyIndexKind, SchemaVectorIndexKind,
};
use serde::{Deserialize, Serialize};

use crate::{CatalogError, CatalogResult, DeclarationMetadata};

/// Element family selected by an index or constraint target.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ElementKind {
    /// Nodes.
    Node,
    /// Edges.
    Edge,
}

/// Computed native index family, independent of element kind or scalar arity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IndexFamily {
    /// Scalar or composite property index.
    Property,
    /// Vector index.
    Vector,
    /// Text index.
    Text,
}

/// Reserved pure scalar expression shapes for F05-PR06 analysis. These are data,
/// not executable GQL strings or closures; no expression can be registered yet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ReservedIndexExpression {
    /// Scalar property selection.
    Property(String),
    /// Bounded native JSON scalar selection with an explicit result family.
    JsonScalarPath {
        /// Exact engine property key.
        property: String,
        /// Typed JSON selectors, not JSONPath text.
        selectors: Vec<IndexJsonSelector>,
        /// Required scalar result family.
        result: SchemaPropertyIndexKind,
    },
}

/// One reserved JSON scalar-expression selector.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum IndexJsonSelector {
    /// Object key.
    Key(String),
    /// Signed array position, including negative-from-end positions.
    Index(i64),
}

/// Already-analyzed property keys in declaration order (not sorted lookup order).
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyTarget {
    /// Element family.
    pub element: ElementKind,
    /// Exact engine label; never regular-identifier folded.
    pub label: String,
    /// Exact engine property keys in declaration order.
    pub properties: Vec<String>,
}

impl PropertyTarget {
    pub(crate) fn validate(&self) -> CatalogResult<()> {
        if self.label.is_empty()
            || self.label.len() > selene_core::db_string::MAX_DB_STRING_BYTES
            || self.properties.is_empty()
            || self.properties.iter().any(|key| {
                key.is_empty() || key.len() > selene_core::db_string::MAX_DB_STRING_BYTES
            })
        {
            return Err(CatalogError::InvalidDeclaration {
                reason: "invalid_property_target",
            });
        }
        let keys: std::collections::BTreeSet<_> = self.properties.iter().collect();
        if keys.len() != self.properties.len() {
            return Err(CatalogError::InvalidDeclaration {
                reason: "duplicate_target_property",
            });
        }
        Ok(())
    }
}

/// Storage-neutral native index configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum IndexConfiguration {
    /// Exact scalar kinds, one per target component in declaration order.
    Property(Vec<SchemaPropertyIndexKind>),
    /// Native vector registration, without trained centroids or HNSW links.
    Vector {
        /// Algorithm/metric selection.
        kind: SchemaVectorIndexKind,
        /// Nonzero vector dimension.
        dimension: u32,
        /// HNSW construction parameters for HNSW kinds only.
        hnsw: Option<HnswIndexConfig>,
        /// Optional IVF construction parameters for IVF kinds only.
        ivf: Option<IvfIndexConfig>,
    },
    /// Current native BM25 tokenizer/scorer semantics are pinned by the profile coordinate.
    Text,
}

impl IndexConfiguration {
    /// Return the native implementation family of this configuration.
    #[must_use]
    pub const fn family(&self) -> IndexFamily {
        match self {
            Self::Property(_) => IndexFamily::Property,
            Self::Vector { .. } => IndexFamily::Vector,
            Self::Text => IndexFamily::Text,
        }
    }
}

/// Existing canonical generated index spelling. Property declaration order is
/// preserved; this is not the sorted physical composite lookup key.
#[must_use]
pub fn generated_index_name<'a>(
    family: IndexFamily,
    label: &str,
    properties: impl ExactSizeIterator<Item = &'a str>,
) -> String {
    use std::fmt::Write as _;
    let prefix = match family {
        IndexFamily::Property => "idx",
        IndexFamily::Vector => "vidx",
        IndexFamily::Text => "tidx",
    };
    let mut name = format!("{prefix}:{}:{label}", label.len());
    if properties.len() > 1 {
        write!(name, ":c{}", properties.len()).expect("writing to a string");
    }
    for property in properties {
        write!(name, ":{}:{property}", property.len()).expect("writing to a string");
    }
    name
}

/// Explicit composite spelling, preserving `cN` even for diagnostic views of
/// lower registrations that are not admitted as catalog composite targets.
#[must_use]
pub fn generated_composite_index_name<'a>(
    label: &str,
    properties: impl ExactSizeIterator<Item = &'a str>,
) -> String {
    use std::fmt::Write as _;
    let mut name = format!("idx:{}:{label}:c{}", label.len(), properties.len());
    for property in properties {
        write!(name, ":{}:{property}", property.len()).expect("writing to a string");
    }
    name
}

/// Logical index declaration. Runtime usability requires a matching complete binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDeclaration {
    /// Profile, lifecycle, and shared dependencies.
    pub metadata: DeclarationMetadata,
    /// Analyzed target.
    pub target: PropertyTarget,
    /// Native configuration.
    pub configuration: IndexConfiguration,
}

impl IndexDeclaration {
    /// Reject expression activation until analyzed execution is implemented by
    /// F05-PR06. A syntactically typed expression is not an analyzed proof.
    pub fn for_expression(_expression: ReservedIndexExpression) -> CatalogResult<Self> {
        Err(CatalogError::InvalidDeclaration {
            reason: "unsupported_expression_target",
        })
    }

    pub(crate) fn validate(&self) -> CatalogResult<()> {
        self.metadata.validate()?;
        self.target.validate()?;
        let arity = self.target.properties.len();
        let valid = match &self.configuration {
            IndexConfiguration::Property(kinds) => {
                kinds.len() == arity && (arity == 1 || self.target.element == ElementKind::Node)
            }
            IndexConfiguration::Text => arity == 1 && self.target.element == ElementKind::Node,
            IndexConfiguration::Vector {
                kind,
                dimension,
                hnsw,
                ivf,
            } => {
                let is_hnsw = matches!(
                    kind,
                    SchemaVectorIndexKind::HnswSquaredEuclidean
                        | SchemaVectorIndexKind::HnswCosine
                        | SchemaVectorIndexKind::HnswNegativeInnerProduct
                );
                let is_ivf = matches!(
                    kind,
                    SchemaVectorIndexKind::IvfSquaredEuclidean
                        | SchemaVectorIndexKind::IvfCosine
                        | SchemaVectorIndexKind::IvfNegativeInnerProduct
                );
                arity == 1
                    && self.target.element == ElementKind::Node
                    && (1..=u16::MAX as u32).contains(dimension)
                    && is_hnsw == hnsw.is_some()
                    && (ivf.is_none() || is_ivf)
                    && hnsw.is_none_or(|config| {
                        (1..=HnswIndexConfig::MAX_NEIGHBORS).contains(&config.max_neighbors)
                            && config.ef_construction >= config.max_neighbors
                            && config.ef_construction <= HnswIndexConfig::MAX_EF_CONSTRUCTION
                    })
                    && ivf.is_none_or(|config| {
                        (1..=IvfIndexConfig::MAX_TARGET_CENTROIDS)
                            .contains(&config.target_centroids)
                    })
            }
        };
        if !valid {
            return Err(CatalogError::InvalidDeclaration {
                reason: "incompatible_index_configuration",
            });
        }
        Ok(())
    }
}
