# Native selected paths (F05-PR03)

`runtime::product_path::BoundedPathProgram` executes one lowered MATCH clause.
The name is retained: execution is resource-bounded even when a source edge
quantifier has no upper bound. The statement driver's legacy path dispatch is
still deletion-owned by **F05-PR04**. This seam does not claim that cutover,
correlated-subquery planning, or grouped/alternating pattern syntax has landed.

## Qualification, selection, and values

For each incoming binding and each flat path automaton, execution enumerates
legal histories, qualifies complete bindings, partitions by stable `(start, end)`
node identities, then selects. It does not choose one global shortest length.
Property maps test every traversed element; inline group conditions see the
complete group, including an empty list. A questioned skip does not test an edge
that was never traversed. Only boolean true qualifies; null/false do not, and
expression errors propagate. A rejected shortest topological route does not
hide a longer qualifying route. Clause-level filters and expression subqueries
remain the statement integration's responsibility, not implicit path-local filters.

Length is the number of traversed edges, including every adjacent quantified
transition. There is no weighted interpretation. The families are:

| Prefix | Per-endpoint-partition selection |
|---|---|
| absent / `ALL` | All qualifying bindings |
| `ANY N` | Up to N bindings; this implementation prefers shorter paths |
| `ANY SHORTEST` / `SHORTEST 1` | One shortest binding |
| `SHORTEST N` | Up to N individual bindings, ranked by length |
| `ALL SHORTEST` / `SHORTEST 1 GROUP` | Every binding in the shortest length group |
| `SHORTEST N GROUPS` | Every binding in the N smallest distinct length groups |

Within a length, ties use node sequence, edge sequence, traversal direction
(outgoing, incoming, undirected), then stable discovery order for identical keys.
This is a reproducible native policy, **not an ISO-portable choice or row-order
guarantee**. Multiplicity survives: adjacent-quantifier decompositions can expose
different bindings for the same path. Parallel edges have distinct identities.
Written literal zero counts still fail in the parser with `22G0F`; defensive
zero-count native IR yields no bindings. Counts above a finite partition's size
retain the whole partition. Absent/unreachable partitions contribute no rows.

Selected path bindings use the existing native `Value::Path` carrier:
graph identity, starting node, then ordered `(edge, orientation, reached node)`
segments. Zero edges still retain the starting node. A reversed directed loop
records incoming orientation; an undirected edge never gains a synthetic directed
identity. The existing facade conversion adds database/graph ownership without
reconstructing paths from endpoints. The declared field remains `PATH`, including
empty results. Copying a path after deletion remains legal; accessing a deleted
referent or reconstructing a live path through it returns `22G11`.

No new list/endpoint result adapter or persistent path encoding was introduced.
The existing core/facade typed carriers already own this contract and are reused;
their ownership conversion is not a legacy path-dispatch bridge to delete.

## Completion and limits

Bounded and history-restricted traversal retains independent history frames;
there is no graph-position visited set that merges legal histories. Open
TRAIL/DIFFERENT EDGES and SIMPLE/ACYCLIC traversal can exhaust naturally. Hops,
examined work, retained estimated bytes, candidates/results, and observations
have explicit limits. A smaller policy hop limit fails when a legal continuation
would be lost; it is never substituted as a successful source quantifier bound.

Open selective WALK uses complete hop layers. A conservative reachability
certificate over the **union** of the automaton's edge tests over-approximates
possible endpoint pairs. It supplies no distances and never accepts paths or
prunes history/predicates. Early completion requires every possible partition's
quota and completion of its last length layer, including all ties. Otherwise
execution exhausts naturally or reports a resource error. An over-approximation
member rejected by predicates or the full automaton can prevent early completion;
cyclic searches with such unresolved partitions may hit the work/hop limit even
when the mathematical selected result is finite. Certificate reservation has a
conservative quadratic-in-node-count upper envelope. These are disclosed
first-slice costs, not silent empty/partial answers or claims of optimal search.

Memory accounting charges search frames, retained candidates, selection scratch,
typed values, and batch/table copies using conservative envelopes, not allocator
measurements. Exhausting the budget while collecting ties fails the entire
request. The eager failure barrier remains above batch pulls; a LIMIT above the
source cannot conceal incomplete selection. Cancellation/deadline errors retain
their existing GQLSTATUS and release the operator budget.

TEMPORARY observations remain opt-in. Their `Debug` representation reports counts
instead of local/temporary full-Value payloads. Callers must not persist or log
those payload fields. No serde/persistence implementation is supplied.

## Evidence

New native tests include exhaustive enumeration → qualification → endpoint
partition → selection over 32 small directed multigraphs; separately authored
open cyclic/DAG cases; complete boundary-layer ties; mixed orientations; adjacent
quantifier multiplicity; memory failure; and cancellation during qualification.
Facade tests carry actual native selected values through the existing result
conversion and check declared types, ownership, parallel identity and deletion.
The Python selector reference is supplemental finite-model evidence, not a parser
or complete GQL conformance oracle.

The existing `bounded_paths` benchmark binary now includes many-tie, long-path and
rejected-shortest rows. See `BENCHMARKS.md` for commands, scales, measured phase
times and retained-byte estimates. No shared-predecessor rewrite, new dependency,
profile change, persistent-format change, or new benchmark target was needed.
