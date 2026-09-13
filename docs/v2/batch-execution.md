# Batch-only execution — F04-PR09

The production route is semantic analysis → logical authority → optimized
physical `ExecutionPlan` → batch assembly. `ExecutionPlan` retains the optimizer's
access paths and expression identities; it is not converted to a legacy row
plan. The scalar expression evaluator remains: evaluating one value is not an
alternate statement executor.

`runtime/batch/query.rs` assembles read segments and drains them before eager
mutation, catalog, native-write or effectful compound-query barriers. A barrier
borrows the existing transaction; it never commits a batch. Subsequent reads
resolve against the updated working graph. Facade request leases, failure states,
cache invalidation, durable acknowledgment and indeterminate outcomes retain
their existing authorities.

There is no plan acceptance predicate, partial-prefix outcome, row-suffix
dispatcher, seeded row route, or error retry. Optimizer disjunctions use batch
scans with anchor deduplication. The optimizer's WCO marker unwraps its one
physical child; malformed multi-child markers still fail explicitly. Pattern
subplans use physical operators and preserve their existing unsupported-pipeline
diagnostic. Unsupported GQL features still fail at profile/semantic admission;
cutover does not change the generated profile or admit grammar.

## Dispatch and regression inventory

| Family | Physical route | Regression evidence |
|---|---|---|
| Unit, empty, node/edge access, all optimizer access kinds | seed/scan/tree | `scan_tests`, primitive differentials, `exec_scan_*`, `parameter_aware_scan` |
| Filters, projection, page | filter/project/page | primitive differentials; consumed-predicate and computed-error fixtures |
| LET, FOR, inline/optional table calls | eager batch extension | `cutover_tests`, `exec_pipeline_*`, `lexical_scopes` |
| Inner/outer joins, disjunction, WCO marker | batch join/tree sources | join/chain matrices, independent relation model, `cutover_tests` |
| Sets, OTHERWISE, NEXT and correlated NEXT | batch set/chain; effectful barriers | set/chain matrices, `facade_joins_sets` |
| Grouping, distinct, sorting, top-K, carrier trim | batch aggregate/distinct/sort | independent relation model, `facade_group_sort` |
| Paths and selectors | product-path physical source | permanent multigraph oracle, selector model, `path_batches` |
| Scalar/native value families | unchanged expression/type kernels within batches | generated batch `inventory_tests`, `structural_type_inventory`, structural/numeric/cast/temporal/JSON/vector suites |
| Graph-native calls, algorithms, retrieval | typed BatchCall | native call/vector/text/JSON tests and facade fixtures |
| Writes, catalog and native-write calls | eager physical barriers | `batch_transactions`, mutation tests, durable failure tests |
| Transaction/session control | existing physical control and facade ownership | transaction/session/request lifecycle suites |
| EXPLAIN | single-shot physical control result | existing EXPLAIN fixtures |

Every supported source family now enters the same route, including seeded
expression/table subqueries. No value kind selects another executor. Result
tables preserve null versus empty versus omitted outcomes, multiplicity, declared
types, preferred columns and required ordering. Private insertion-site metadata
travels with preserving bindings across batch selection, paging and native calls;
it is not a user-visible value or persisted format.

Pipeline paging consumes preceding fallible work even after filling its result
window (including LIMIT 0); otherwise physical batch size could hide a later
division/type error. Only `pattern_row_limit`'s separately proved safe pattern
bound may short-circuit. No second executor is involved in either page behavior.

## Independent evidence and deletion

Before deletion, the non-empty transition run passed **170 tests** at baseline
`97facacb9c94a678d36e185fb8a37f5fb9483327`:

```sh
cargo nextest run -p selene-db-gql --lib --locked --all-features --profile default -E 'test(runtime::batch) | test(runtime::product_path)'
```

The old pipeline implementations, recursive pattern walk, scan collector,
hash/outer/subplan/WCO row executors, and batch-to-row adapters are physically
deleted. `runtime/pipeline` now contains only expression/schema/registration/
diagnostic kernels and the public entry point into batch execution.

Post-cutover policy comparisons are **partition-invariance tests**, not retained
row-oracle evidence. Permanent relation, type, bounded multigraph and selector
models remain test-only and do not import the deleted implementation. Hand-derived
fixtures can disagree with the engine. `batch_only_consumer` imports only the
facade and joins named graph selection, retrieval, algorithms, failed transaction
isolation, commit, checkpoint and reopen in one consumer.

See `BENCHMARKS.md` for measured costs and limitations. Materialization and scalar
value kernels remain intentional; this is not a claim of allocation-free execution
or completion of the separate F05 performance lane.
