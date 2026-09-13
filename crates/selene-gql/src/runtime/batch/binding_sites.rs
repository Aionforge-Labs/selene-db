//! Private, selection-aligned mutation provenance in otherwise columnar batches.
//!
//! An anonymous INSERT endpoint belongs to a binding, not a value column. Read
//! calls/filter/page may preserve that binding between mutation barriers. The
//! sidecar is absent for ordinary query rows, never exported, and discarded by
//! projections/grouping that deliberately construct fresh bindings.

use super::{BatchError, BindingBatch};
use crate::runtime::Binding;

impl BindingBatch {
    pub(crate) fn with_binding_sites(mut self, rows: &[Binding]) -> Result<Self, BatchError> {
        if self.columns.is_empty() && self.selection.is_none() {
            self.logical_rows = rows.len();
        }
        if self.selection.is_some() || self.logical_rows != rows.len() {
            return Err(BatchError::ColumnLengthMismatch);
        }
        if rows.iter().any(|row| !row.insert_sites().is_empty()) {
            self.insert_sites = rows.iter().map(Binding::cloned_insert_sites).collect();
        }
        Ok(self)
    }

    pub(crate) fn logical_binding(&self, logical: usize) -> Binding {
        let physical = self
            .selection
            .as_ref()
            .map_or(logical, |selection| selection[logical].index());
        Binding::with_insert_sites(
            self.logical_row(logical),
            self.insert_sites.get(physical).cloned().unwrap_or_default(),
        )
    }

    pub(super) fn insert_site_bytes(&self) -> usize {
        self.insert_sites
            .capacity()
            .saturating_mul(std::mem::size_of::<
                smallvec::SmallVec<[(crate::InsertSiteId, selene_core::NodeId); 4]>,
            >())
            .saturating_add(
                self.insert_sites
                    .iter()
                    .filter(|sites| sites.spilled())
                    .map(|sites| {
                        sites.capacity().saturating_mul(std::mem::size_of::<(
                            crate::InsertSiteId,
                            selene_core::NodeId,
                        )>())
                    })
                    .sum::<usize>(),
            )
    }
}
