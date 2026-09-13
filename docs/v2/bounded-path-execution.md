# Bounded product-graph paths (F05-PR02)

`selene_gql::runtime::product_path::BoundedPathProgram` is a native integration
seam over the F05-PR01 automata, not a new language surface or facade promise.
Compile borrows one clause's automata and the analyzer's declarations; execute
borrows the statement snapshot and returns a complete named binding table.
It does not evaluate a whole statement or silently discard statement operators.

## Admitted behavior

- Alternating node/edge tests, label predicates and all seven mixed-edge
  orientations, with stable node/edge identities and parallel-edge multiplicity.
- Single edges, questioned conditional singletons and finite group quantifiers.
  Zero length binds its start/end node and zero edges; `?` exposes NULL or one
  edge while `{0,1}` exposes an empty or singleton list.
- WALK has no uniqueness filter; TRAIL forbids repeated edge identities;
  ACYCLIC forbids repeated nodes; SIMPLE permits only first/last node repetition.
  SIMPLE is not TRAIL: traversing an undirected edge out and back is SIMPLE.
- Path history resets at each automaton. DIFFERENT EDGES spans every automaton
  of the clause, separately from path mode. The implicit match mode is read
  from generated profile ID086 (currently REPEATABLE ELEMENTS).
- Reused named locals unify by analyzer identity, including whole group lists
  and bound NULL. Anonymous locals reduce out without deduplicating bindings.
- Adjacent quantified transitions retain independent bounds and mode provenance
  (the clarified MM03 decomposition). There are no parenthesized group scopes.

The search state retains graph position, automaton position, repetition depth,
named/anonymous captures, path nodes/edges and preceding clause edge identities.
An iterative history-sensitive DFS does not merge equal node/automaton positions.
Each sibling owns its history, so backtracking cannot contaminate another choice.
One-hop and quantified transitions use the same search and resource machinery.

## Failure and observability

Unsatisfiable bounds are implementation-defined errors. Open bounds, selectors
(including explicit ALL), inline/property predicates, path-value construction,
mixed clauses and malformed automata fail explicitly before traversal. There is
no fallback. F05-PR01's unsupported group quantifiers/group alternation/path pipe
alternation remain unsupported. An empty match is not an unsatisfiable bound.

Default execution limits: 1024 total declared upper-bound hops per pattern,
1,000,000 examined product states plus seed/edge candidates, 100,000 complete
rows, 64 MiB estimated retained storage, and 100,000 debug observations when
enabled. The native entry also respects the statement's smaller max-quantifier
cap and outer row cap. Every limit fails the entire call; none truncates a
successful path set. Cancellation, deadline, scan-budget and memory errors reuse
the batch diagnostics. The eager source fails even when a LIMIT above it would
consume only one row. The pin and owned reservation claims close on every exit.

Memory accounting is a conservative per-state capacity envelope plus candidate
storage, accounting for group payloads, copies and debug captures. It is not a
measurement of allocator calls, heap use or process RSS. Output-sensitive eager
materialization and history copying are intentional, bounded first-slice costs.

Path-length statistics run without debug tracing. Opt-in **TEMPORARY** observations
record each visited legal hop's transition, local mode provenance, graph IDs,
repetition depth, incidence choice and query locals (including the current group
prefix). These are debugging data outside result columns, not public traversal
order. No separate pre-existing TEMPORARY containment note was found in `docs/v2`;
this seam applies the supplied deterministic-presentation-only rule explicitly.

`cheapest_projection` records one hypothetical candidate cost per matched path
and one hypothetical edge-cost evaluation per edge occurrence, over **every**
complete clause binding. It assumes no cached prefix costs. These are cost-model
projections, not measured weighted costs or evidence that a cheapest selector ran.

## Evidence and bridges

Permanent Rust tests independently enumerate small mixed multigraph walks before
applying mode restrictions and joining quantifier decompositions. Shrinking
property tests preserve edge identities, loops, multiplicity and zero length.
Differentials compare native and statement-level batch filter/project operators using
deterministic presentation carriers; traversal order is never a conformance claim.
Separate regressions trigger resource limits, syntax/IR boundaries and lifecycle
cleanup. The `bounded_paths` benchmark reports absolute costs in `BENCHMARKS.md`.

The source uses the physical operator, batch row source and tracer; all batches
remain internal. F05-PR03 delivered selectors, predicates and path values;
F05-PR04 integrated paths and deleted the legacy path implementation. F04-PR09
removed the remaining row dispatcher and its path adapters. See
[batch-only execution](batch-execution.md). No profile, persisted representation
or grammar changed.
