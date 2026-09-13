use selene_core::{Change, DbString, EdgeId, NodeId, PropertyDiff};

use super::{
    Mutator, reject_immutable_edge_update, reject_immutable_node_update, remove_index_row,
};
use crate::GraphResult;

impl<'tx, 'g> Mutator<'tx, 'g> {
    /// Remove a property from an alive node and emit `Change::NodePropertyRemoved`.
    ///
    /// The mutation is a no-op when `property` is absent.
    pub fn remove_node_property(&mut self, id: NodeId, property: DbString) -> GraphResult<()> {
        let row = self.require_live_node(id)?;
        let labels = self
            .txn
            .read()
            .node_store
            .labels
            .get(row.index())
            .cloned()
            .unwrap_or_default();
        let old_props = self
            .txn
            .read()
            .node_store
            .properties
            .get(row.index())
            .cloned()
            .unwrap_or_default();
        if !old_props.contains_key(&property) {
            return Ok(());
        }

        let diff = PropertyDiff::new([], [property.clone()])?;
        reject_immutable_node_update(self.txn.read(), id, &labels, &diff)?;
        let mut new_props = old_props.clone();
        new_props.remove(&property);
        {
            let graph = self.txn.guard_mut();
            graph
                .node_store
                .properties
                .set(row.index(), new_props.clone());
            crate::property_index::apply_node_update(
                &mut graph.property_index,
                &labels,
                &old_props,
                &labels,
                &new_props,
                row.get(),
            )?;
            crate::composite_property_index::apply_node_update(
                &mut graph.composite_property_index,
                &labels,
                &old_props,
                &labels,
                &new_props,
                row.get(),
            )?;
            crate::vector_index::apply_node_update(
                &mut graph.vector_index,
                &labels,
                &old_props,
                &labels,
                &new_props,
                row.get(),
            )?;
            crate::text_index::apply_node_update(
                &mut graph.text_index,
                &labels,
                &old_props,
                &labels,
                &new_props,
                row.get(),
                id,
            );
        }
        self.txn
            .changes
            .push(Change::NodePropertyRemoved { id, property });
        Ok(())
    }

    /// Remove a property from an alive edge and emit `Change::EdgePropertyRemoved`.
    ///
    /// The mutation is a no-op when `property` is absent.
    pub fn remove_edge_property(&mut self, id: EdgeId, property: DbString) -> GraphResult<()> {
        let row = self.require_live_edge(id)?;
        let label = self
            .txn
            .read()
            .edge_store
            .label
            .get(row.index())
            .cloned()
            .ok_or(crate::GraphError::EdgeNotFound { id })?;
        let old_props = self
            .txn
            .read()
            .edge_store
            .properties
            .get(row.index())
            .cloned()
            .unwrap_or_default();
        if !old_props.contains_key(&property) {
            return Ok(());
        }

        let diff = PropertyDiff::new([], [property.clone()])?;
        reject_immutable_edge_update(self.txn.read(), id, &diff)?;
        let mut new_props = old_props.clone();
        new_props.remove(&property);
        {
            let graph = self.txn.guard_mut();
            graph
                .edge_store
                .properties
                .set(row.index(), new_props.clone());
            crate::property_index::apply_edge_update(
                &mut graph.edge_property_index,
                &label,
                &old_props,
                &new_props,
                row.get(),
            )?;
        }
        self.txn
            .changes
            .push(Change::EdgePropertyRemoved { id, property });
        Ok(())
    }

    /// Remove a label from an alive node and emit `Change::NodeLabelRemoved`.
    ///
    /// The mutation is a no-op when `label` is absent.
    pub fn remove_node_label(&mut self, id: NodeId, label: DbString) -> GraphResult<()> {
        let row = self.require_live_node(id)?;
        let old_labels = self
            .txn
            .read()
            .node_store
            .labels
            .get(row.index())
            .cloned()
            .unwrap_or_default();
        if !old_labels.contains(&label) {
            return Ok(());
        }
        let props = self
            .txn
            .read()
            .node_store
            .properties
            .get(row.index())
            .cloned()
            .unwrap_or_default();
        let mut new_labels = old_labels.clone();
        new_labels.remove(&label);
        {
            let graph = self.txn.guard_mut();
            graph.node_store.labels.set(row.index(), new_labels.clone());
            remove_index_row(&mut graph.idx_label, &label, row.get());
            crate::property_index::apply_node_update(
                &mut graph.property_index,
                &old_labels,
                &props,
                &new_labels,
                &props,
                row.get(),
            )?;
            crate::composite_property_index::apply_node_update(
                &mut graph.composite_property_index,
                &old_labels,
                &props,
                &new_labels,
                &props,
                row.get(),
            )?;
            crate::vector_index::apply_node_update(
                &mut graph.vector_index,
                &old_labels,
                &props,
                &new_labels,
                &props,
                row.get(),
            )?;
            crate::text_index::apply_node_update(
                &mut graph.text_index,
                &old_labels,
                &props,
                &new_labels,
                &props,
                row.get(),
                id,
            );
        }
        self.txn
            .changes
            .push(Change::NodeLabelRemoved { id, label });
        Ok(())
    }
}
