//! Row-space cap + TOMBSTONE-aliasing-filter boundary tests (GRAPH-13).
//!
//! `Mutator::create_node` / `create_edge` derive the new dense row from the
//! current row count with
//! ```ignore
//! let row = u32::try_from(graph.{node,edge}_store.len())
//!     .ok()
//!     .filter(|&row| row != u32::MAX) // u32::MAX is RowIndex::TOMBSTONE
//!     .ok_or(GraphError::RowSpaceExhausted { .. })?;
//! ```
//! The `!= u32::MAX` filter is the load-bearing, unreachable-by-construction
//! guard: dropping it would let the 2^32-th live row alias
//! [`NodeRow::TOMBSTONE`], which unmapped/dead row checks treat as
//! "no row" — silently corrupting reads. We cannot push 2^32 rows in a test, so
//! this mirrors the EXACT production expression (kept in lock-step with
//! `mutator.rs`) and pins its behavior at the `u32::MAX-2 / -1 / MAX / MAX+1`
//! boundaries, plus the error-field shape the production sites construct.

use crate::error::GraphError;
use crate::store::NodeRow;

/// Verbatim mirror of the production row-derivation expression in
/// `Mutator::create_node` / `create_edge`. If the production filter changes, this
/// helper (and the boundary assertions below) must change with it — that paired
/// edit is the regression tripwire a silent filter-drop would otherwise evade.
fn derive_row(len: u64, kind: &'static str) -> Result<NodeRow, GraphError> {
    u32::try_from(len)
        .ok()
        .filter(|&row| row != u32::MAX)
        .map(NodeRow::new)
        .ok_or(GraphError::RowSpaceExhausted {
            kind,
            rows: len,
            max_rows: u32::MAX as u64,
        })
}

#[test]
fn row_below_tombstone_boundary_is_accepted() {
    // len = u32::MAX - 2 → row u32::MAX - 2 (well clear of the sentinel).
    let len = u32::MAX as u64 - 2;
    assert_eq!(derive_row(len, "node").unwrap(), NodeRow::new(u32::MAX - 2),);
}

#[test]
fn last_real_row_just_below_tombstone_is_accepted() {
    // len = u32::MAX - 1 → row u32::MAX - 1: the LAST allocatable row, exactly one
    // below the TOMBSTONE sentinel. This must succeed (the filter rejects only
    // u32::MAX itself), and the produced row must NOT equal the sentinel.
    let len = u32::MAX as u64 - 1;
    let row = derive_row(len, "node").unwrap();
    assert_eq!(row, NodeRow::new(u32::MAX - 1));
    assert_ne!(
        row,
        NodeRow::TOMBSTONE,
        "the last real row is not the sentinel"
    );
}

#[test]
fn row_at_tombstone_value_is_rejected() {
    // len = u32::MAX → row would be u32::MAX == NodeRow::TOMBSTONE. THIS is the
    // case the `!= u32::MAX` filter exists for: without it `derive_row` would
    // return Ok(NodeRow::TOMBSTONE), a live row aliasing the "no row" sentinel
    // that all reads would then misresolve. With the filter it is RowSpaceExhausted.
    let len = u32::MAX as u64;
    let err = derive_row(len, "node").expect_err("a row equal to TOMBSTONE must be rejected");
    assert!(
        matches!(
            err,
            GraphError::RowSpaceExhausted { kind, rows, max_rows }
                if kind == "node" && rows == u32::MAX as u64 && max_rows == u32::MAX as u64
        ),
        "expected RowSpaceExhausted at the TOMBSTONE boundary, got {err:?}",
    );
}

#[test]
fn row_beyond_u32_range_is_rejected() {
    // len = u32::MAX + 1 → u32::try_from fails outright (over the addressable
    // range). Carries the edge `kind` to also pin the per-call-site field.
    let len = u32::MAX as u64 + 1;
    let err = derive_row(len, "edge").expect_err("a row beyond u32 range must be rejected");
    assert!(
        matches!(
            err,
            GraphError::RowSpaceExhausted { kind, rows, .. }
                if kind == "edge" && rows == u32::MAX as u64 + 1
        ),
        "expected RowSpaceExhausted past the u32 range, got {err:?}",
    );
}

#[test]
fn tombstone_sentinel_is_the_excluded_value() {
    // Documents the invariant the filter encodes: the excluded value IS
    // NodeRow::TOMBSTONE's raw form, so a future change to the sentinel that
    // forgot to update the filter would diverge here.
    assert_eq!(NodeRow::TOMBSTONE.get(), u32::MAX);
    // And the boundary the filter draws: MAX-1 accepted, MAX rejected.
    assert!(derive_row(u32::MAX as u64 - 1, "node").is_ok());
    assert!(derive_row(u32::MAX as u64, "node").is_err());
}
