//! Stable endpoint partitions, unit edge count, complete length groups.
//!
//! Equal-length choices use stable node/edge identity and traversal direction,
//! then encounter order for otherwise identical bindings. This is a reproducible
//! native policy, NOT a portable GQL row-order or tie-choice guarantee.

use super::state::SearchState;
use crate::{
    PathSelector, SourceSpan,
    runtime::{ExecutorError, batch::operator::BatchExecutionContext},
};

pub(super) fn select(
    candidates: &mut Vec<SearchState>,
    selector: Option<PathSelector>,
    ctx: &BatchExecutionContext<'_>,
    span: SourceSpan,
) -> Result<(), ExecutorError> {
    let Some(selector) = selector else {
        return Ok(());
    };
    if selector == PathSelector::All {
        return Ok(());
    }
    // Stable in-place insertion into a total-key ordering would be quadratic.
    // Sort indexes are unnecessary: stable slice sort needs at most N/2 scratch
    // elements, already covered by the per-candidate reservation envelope.
    candidates.sort_by(|a, b| {
        (
            a.nodes.first(),
            a.nodes.last(),
            a.edges.len(),
            &a.nodes,
            &a.edges,
        )
            .cmp(&(
                b.nodes.first(),
                b.nodes.last(),
                b.edges.len(),
                &b.nodes,
                &b.edges,
            ))
            .then_with(|| {
                a.directions
                    .iter()
                    .map(direction_key)
                    .cmp(b.directions.iter().map(direction_key))
            })
    });
    ctx.check_cancel(span)?;
    let (count, groups) = match selector {
        PathSelector::Any { paths } | PathSelector::CountedShortest { paths } => {
            (paths as usize, false)
        }
        PathSelector::AnyShortest => (1, false),
        PathSelector::AllShortest => (1, true),
        PathSelector::CountedShortestGroup { groups } => (groups as usize, true),
        PathSelector::All => unreachable!(),
    };
    let mut pair = None;
    let mut previous_length = None;
    let mut rank = 0usize;
    let mut error = None;
    candidates.retain(|state| {
        if error.is_some() {
            return false;
        }
        if let Err(e) = ctx.check_cancel(span) {
            error = Some(e);
            return false;
        }
        let next_pair = (state.nodes[0], *state.nodes.last().expect("path endpoint"));
        if pair != Some(next_pair) {
            pair = Some(next_pair);
            previous_length = None;
            rank = 0;
        }
        if !groups || previous_length != Some(state.edges.len()) {
            rank += 1;
        }
        previous_length = Some(state.edges.len());
        rank <= count
    });
    error.map_or(Ok(()), Err)
}

fn direction_key(direction: &selene_core::EdgeDirection) -> u8 {
    match direction {
        selene_core::EdgeDirection::Outgoing => 0,
        selene_core::EdgeDirection::Incoming => 1,
        selene_core::EdgeDirection::Undirected => 2,
    }
}
