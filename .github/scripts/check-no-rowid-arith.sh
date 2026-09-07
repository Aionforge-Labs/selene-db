#!/usr/bin/env bash
# BRIEF-Item-4a guard (internal/external NodeId split, D22).
#
# Forbids reconstructing an external id from a storage row (`row + 1`) or a row
# from an external id (`id - 1`) via arithmetic in production read paths. The
# external NodeId/EdgeId is stable and never-reused; the internal RowIndex is
# remappable by compaction (BRIEF-Item-4b), so `id == row + 1` no longer holds.
# Read paths MUST resolve through SeleneGraph's candidate producers or stable
# ID accessors (node_candidates_*, is_node_alive, node_labels, etc.).
# The binding-authority helper `node_row_index_arith` / `edge_row_index_arith`
# (create_* and the recovery WAL-replay fallback in into_graph) is the only
# sanctioned id->row arithmetic. The snapshot encoder no longer synthesizes ids
# (STEP 9: it persists the explicit id from the row_to_id column), and recovery
# places snapshot rows positionally.
#
# A handful of legitimate row->id sites (the recovery identity bootstrap in
# shared.rs::rebuild_id_maps for externally-built graphs that never populated
# row_to_id) are annotated with a trailing `// rowid-arith-ok: <reason>` and
# excluded here.
#
# Scope: production source of the split-affected crates. Test/bench fixtures
# (which legitimately build identity-mapped data) and other crates (e.g.
# selene-vector's own slot<->id mapping) are out of scope.
set -uo pipefail

# row->id reconstruction: *Id::new(<...> + 1); inline id->row: .get() - 1
# `[^;]*` (not `[^)]*`) so nested calls like `NodeId::new(u64::from(row) + 1)`
# are caught, while still not crossing a statement boundary.
pattern='(Node|Edge)Id::new\([^;]*\+ ?1\)|\.get\(\) ?- ?1'

# git grep over the four split-affected crates' production source, excluding test
# files; then drop allowlisted lines and full-line comments. git grep exits 1
# when there are no matches, so `|| true` keeps the pipeline alive.
matches=$(
  git grep -nE "$pattern" -- \
    ':(glob)crates/selene-graph/src/**/*.rs' \
    ':(glob)crates/selene-gql/src/**/*.rs' \
    ':(glob)crates/selene-algorithms/src/**/*.rs' \
    ':(glob,exclude)**/*tests*.rs' \
    ':(glob,exclude)**/tests.rs' 2>/dev/null \
    | grep -v 'rowid-arith-ok' \
    | grep -vE ':[0-9]+: *//' \
    || true
)

if [ -n "$matches" ]; then
  echo "$matches"
  echo
  echo "FORBIDDEN id<->row arithmetic in a production read path (BRIEF-Item-4a / D22)."
  echo "Resolve through SeleneGraph's candidate producers and stable ID accessors,"
  echo "or — for a genuine binding-authority/encode site — annotate the line // rowid-arith-ok: <reason>."
  exit 1
fi
echo "OK: no unguarded id<->row arithmetic in production read paths."
