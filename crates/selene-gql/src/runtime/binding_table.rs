//! Row-at-a-time binding table representation.

use smallvec::SmallVec;

use selene_core::{DbString, NodeId, Value};

use crate::plan::{BindingTableSchema, InsertSiteId};

/// Owned row storage: ordered values plus the insert sites the row carries.
type BindingParts = (SmallVec<[Value; 8]>, SmallVec<[(InsertSiteId, NodeId); 4]>);

/// One executor binding-table row.
#[derive(Clone, Debug)]
pub struct Binding {
    values: SmallVec<[Value; 8]>,
    insert_sites: SmallVec<[(InsertSiteId, NodeId); 4]>,
}

impl Binding {
    /// Construct a row from ordered values.
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = Value>) -> Self {
        Self {
            values: values.into_iter().collect(),
            insert_sites: SmallVec::new(),
        }
    }

    /// Construct an empty row.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            values: SmallVec::new(),
            insert_sites: SmallVec::new(),
        }
    }

    pub(crate) fn with_insert_sites(
        values: impl IntoIterator<Item = Value>,
        insert_sites: SmallVec<[(InsertSiteId, NodeId); 4]>,
    ) -> Self {
        Self {
            values: values.into_iter().collect(),
            insert_sites,
        }
    }

    pub(crate) fn from_parts(
        values: SmallVec<[Value; 8]>,
        insert_sites: SmallVec<[(InsertSiteId, NodeId); 4]>,
    ) -> Self {
        Self {
            values,
            insert_sites,
        }
    }

    /// Borrow the row's values.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Borrow one value by column index.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }

    pub(crate) fn into_parts(self) -> BindingParts {
        (self.values, self.insert_sites)
    }

    pub(crate) fn insert_sites(&self) -> &[(InsertSiteId, NodeId)] {
        &self.insert_sites
    }

    pub(crate) fn cloned_values(&self) -> SmallVec<[Value; 8]> {
        self.values.clone()
    }

    pub(crate) fn cloned_insert_sites(&self) -> SmallVec<[(InsertSiteId, NodeId); 4]> {
        self.insert_sites.clone()
    }

    pub(crate) fn inserted_node(&self, site_id: InsertSiteId) -> Option<NodeId> {
        self.insert_sites
            .iter()
            .find_map(|(site, id)| (*site == site_id).then_some(*id))
    }

    pub(crate) fn with_appended_values(&self, values: impl IntoIterator<Item = Value>) -> Self {
        let mut output = self.values.clone();
        output.extend(values);
        Self {
            values: output,
            insert_sites: self.insert_sites.clone(),
        }
    }
}

impl PartialEq for Binding {
    fn eq(&self, rhs: &Self) -> bool {
        self.values == rhs.values
    }
}

/// Executor binding table with schema and row storage.
#[derive(Clone, Debug, PartialEq)]
pub struct BindingTable {
    schema: BindingTableSchema,
    rows: Vec<Binding>,
    ordering: Vec<selene_core::ResultOrderKey>,
}

impl BindingTable {
    /// Construct an empty table with the supplied schema.
    #[must_use]
    pub fn empty(schema: BindingTableSchema) -> Self {
        Self {
            schema,
            rows: Vec::new(),
            ordering: Vec::new(),
        }
    }

    /// Construct the relational unit table: one empty row and no fields.
    #[must_use]
    pub fn unit() -> Self {
        Self {
            schema: BindingTableSchema {
                columns: Vec::new(),
            },
            rows: vec![Binding::empty()],
            ordering: Vec::new(),
        }
    }

    /// Construct a table from a schema and row vector.
    #[must_use]
    pub fn new(schema: BindingTableSchema, rows: Vec<Binding>) -> Self {
        Self {
            schema,
            rows,
            ordering: Vec::new(),
        }
    }

    /// Borrow the table schema.
    #[must_use]
    pub const fn schema(&self) -> &BindingTableSchema {
        &self.schema
    }

    /// Declared global result ordering, retained independently of row count.
    #[must_use]
    pub fn ordering(&self) -> &[selene_core::ResultOrderKey] {
        &self.ordering
    }

    pub(crate) fn declare_result_order(&mut self, plan: &crate::ExecutionPlan) {
        self.ordering = super::result_order::for_plan(plan, &self.schema);
    }

    /// Borrow all rows.
    #[must_use]
    pub fn rows(&self) -> &[Binding] {
        &self.rows
    }

    pub(crate) fn into_parts(self) -> (BindingTableSchema, Vec<Binding>) {
        (self.schema, self.rows)
    }

    /// Iterate rows in storage order.
    pub fn iter(&self) -> impl Iterator<Item = &Binding> {
        self.rows.iter()
    }

    /// Return the number of rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Return true when the table has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Return the index of the first named column matching `name`.
    #[must_use]
    pub fn column_index(&self, name: DbString) -> Option<usize> {
        self.schema
            .columns
            .iter()
            .position(|column| column.name == Some(name.clone()))
    }
}
