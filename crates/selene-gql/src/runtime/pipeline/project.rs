//! Declared projection schema, including empty results.
use crate::{BindingTableColumn, BindingTableSchema, ProjectExpr};

pub(crate) fn schema_for_items(items: &[ProjectExpr]) -> BindingTableSchema {
    BindingTableSchema {
        columns: items
            .iter()
            .map(|item| BindingTableColumn {
                name: item.alias.clone(),
                hidden: None,
                ty: item.ty.clone(),
            })
            .collect(),
    }
}
