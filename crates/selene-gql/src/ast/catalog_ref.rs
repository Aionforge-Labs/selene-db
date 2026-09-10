//! Unresolved catalog object references carried by source syntax.
//!
//! The parser records the spelling of every path segment together with its
//! lexical form. Name validation (NFC canonicalisation, UAX #31 profile,
//! private-use rejection) is not repeated here: catalog-aware analysis and
//! facade lifecycle commands use the catalog's validated name constructors.

use selene_core::DbString;

use crate::ast::span::SourceSpan;

/// Lexical form of one catalog path segment (ISO/IEC 39075:2024 §21.3).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum IdentifierForm {
    /// A regular identifier; validated under the regular-identifier profile.
    Regular,
    /// A delimited identifier; its decoded spelling may contain any scalar
    /// the catalog admits, including `/`, spaces, and quote characters.
    Delimited,
}

/// One decoded path segment plus the lexical form it was written in.
#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CatalogPathSegment {
    /// Decoded spelling (delimiters stripped and doubled delimiters unescaped).
    pub name: DbString,
    /// Whether the segment was written as a regular or delimited identifier.
    pub form: IdentifierForm,
}

/// An unresolved reference to a schema or a schema-owned object.
///
/// `absolute` records the leading solidus of an ISO `<absolute directory
/// path>` or explicit `<catalog object parent reference>`. A relative
/// reference has no leading solidus and, in this profile, exactly one segment
/// that resolves against the current working schema (§17.2 SR2a). The parser
/// never resolves references; segment-count and directory-depth rules are
/// applied by catalog-aware analysis or the facade lifecycle boundary, which
/// report them as invalid references (`42002`).
#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CatalogObjectReference {
    /// Whether the reference started with `/`.
    pub absolute: bool,
    /// Path segments in source order.
    pub segments: Vec<CatalogPathSegment>,
    /// Span of the whole reference.
    pub span: SourceSpan,
}

impl CatalogObjectReference {
    /// Return the last segment, which names the referenced object itself.
    ///
    /// # Panics
    ///
    /// Panics if the reference has no segments; the grammar guarantees at
    /// least one.
    #[must_use]
    pub fn leaf(&self) -> &CatalogPathSegment {
        self.segments
            .last()
            .expect("grammar guarantees at least one path segment")
    }
}
