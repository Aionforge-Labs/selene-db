# Catalog-resolved native calls (F04-PR06)

The facade retains ownership of the frozen `BuiltinProcedureRegistry`. Each of
its 69 registrations (19 `algo.*`, 50 `selene.*`) attaches the exact existing
durable procedure descriptor to its runtime metadata. Catalog-bound analysis
resolves the named declaration and requires that identity, revision, signature,
defaults and effect to agree with the installed implementation. The declaration
becomes an exact plan dependency; removal/replacement invalidates dependent
plans. Reopen uses the same closed-inventory admission check: unavailable or
misdescribed active code bindings fail explicitly, not by silently skipping them.
No new procedure names, grammar, serialized code, dependencies, profile claims,
or on-disk fields are introduced.

At execution the call checks its captured registry epoch and complete metadata
before dispatch, including for empty input. Evaluated arguments are validated
against the signature, including numeric assignment widening and materialized
trailing defaults. The shared result validator enforces declared width, types
and nullability before YIELD projection. Native algorithm errors retain their
original cause under the procedure and source-span-bearing executor diagnostic.

## Batch execution and authority

Graph-read CALL operators join the physical query prefix. Each logical input
row invokes the registered implementation exactly once. Optional calls preserve
empty invocations with null yields; calls without YIELD preserve zero-column
unit multiplicity. Calls are eager barriers, so a later LIMIT does not suppress
invocations or their errors. Output is emitted in policy-sized typed batches
(explicit unit batches for zero-column output).

Algorithms receive only a graph context over the catalog-selected snapshot. They
cannot acquire a mutator. Catalog-write and maintenance procedures retain their
existing effect checks and transaction dispatch; the facade still rejects selected
maintenance requests. Procedure failure follows the existing failed-statement
and rollback rules; there is no native publication coordinator.

The remaining row suffix adapter is necessary for unsupported pattern families,
correlated seeds, and mutation insertion-site state. It shares registration,
argument and result validation with the batch path, not a separate native registry.
F04-PR09 owns final row-dispatch deletion. [Vector retrieval](native-vectors.md)
uses this same typed physical CALL boundary (F04-PR07), rather than introducing a
second vector dispatcher. Text/JSON reintegration remains F04-PR08 work. There is
no plugin installation API.

## Projection identity and lifetime

All existing algorithms use private stable-ID projections. Named projection
recipes and CSR caches remain ephemeral per graph, including build/drop calls;
they are not durable catalog mutations. Inactive `NativeProjection` declarations
do not become active through this change. Reopen reattaches executable procedure
declarations, not old ephemeral projection instances.

`ProjectionCatalog::resolve` validates snapshot identity and pins the returned
projection under the same lock. Its empty candidate token validates graph,
generation, layout and detached workspace identity without scanning graph rows.
Equal numeric generations from different detached snapshots cannot share cached
CSR state. A concurrent resolve cannot replace the caller's pinned snapshot.
Replacing a named recipe replaces its entry atomically; no independent result
cache or unvalidated declaration-generation cache is introduced.

## Evidence

`native_calls` exercises all 19 algorithm registrations through the facade,
graph selection with repeated labels, stable IDs after deletion, descriptor
reattachment after checkpoint/reopen, cause chains and failed-transaction writes.
`native_registration` covers catalog dependency invalidation and unsupported
activation. Batch tests cover input-window sizes 1/2/3/7/1024, optional/default
behavior, empty-input epoch/signature/effect changes, dynamic type rejection,
result-schema failure, and deleted/compacted IDs against direct WCC construction.
Direct algorithm comparisons share numerical kernels; they independently check
projection selection and adapter/cache correctness, not independent numerical
conformance. Projection tests also race equal-generation snapshots.

See `BENCHMARKS.md` for measured costs and the explicit planning/cache differences
between repeated statements and one multi-input query. No speedup is claimed.
