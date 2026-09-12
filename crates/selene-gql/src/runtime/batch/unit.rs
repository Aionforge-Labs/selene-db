//! Single-row seed sources for batch execution.
//!
//! [`BatchSeedRow`] serves exactly one logical row, covering two row-path
//! shapes: the pattern-less seed table (one empty row over zero columns) and
//! `JoinTree::Unit` (one all-null row over the pattern width, the anchor for
//! leading optional patterns). Downstream operators run once per seed row,
//! exactly as the row path runs its pipeline once over the seed table.

use selene_core::Value;

use crate::{plan::BindingTableSchema, runtime::ExecutorError};

use super::{
    binding_batch::{BatchBuffer, BindingBatch},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
};

/// Pull-based single-row source.
pub(crate) struct BatchSeedRow {
    schema: BindingTableSchema,
    emitted: bool,
    empty: bool,
    state: OperatorState,
}

impl BatchSeedRow {
    /// Construct the pattern-less seed: one empty row over zero columns.
    #[must_use]
    pub(crate) fn unit() -> Self {
        Self {
            schema: BindingTableSchema {
                columns: Vec::new(),
            },
            emitted: false,
            empty: false,
            state: OperatorState::Created,
        }
    }

    /// Construct a `JoinTree::Unit` row: one all-null row over the schema width.
    #[must_use]
    pub(crate) fn null_row(schema: BindingTableSchema) -> Self {
        Self {
            schema,
            emitted: false,
            empty: false,
            state: OperatorState::Created,
        }
    }

    /// Construct an empty table source: one zero-row batch, then exhausted.
    ///
    /// The driver uses this for the proven-safe zero row limit (the row path
    /// skips its pattern walk entirely when the pushed-down limit is zero):
    /// downstream prefix operators still run over the empty input, exactly as
    /// the row pipeline runs over the empty pattern table.
    #[must_use]
    pub(crate) fn empty_table(schema: BindingTableSchema) -> Self {
        Self {
            schema,
            emitted: false,
            empty: true,
            state: OperatorState::Created,
        }
    }
}

impl PhysicalOperator for BatchSeedRow {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch seed init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.emitted = false;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn next_batch(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        _buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        if self.state == OperatorState::Exhausted {
            return Ok(None);
        }
        if self.state != OperatorState::Open {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch seed pull is legal only while Open",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        if self.emitted {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        }
        self.emitted = true;
        ctx.finish_batch(1);
        if self.empty {
            return Ok(Some(BindingBatch::empty(self.schema.clone())));
        }
        if self.schema.columns.is_empty() {
            return Ok(Some(BindingBatch::unit()));
        }
        let columns = (0..self.schema.columns.len())
            .map(|_| vec![Value::Null])
            .collect::<Vec<_>>();
        BindingBatch::from_columns(self.schema.clone(), columns)
            .map(Option::Some)
            .map_err(|_| ExecutorError::ImplementationDefined {
                detail: "batch seed built a malformed batch",
            })
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        self.emitted = false;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}
