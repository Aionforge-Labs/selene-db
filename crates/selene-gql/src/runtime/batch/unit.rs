//! Single-row seed sources for batch execution.
//!
//! [`BatchSeedRow`] serves exactly one logical row, covering two row-path
//! shapes: the pattern-less seed table (one empty row over zero columns) and
//! `JoinTree::Unit` (one all-null row over the pattern width, the anchor for
//! leading optional patterns). Downstream operators run once per seed row,
//! exactly as the row path runs its pipeline once over the seed table.

use selene_core::Value;

use crate::{
    plan::BindingTableSchema,
    runtime::{Binding, BindingTable, ExecutorError},
};

use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
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

/// Pull-based source serving owned rows in policy-sized batches.
///
/// Seeded-subplan pipeline prefixes start here: a materialized table (one
/// seed row, or one block's rows) re-enters the operator protocol so the
/// shared prefix builders (`Filter`, `Project`, `Limit`, `Match`, set and
/// chain operators) run unchanged. Rows keep their table order.
pub(crate) struct BatchRowSource {
    schema: BindingTableSchema,
    rows: Vec<Binding>,
    cursor: usize,
    policy: BatchPolicy,
    state: OperatorState,
}

impl BatchRowSource {
    /// Construct a source over `table`'s rows with `policy` sizing.
    #[must_use]
    pub(crate) fn new(table: BindingTable, policy: BatchPolicy) -> Self {
        let (schema, rows) = table.into_parts();
        Self {
            schema,
            rows,
            cursor: 0,
            policy,
            state: OperatorState::Created,
        }
    }
}

impl PhysicalOperator for BatchRowSource {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch row source init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn next_batch(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        if self.state == OperatorState::Exhausted {
            return Ok(None);
        }
        if self.state != OperatorState::Open {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch row source pull is legal only while Open",
            });
        }
        ctx.ensure_generation()?;
        let span = crate::SourceSpan::default();
        ctx.check_cancel(span)?;
        if self.cursor >= self.rows.len() {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        }
        // An anonymous path can reduce to multiple zero-column bindings.
        // Empty column storage alone cannot encode their multiplicity; emit
        // one unit batch per binding rather than silently turning it into zero.
        if self.schema.columns.is_empty() {
            let batch = BindingBatch::unit()
                .with_binding_sites(&self.rows[self.cursor..self.cursor + 1])
                .map_err(|_| ExecutorError::ImplementationDefined {
                    detail: "invalid batch binding provenance",
                })?;
            self.cursor += 1;
            ctx.finish_batch(1);
            ctx.budget_mut()
                .reserve(batch.estimated_bytes())
                .map_err(|err| err.into_executor_error(span))?;
            return Ok(Some(batch));
        }
        let width = self.schema.columns.len();
        let take = self
            .policy
            .rows_per_batch(
                width
                    .saturating_mul(std::mem::size_of::<Value>().saturating_add(1))
                    .max(1),
            )
            .min(self.rows.len() - self.cursor);
        let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(width);
        for _ in 0..width {
            let mut values = buffer.take_values();
            let mut nulls = buffer.take_nulls();
            values.clear();
            nulls.clear();
            columns.push((values, nulls));
        }
        for row in &self.rows[self.cursor..self.cursor + take] {
            for (slot, (column, nulls)) in columns.iter_mut().enumerate() {
                let value = row.get(slot).cloned().unwrap_or(Value::Null);
                nulls.push(value == Value::Null);
                column.push(value);
            }
        }
        self.cursor += take;
        ctx.finish_batch(take);
        let batch_columns = columns
            .into_iter()
            .map(|(values, nulls)| {
                BatchColumn::from_parts(values, nulls).map_err(|_| {
                    ExecutorError::ImplementationDefined {
                        detail: "batch row source built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = BindingBatch::from_batch_columns(self.schema.clone(), batch_columns)
            .and_then(|batch| batch.with_binding_sites(&self.rows[self.cursor - take..self.cursor]))
            .map_err(|_| ExecutorError::ImplementationDefined {
                detail: "batch row source built a malformed batch",
            })?;
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        self.rows.clear();
        self.cursor = 0;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}
