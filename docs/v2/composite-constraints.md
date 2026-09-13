# Composite UNIQUE and keys

F05-PR05 supplies `Catalog::create_constraint` for a named, graph-owned constraint
over an ordered property target within one declared node or edge type. This is
the Selene `IM_COMPOSITE_CONSTRAINTS` extension, **not ISO GQL constraint syntax**.
No parser grammar is added. Existing property `UNIQUE` annotations are normalized
to the same constraint service; they are not an independent validator.

## Equality and missing values

- `Unique` and `CompositeUnique` have the same semantics at every arity. If any
  component is missing or native NULL, the entire tuple is excluded. This extends
  Selene's existing arity-one policy; it is not an implicit adoption of SQL rules.
- `Key` requires every component to be present and non-null, and the complete
  tuple must be unique. Default materialization precedes validation.
- Components use the shared distinctness/comparison domain and canonical numeric
  equality, with binary string collation. Tuple components are individually typed
  and length-delimited, never displayed values joined by a separator. Equivalent
  numeric representations and signed zero have equal keys. Record/list domain
  evidence is recursive; deleting its last witness releases the selected domain.
- Domains include graph ownership, node/edge kind, and exact declaring type.
  Constraint indexes contain stable entity identities, not reusable physical rows.

## Activation and transaction boundaries

The caller supplies inactive metadata without a backing identity. The facade
holds its writer reservation while pinning the graph, allocating an exact backing
index declaration, building the entire index and validating every tuple. Only
then may its ordinary publication authority store the new catalog and graph.
Duplicate data, invalid targets, failed backing construction or failed publication
leave the previous state intact. A query index, ANN index or text index is never
used as proof of uniqueness. Required backing cannot be dropped independently.

Enforcement removes every affected old tuple before inserting any final tuple.
Swaps and delete/reuse in one prepared mutation therefore do not depend on row or
batch order. Existing request/statement validation boundaries are retained: an
explicit transaction does not make a successfully executed request's constraints
deferred until a later request. A swap can be expressed in a single set-oriented
mutation. Rollback discards both primary data and the copy-on-write exact index.

Full construction is reserved for activation, schema changes and untrusted
snapshot admission. Ordinary mutation, prepared native mutation and replay use
the same delta service. Named catalog graph-type obligations remain separately
enforced even when an instance-local schema differs. Immutable admission proofs
let detached facade execution retain complete indexes without trusting an edited
public graph snapshot. No new publication authority is introduced.

## Persistence and cost

Format-2 index configuration tag 4 names exact constraint backing and its declaring
type. Only declarations and primary values are persisted; indexes and comparison
domain counts are rebuilt before open returns a writable database. Earlier unary
declaration representation without a separate backing ID remains decodable and
receives the same complete in-memory tuple index at admission, not a legacy
full-scan commit path. Profile compatibility remains the existing exact-profile
admission policy: this extension changes the profile hash; it does not add a
migration path for stores created under a previous hash.

Constraint work for a property-only single-element update follows the old/new
tuple and recursive value shape, independent of total graph cardinality. Index
maps use persistent copy-on-write chunks, not a whole-map clone on each commit.
This is not a claim that the entire facade or durable preflight is O(delta):
logical replay still performs its existing primary-state reconstruction and
resource-accounting passes. Those passes are not repeated uniqueness key scans.

Regression evidence lives in `tests/composite_constraints.rs` in the facade and
`type_validator/unique/incremental_tests.rs` in the graph crate. The latter uses an
independent integer-tuple final-state model and work counters at multiple sizes
and arities. Benchmarks separate activation, one-element updates, mixed writes,
and rollback in the existing `catalog_lifecycle` target; see `BENCHMARKS.md`.
