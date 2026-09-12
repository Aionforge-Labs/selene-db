//! Mutation-pipeline helpers for logical lowering.

use crate::{
    SourceSpan,
    analyze::{AnalyzedType, BindingDeclKind},
    plan::{
        BindingTableColumn, BindingTableSchema, PlannerError, logical::lowering::LogicalBuilder,
    },
};

impl<'a, 'r> LogicalBuilder<'a, 'r> {
    /// Conservative mutation descriptor from the analyzer write set.
    pub(crate) fn mutation_descriptor(
        &self,
        span: SourceSpan,
    ) -> Result<crate::plan::logical::descriptors::LogicalMutationDescriptor, PlannerError> {
        let Some(write_set) = self.analyzed.write_set.as_ref() else {
            return Err(PlannerError::WriteSetMissing { span });
        };
        let mut inserts_node = false;
        let mut inserts_edge = false;
        let mut updates_graph = false;
        let mut deletes_target = false;
        for entry in &write_set.entries {
            match &entry.kind {
                crate::WriteKind::InsertNode { .. } => inserts_node = true,
                crate::WriteKind::InsertEdge { .. } => inserts_edge = true,
                crate::WriteKind::SetProperty { .. }
                | crate::WriteKind::SetLabel { .. }
                | crate::WriteKind::RemoveProperty { .. }
                | crate::WriteKind::RemoveLabel { .. } => updates_graph = true,
                crate::WriteKind::DeleteTarget { .. } => deletes_target = true,
            }
        }
        Ok(
            crate::plan::logical::descriptors::LogicalMutationDescriptor {
                write_entry_count: write_set.entries.len(),
                inserts_node,
                inserts_edge,
                updates_graph,
                deletes_target,
                origin: span,
            },
        )
    }

    /// Mutation output columns: projection aliases visible after the boundary.
    pub(crate) fn projection_schema(&self) -> BindingTableSchema {
        let mut columns = Vec::new();
        for decl in self.analyzed.scopes.declarations() {
            match decl.kind() {
                BindingDeclKind::ProjectionAlias | BindingDeclKind::YieldColumn => {
                    columns.push(BindingTableColumn {
                        name: Some(decl.name()),
                        hidden: None,
                        ty: decl.ty().clone(),
                    });
                }
                BindingDeclKind::NodePattern
                | BindingDeclKind::EdgePattern
                | BindingDeclKind::LetAlias
                | BindingDeclKind::ForAlias
                | BindingDeclKind::InsertNode
                | BindingDeclKind::InsertEdge
                | BindingDeclKind::PathBinding => {}
            }
        }
        if columns.is_empty() {
            self.schema()
        } else {
            BindingTableSchema { columns }
        }
    }

    /// Resolve a projection alias to its semantic type, defaulting to dynamic.
    pub(crate) fn binding_type_for_alias(&self, name: &selene_core::DbString) -> AnalyzedType {
        self.analyzed
            .scopes
            .declarations()
            .iter()
            .find(|decl| decl.name() == *name)
            .map(|decl| decl.ty().clone())
            .unwrap_or(AnalyzedType::Dynamic)
    }
}
