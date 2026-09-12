use rustc_hash::FxHashSet;
use selene_core::Value;

use crate::{
    Aggregate, GqlStatus, SourceSpan,
    runtime::{
        Binding, BindingTableSchema, EvalCtx, ExecutorError, ExecutorWarning, evaluator,
        value_compare, value_key::RuntimeEqKey,
    },
};

mod numeric;

use self::numeric::{
    NumericSum, Welford, add_numeric, avg_to_value, count_to_value, percentile_cont_to_value,
    percentile_disc_to_value, percentile_numeric_to_f64, percentile_value, stddev_pop_to_value,
    stddev_samp_to_value,
};

/// One aggregate application with its running state and `DISTINCT` set.
///
/// Shared with the batch group operator so batch accumulation runs the same
/// state transitions, null elimination, `DISTINCT` handling, and
/// finalization (including type promotion and specified empty-group
/// results) as the row path, from the same analyzed [`Aggregate`]
/// descriptor. The grouping loop itself stays native to each engine.
pub(crate) struct AggregateSlot<'plan> {
    aggregate: &'plan Aggregate,
    state: AggregateState,
    seen: FxHashSet<RuntimeEqKey>,
    domain: crate::runtime::comparison_domain::ComparisonDomain,
}

impl<'plan> AggregateSlot<'plan> {
    /// Classify `aggregate` and build its empty state.
    ///
    /// # Errors
    ///
    /// Returns `ImplementationDefined` for star/arity shapes the executor
    /// does not implement.
    pub(crate) fn new(aggregate: &'plan Aggregate) -> Result<Self, ExecutorError> {
        Ok(Self {
            aggregate,
            state: AggregateState::new(classify(aggregate)?),
            seen: FxHashSet::default(),
            domain: Default::default(),
        })
    }

    /// Observe one input row: evaluate the argument, eliminate nulls (with
    /// the shared `NULL_VALUE_ELIMINATED_IN_SET_FUNCTION` warning),
    /// deduplicate `DISTINCT` arguments, and advance the state.
    ///
    /// `COUNT(*)` counts rows without evaluating an argument, exactly as on
    /// the row path.
    ///
    /// # Errors
    ///
    /// Returns evaluation, domain, and numeric data exceptions unchanged.
    pub(crate) fn observe(
        &mut self,
        row: &Binding,
        schema: &BindingTableSchema,
        ctx: &EvalCtx<'_, '_, '_, '_>,
    ) -> Result<(), ExecutorError> {
        if matches!(self.state, AggregateState::CountStar { .. }) {
            return self.state.observe(None, self.aggregate.span);
        }
        if self.state.needs_percentile() {
            let arg = self
                .aggregate
                .args
                .get(1)
                .ok_or(ExecutorError::ImplementationDefined {
                    detail: "PERCENTILE independent argument missing",
                })?;
            let value = evaluator::evaluate(&arg.expr, row, schema, ctx)?;
            let percentile = percentile_value(value, self.aggregate.span)?;
            self.state.set_percentile(percentile);
        }
        let arg = self
            .aggregate
            .args
            .first()
            .ok_or(ExecutorError::ImplementationDefined {
                detail: "aggregate argument missing",
            })?;
        let value = evaluator::evaluate(&arg.expr, row, schema, ctx)?;
        if self.state.skips_null() && matches!(value, Value::Null) {
            ctx.tx.emit_warning_once(ExecutorWarning {
                code: GqlStatus::NULL_VALUE_ELIMINATED_IN_SET_FUNCTION,
                message: "null value eliminated in set function".to_owned(),
                span: self.aggregate.span,
            });
            return Ok(());
        }
        if self.aggregate.distinct {
            self.domain.observe(
                std::slice::from_ref(&value),
                selene_core::ComparisonMode::Distinctness,
            )?;
            let key = RuntimeEqKey::from_row(vec![value.clone()]);
            if !self.seen.insert(key) {
                return Ok(());
            }
        }
        self.state.observe(Some(value), self.aggregate.span)
    }

    /// Finalize the running state into the aggregate result value,
    /// preserving the specified empty-group results (`COUNT` zero, `SUM`
    /// zero, `AVG`/`MIN`/`MAX`/deviations null, `COLLECT_LIST` empty).
    ///
    /// # Errors
    ///
    /// Returns the numeric range errors the final conversion can raise.
    pub(crate) fn finalize_value(self) -> Result<Value, ExecutorError> {
        self.state.finalize(self.aggregate.span)
    }
}

/// Output column names for one aggregate application.
///
/// Shared with the batch group operator so both engines describe the same
/// synthesized columns.
pub(crate) fn output_names(aggregate: &Aggregate) -> Vec<selene_core::DbString> {
    vec![aggregate.output_name.clone()]
}

#[derive(Clone, Copy)]
enum AggregateFn {
    Count,
    CountStar,
    Sum,
    Avg,
    StddevPop,
    StddevSamp,
    Min,
    Max,
    Collect,
    PercentileCont,
    PercentileDisc,
}

enum AggregateState {
    Count {
        count: u64,
    },
    CountStar {
        count: u64,
    },
    Sum {
        sum: Option<NumericSum>,
    },
    Avg {
        sum: Option<NumericSum>,
        count: u64,
    },
    StddevPop {
        stats: Welford,
    },
    StddevSamp {
        stats: Welford,
    },
    Min {
        value: Option<Value>,
    },
    Max {
        value: Option<Value>,
    },
    Collect {
        values: Vec<Value>,
    },
    PercentileCont {
        values: Vec<f64>,
        percentile: Option<Option<f64>>,
    },
    PercentileDisc {
        values: Vec<Value>,
        percentile: Option<Option<f64>>,
    },
}

impl AggregateState {
    fn new(function: AggregateFn) -> Self {
        match function {
            AggregateFn::Count => Self::Count { count: 0 },
            AggregateFn::CountStar => Self::CountStar { count: 0 },
            AggregateFn::Sum => Self::Sum { sum: None },
            AggregateFn::Avg => Self::Avg {
                sum: None,
                count: 0,
            },
            AggregateFn::StddevPop => Self::StddevPop {
                stats: Welford::default(),
            },
            AggregateFn::StddevSamp => Self::StddevSamp {
                stats: Welford::default(),
            },
            AggregateFn::Min => Self::Min { value: None },
            AggregateFn::Max => Self::Max { value: None },
            AggregateFn::Collect => Self::Collect { values: Vec::new() },
            AggregateFn::PercentileCont => Self::PercentileCont {
                values: Vec::new(),
                percentile: None,
            },
            AggregateFn::PercentileDisc => Self::PercentileDisc {
                values: Vec::new(),
                percentile: None,
            },
        }
    }

    fn skips_null(&self) -> bool {
        !matches!(self, Self::CountStar { .. } | Self::Collect { .. })
    }

    fn needs_percentile(&self) -> bool {
        matches!(
            self,
            Self::PercentileCont {
                percentile: None,
                ..
            } | Self::PercentileDisc {
                percentile: None,
                ..
            }
        )
    }

    fn set_percentile(&mut self, next: Option<f64>) {
        match self {
            Self::PercentileCont { percentile, .. } | Self::PercentileDisc { percentile, .. }
                if percentile.is_none() =>
            {
                *percentile = Some(next);
            }
            _ => {}
        }
    }

    fn observe(&mut self, value: Option<Value>, span: SourceSpan) -> Result<(), ExecutorError> {
        match self {
            Self::Count { count } => {
                *count = count.saturating_add(1);
                Ok(())
            }
            Self::CountStar { count } => {
                *count = count.saturating_add(1);
                Ok(())
            }
            Self::Sum { sum } => {
                let value = value.ok_or(ExecutorError::ImplementationDefined {
                    detail: "aggregate value missing",
                })?;
                *sum = Some(add_numeric(sum.take(), value, span)?);
                Ok(())
            }
            Self::Avg { sum, count } => {
                let value = value.ok_or(ExecutorError::ImplementationDefined {
                    detail: "aggregate value missing",
                })?;
                *sum = Some(add_numeric(sum.take(), value, span)?);
                *count = count.saturating_add(1);
                Ok(())
            }
            Self::StddevPop { stats } | Self::StddevSamp { stats } => {
                let value = value.ok_or(ExecutorError::ImplementationDefined {
                    detail: "aggregate value missing",
                })?;
                stats.observe(value, span)
            }
            Self::Min { value: current } => update_min_max(current, value, span, true),
            Self::Max { value: current } => update_min_max(current, value, span, false),
            Self::Collect { values } => {
                values.push(value.ok_or(ExecutorError::ImplementationDefined {
                    detail: "aggregate value missing",
                })?);
                Ok(())
            }
            Self::PercentileCont { values, .. } => {
                let value = value.ok_or(ExecutorError::ImplementationDefined {
                    detail: "aggregate value missing",
                })?;
                values.push(percentile_numeric_to_f64(&value, span)?);
                Ok(())
            }
            Self::PercentileDisc { values, .. } => {
                let value = value.ok_or(ExecutorError::ImplementationDefined {
                    detail: "aggregate value missing",
                })?;
                percentile_numeric_to_f64(&value, span)?;
                values.push(value);
                Ok(())
            }
        }
    }

    fn finalize(self, span: SourceSpan) -> Result<Value, ExecutorError> {
        match self {
            Self::Count { count } | Self::CountStar { count } => count_to_value(count, span),
            Self::Sum { sum } => Ok(sum.map_or(Value::Int(0), NumericSum::into_value)),
            Self::Avg { sum, count } => avg_to_value(sum, count, span),
            Self::StddevPop { stats } => stddev_pop_to_value(stats, span),
            Self::StddevSamp { stats } => stddev_samp_to_value(stats, span),
            Self::Min { value } | Self::Max { value } => Ok(value.unwrap_or(Value::Null)),
            Self::Collect { values } => Ok(Value::List(values)),
            Self::PercentileCont { values, percentile } => {
                percentile_cont_to_value(values, percentile, span)
            }
            Self::PercentileDisc { values, percentile } => {
                percentile_disc_to_value(values, percentile, span)
            }
        }
    }
}

fn classify(aggregate: &Aggregate) -> Result<AggregateFn, ExecutorError> {
    let name = aggregate.function.as_str();
    if aggregate.star {
        return if name == "count" && !aggregate.distinct {
            Ok(AggregateFn::CountStar)
        } else {
            Err(ExecutorError::ImplementationDefined {
                detail: "aggregate star form not implemented",
            })
        };
    }
    if matches!(name, "percentile_cont" | "percentile_disc") {
        if aggregate.args.len() != 2 {
            return Err(ExecutorError::ImplementationDefined {
                detail: "PERCENTILE aggregate arity not implemented",
            });
        }
        return match name {
            "percentile_cont" => Ok(AggregateFn::PercentileCont),
            "percentile_disc" => Ok(AggregateFn::PercentileDisc),
            _ => unreachable!("matches! limited percentile names"),
        };
    }
    if aggregate.args.len() != 1 {
        return Err(ExecutorError::ImplementationDefined {
            detail: "aggregate arity not implemented",
        });
    }
    match name {
        "count" => Ok(AggregateFn::Count),
        "sum" => Ok(AggregateFn::Sum),
        "avg" => Ok(AggregateFn::Avg),
        "stddev_pop" => Ok(AggregateFn::StddevPop),
        "stddev_samp" => Ok(AggregateFn::StddevSamp),
        "min" => Ok(AggregateFn::Min),
        "max" => Ok(AggregateFn::Max),
        "collect_list" => Ok(AggregateFn::Collect),
        _ => Err(ExecutorError::ImplementationDefined {
            detail: "aggregate function not implemented",
        }),
    }
}

fn update_min_max(
    current: &mut Option<Value>,
    next: Option<Value>,
    span: SourceSpan,
    keep_min: bool,
) -> Result<(), ExecutorError> {
    let next = next.ok_or(ExecutorError::ImplementationDefined {
        detail: "aggregate value missing",
    })?;
    crate::runtime::comparison_domain::ensure_pair(
        &next,
        &next,
        selene_core::ComparisonMode::Ordering,
        span,
    )?;
    let Some(current_value) = current else {
        *current = Some(next);
        return Ok(());
    };
    crate::runtime::comparison_domain::ensure_pair(
        &next,
        current_value,
        selene_core::ComparisonMode::Ordering,
        span,
    )?;
    let ordering =
        value_compare::compare_for_sort(&next, current_value, value_compare::NullSortOrder::Last);
    if (keep_min && ordering.is_lt()) || (!keep_min && ordering.is_gt()) {
        *current_value = next;
    }
    Ok(())
}
