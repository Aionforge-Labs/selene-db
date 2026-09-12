# Path planning and batch execution (F05-PR04)

Statements and the direct native path seam use the same product-graph search,
qualification, selection and native `PATH` materialization. Physical lowering
transports the logical automata, analyzer binding identities/types, predicate
identities and source expression payloads. It does not parse source again or
infer singleton/group degree at execution time. Nested expression-query patterns
in mutation values are included in that logical inventory; they remain read-only
while observing the statement's staged snapshot.

`JoinTree::Paths` owns one complete MATCH clause. `BatchPath` receives the pinned
snapshot, cancellation/deadline/scan-budget context, memory accounting, planned
expression subqueries, batch policy and input binding. Each input has independent
endpoint partitions and history. A bound questioned `NULL` is not an unbound
slot. Reduced anonymous bindings keep multiplicity, including zero-column rows.
Graph-pattern joins and optional null extension stay at their existing boundaries.

Unrestricted one-hop primitives retain the existing batch scan/expand access
paths. A positive selector on an unnamed node-only pattern is an identity
operation (one zero-hop candidate per endpoint pair), so its indexed scan stays
intact. Other path families have an eager failure barrier before batch pulls;
even a downstream `LIMIT 0` cannot suppress incomplete selection. Resource
exhaustion produces a failed request, never a successful partial table.

The [selection and completion policy](product-path-selection.md) remains
history-sensitive. A clause with only literal property conditions can exclude
impossible endpoints from its completion certificate without executing expressions
early. Clauses with opaque predicates retain the conservative superset.
Statement paths use the existing native work/row/byte defaults and the statement's
`max_quantifier` hop cap. Memory counters are conservative executor estimates,
not allocator or process-peak measurements. The whole-query benchmark separately
reports fresh-process RSS with a retained result, including filter/join and result
materialization costs. It is not a traversal-only speed claim.

## Execution witnesses, not a grammar-based claim

All features remain **implemented, unclaimed**. `EVID-PATH-BATCHES` records
executable evidence presence, not complete ISO conformance or release readiness.
Source clauses are §§14.4, 16.3–16.12 and 22.2–22.4; the evidence policy is §24.

| Feature/rule | Concrete witness |
|---|---|
| G002/G003, ID086 | `product_path/oracle.rs` compares absent/default, explicit REPEATABLE ELEMENTS and DIFFERENT EDGES for all four modes; `path_batches.rs::mixed_orientation_modes_match_modes_and_zero_edges_share_the_request_path` checks clause-wide edge identity. |
| G010–G013 | Independent small multigraph oracle and facade mixed-mode witness distinguish WALK, TRAIL, SIMPLE closure and ACYCLIC. |
| G014–G020 | `path_batches.rs::selective_families_keep_parallel_identity_and_distinct_length_group_counts` exercises ALL PATHS, ANY, ANY N PATHS, ANY/ALL SHORTEST, counted paths and counted groups. `product_path/selector_tests.rs` independently enumerates 32 multigraphs, qualifies, partitions and selects before comparing direct and statement execution at batch sizes 1 and 7. |
| G036/G037/G060 | `path_batches.rs::questioned_and_zero_one_group_have_equal_traversals_but_distinct_exposure`; adjacent-quantifier differential decomposition and bound-NULL correlation tests. |
| G061 | Native open cyclic/DAG completion and boundary-layer tie tests; facade resource failure cannot become successful LIMIT output. |
| G043–G045/GH02 | Existing mixed-orientation fixtures plus the independent seven-orientation oracle now compare physical statement execution too. No orientation is reconstructed from endpoint order alone. |
| Correlation/multiplicity | Physical policies 1/2/7/1024 preserve 99 joined rows with per-input partitions and a reused questioned NULL; the facade preserves 1,650 rows from 1,100 duplicate scalar inputs. |
| Predicate order | Facade shortest results retain a longer qualifying route when the shorter route fails inline qualification; a clause-level WHERE remains post-selection. Planned EXISTS uses the same input. |
| Failure/mutations | Failed resource requests contain no result; path-qualified SET observes staged state and rolls back through the ordinary transaction funnel. |

Test paths above are under `crates/selene-gql/src/runtime/` and
`crates/selene-db/tests/`. Full commands and measurements belong in the delivery
handoff and [BENCHMARKS.md](../../BENCHMARKS.md).

## Example

```gql
INSERT (a:A)-[:E]->(b:B), (a)-[:E]->(c:C)-[:E]->(b) FINISH
```

Run the next statement separately. The inline condition qualifies a complete
group **before** shortest selection, retaining the two-edge route:

```gql
MATCH ALL SHORTEST p = (a:A)-[r{1,2} WHERE size(r) = 2]->(b:B)
RETURN p, size(r) AS hops
```

Changing `[r{0,1}]` to `[r?]` changes the exposed binding from `LIST<EdgeRef>`
(empty or singleton) to a conditional singleton (`NULL` or `EdgeRef`), not just
the spelling of the same binding.

## Deletion boundary

Legacy expectations were resolved against the landed logical/native contract,
not preserved as an oracle: omitted path mode stays WALK (it does not invent
TRAIL), counted open paths retain required edge reuse, and anonymous edge identity
is tested through typed path values rather than removed hidden result columns.
Hop exhaustion still reports `5GQL1`, now with the native `max_path_hops` detail.

Legacy repeat/questioned/path-mode/match-mode/selector row evaluators, their
visited/contributor machinery, obsolete IR and path-lowering helpers are gone.
`PATH[...]` validation and selected-path construction share the native typed
carrier; there is no endpoint/list reconstruction fallback. The independent
small oracle and selector reference remain test-only.

The generic row dispatcher still exists for **F04-PR09**. Its remaining callers
of patterns enter the same physical path operator (and the same batch one-hop
primitive); this adapter is not a second path evaluator. Group-level quantifier
and alternation syntax remains out of scope. No facade API, persistence format,
dependency or profile selection has changed.
