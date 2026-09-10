//! Analyzer type cells.

#[path = "types/expr_lookup.rs"]
mod expr_lookup;
#[path = "types/fingerprint.rs"]
mod fingerprint;

pub use expr_lookup::ExprIdLookup;

use crate::GqlType;
use selene_core::{StructuralType, StructuralTypeError, TypeKind};
use std::sync::OnceLock;

/// Stable, opaque identifier for a `ValueExpr` cell within one analyzer call.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct ExprId(u32);

impl ExprId {
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Return this expression cell's zero-based numeric index.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Type carried by an analyzed expression cell.
///
/// `Dynamic` is the explicit sink for static-inference gaps. It is not a
/// hint downstream stages can ignore; planners and executors must handle it
/// deliberately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnalyzedType {
    /// Statically resolved type.
    Resolved(GqlType),
    /// Static inference did not resolve this expression in the current pass.
    Dynamic,
}

impl AnalyzedType {
    /// Normalize a transient inference result into the structural authority.
    pub fn structural_type(&self) -> Result<StructuralType, StructuralTypeError> {
        match self {
            Self::Resolved(ty) => crate::normalize_value_type(ty),
            Self::Dynamic => Ok(StructuralType::DYNAMIC),
        }
    }
    /// Canonical dynamic type cell.
    pub const DYNAMIC: Self = Self::Dynamic;

    /// Return true when this type cell is dynamic.
    #[must_use]
    pub const fn is_dynamic(&self) -> bool {
        matches!(self, Self::Dynamic)
    }

    /// Return true when this type cell has a concrete static type.
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved(_))
    }
}

/// Side-table of inferred type cells, indexed by [`ExprId`].
#[derive(Clone, Debug, Default)]
pub struct ExprTypeTable {
    cells: Vec<TypeCell>,
}

/// Only `semantic` is authoritative. `legacy` is an immutable, lazily derived
/// current-planner view through the single F03-PR04 deletion adapter.
#[derive(Clone, Debug)]
struct TypeCell {
    semantic: StructuralType,
    legacy: OnceLock<AnalyzedType>,
}

impl TypeCell {
    fn lower(&self) -> &AnalyzedType {
        self.legacy.get_or_init(|| match self.semantic.kind() {
            TypeKind::Dynamic => AnalyzedType::Dynamic,
            _ => AnalyzedType::Resolved(crate::lower_value_type(&self.semantic)),
        })
    }
}

impl ExprTypeTable {
    pub(crate) fn push(&mut self, ty: AnalyzedType) -> Result<ExprId, StructuralTypeError> {
        let semantic = ty.structural_type()?;
        let id = ExprId::new(self.cells.len() as u32);
        self.cells.push(TypeCell {
            semantic,
            legacy: OnceLock::new(),
        });
        Ok(id)
    }

    /// Type cell for `id`.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not produced by this table's allocator; that would
    /// indicate analyzer corruption rather than user input.
    #[must_use]
    pub fn get(&self, id: ExprId) -> &AnalyzedType {
        self.cells[id.0 as usize].lower()
    }

    /// Borrow the authoritative normalized descriptor, independent of the
    /// current planner's derived source-type vocabulary.
    #[must_use]
    pub fn structural_type(&self, id: ExprId) -> &StructuralType {
        &self.cells[id.0 as usize].semantic
    }

    /// Type cell for `id`, or `None` when `id` is out of range.
    ///
    /// Use this from cross-statement or external callers where the [`ExprId`]
    /// may not have been produced by this table's allocator. The panicking
    /// [`get`](Self::get) stays the hot-path accessor for ids known to be
    /// in range. (Precedent: `SubqueryRegistry::get`.)
    #[must_use]
    pub fn try_get(&self, id: ExprId) -> Option<&AnalyzedType> {
        self.cells.get(id.0 as usize).map(TypeCell::lower)
    }

    /// Number of allocated cells.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Return true when the table has no expression cells.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Iterate over type cells in allocation order.
    pub fn iter(&self) -> impl Iterator<Item = (ExprId, &AnalyzedType)> {
        self.cells
            .iter()
            .enumerate()
            .map(|(index, ty)| (ExprId::new(index as u32), ty.lower()))
    }
}
