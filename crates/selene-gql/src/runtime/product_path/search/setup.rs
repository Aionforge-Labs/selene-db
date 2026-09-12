//! Search setup and reservation envelope shared by native and statement execution.

use super::*;

pub(in crate::runtime::product_path) fn execute(
    program: &BoundedPathProgram<'_>,
    limits: PathExecutionLimits,
    ctx: &mut BatchExecutionContext<'_>,
    qualify: Option<&Qualifier<'_>>,
    seed: Option<Vec<Option<Value>>>,
) -> Result<SearchResult, ExecutorError> {
    ctx.ensure_generation()?;
    ctx.check_cancel(program.paths[0].automaton.origin)?;
    for path in &program.paths {
        if path.upper > limits.max_hops {
            return Err(limit("max_path_hops", path.automaton.origin));
        }
    }
    // Capacity envelope includes growth slack, histories, values and batch/table
    // copies. This is not allocator/RSS measurement or a semantic hop bound.
    let path_hops: Vec<_> = program
        .paths
        .iter()
        .map(|p| {
            if !p.open {
                return p.upper;
            }
            let graph = ctx.snapshot().expect("validated pin");
            let natural =
                if program.different_edges || p.automaton.mode.mode == crate::PathMode::Trail {
                    graph.edge_count()
                } else if matches!(
                    p.automaton.mode.mode,
                    crate::PathMode::Simple | crate::PathMode::Acyclic
                ) {
                    graph.node_count()
                } else {
                    limits.max_hops as usize
                };
            limits
                .max_hops
                .min(u32::try_from(natural).unwrap_or(u32::MAX))
        })
        .collect();
    let hops = path_hops
        .iter()
        .try_fold(0usize, |sum, &hops| sum.checked_add(hops as usize))
        .ok_or_else(|| invalid("product path memory estimate overflow"))?;
    let elements: usize = program
        .paths
        .iter()
        .map(|p| p.automaton.semantic.elements.len())
        .sum();
    let input_bytes = seed.as_ref().map_or(0, |values| {
        values
            .iter()
            .flatten()
            .fold(0usize, |sum, value| sum.saturating_add(clone_bytes(value)))
    });
    let state_bytes = hops
        .checked_add(elements)
        .and_then(|n| n.checked_add(program.schema.columns.len()))
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(size_of::<Value>() * 16))
        .and_then(|n| n.checked_add(input_bytes.saturating_mul(4)))
        .ok_or_else(|| invalid("product path memory estimate overflow"))?;
    let budget = *ctx.budget_mut();
    let mut run = Search {
        program,
        limits,
        ctx,
        budget,
        reserved: 0,
        state_bytes,
        stats: PathExecutionStats::default(),
        observations: Vec::new(),
        stack: VecDeque::new(),
        rows: Vec::new(),
        candidates: Vec::new(),
        path_hops,
        qualify,
        breadth: false,
        cutoff: false,
        pairs: None,
    };
    let outcome = run.run(seed);
    if outcome.is_err() {
        run.budget.release(run.reserved);
        run.reserved = 0;
    }
    let Search {
        budget,
        stats,
        observations,
        rows,
        reserved,
        ..
    } = run;
    *ctx.budget_mut() = budget;
    outcome?;
    Ok(SearchResult {
        table: BindingTable::new(program.schema.clone(), rows),
        stats,
        observations,
        reserved,
    })
}

// Correlated inputs can own arbitrary recursive query values, not just graph
// IDs. Charge their deep-cloned storage in every history/output envelope too.
// Shared string/bytes/JSON/vector backing storage is retained by the input;
// cloning their handle does not allocate another payload.
fn clone_bytes(value: &Value) -> usize {
    stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
        let heap = match value {
            Value::List(items) => items
                .iter()
                .fold(0usize, |n, v| n.saturating_add(clone_bytes(v))),
            Value::Record(record) => match record.as_ref() {
                selene_core::Record::Open(fields) => {
                    fields
                        .iter()
                        .fold(size_of::<selene_core::Record>(), |n, (_, v)| {
                            n.saturating_add(size_of::<selene_core::DbString>())
                                .saturating_add(clone_bytes(v))
                        })
                }
                _ => usize::MAX,
            },
            Value::RecordTyped(record) => record
                .values
                .iter()
                .flatten()
                .fold(size_of::<selene_core::RecordTyped>(), |n, v| {
                    n.saturating_add(clone_bytes(v))
                }),
            Value::Path(path) => size_of::<selene_core::Path>().saturating_add(
                path.segments
                    .len()
                    .saturating_mul(size_of::<selene_core::PathSegment>()),
            ),
            Value::Duration(_) | Value::ZonedDateTime(_) | Value::ZonedTime(_) => 128,
            _ => 0,
        };
        size_of::<Value>().saturating_add(heap)
    })
}
