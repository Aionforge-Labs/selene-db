# Native vector retrieval (F04-PR07)

Vector retrieval uses the catalog-resolved graph-read CALL boundary described in
[native calls](native-calls.md), including signature/default/result validation
and policy-sized typed binding batches. There is no second vector registry or
facade dispatch path. The 69 procedure registrations, names, signatures, profile
claims and durable descriptor layout are unchanged.

## Identity, values and lifecycle

An index declaration belongs to its catalog graph and identifies the node label,
property, dimension, kind (including ANN metric), optional name and construction
configuration. The selected graph's compiled catalog binding checks declaration
readiness and the installed configuration/name. The runtime uses that pinned
graph, never a process-wide `(label, property)` cache. Replacing the graph creates
new ownership and declaration identities; identical labels in another graph are
not interchangeable. OR REPLACE still obeys the existing RESTRICT contract.

`CAST(<LIST<numeric>> AS VECTOR)` remains the GQL producer. Vectors contain
1–65,535 finite `f32` components. Empty/non-numeric/out-of-f32-range casts fail;
the native constructor also rejects NaN/infinity. No truncation or padding is
performed. Scoring dimension mismatches and cosine zero norms are errors, not
missing values. Missing and non-vector properties are skipped by scoring.
Generic candidate binding checks identity and liveness only: it does not impose
vector-property requirements. Index creation and indexed writes remain stricter:
non-null incompatible values, dimensions and cosine zero norms are rejected.

Insert/update/delete and rollback use existing mutation funnels and copy-on-write
snapshots. HNSW/IVF cached vector references and TurboQuant codes are derived;
primary graph values and durable declarations remain authoritative. Facade open
strictly rebuilds indexes before publishing a usable database. Rebuild failure
does not publish partially rebuilt indexes. A lower-layer lenient rebuild which
skips any rejected value now marks that accelerator incomplete; query lookup
declines it until a successful rebuild. Registration/diagnostic state is retained.
This completeness bit is in-memory only, not an on-disk field.

Exact search no longer restricts its label scan using a derived vector bitmap.
Even a stale/partial bitmap retained after rebuild failure cannot produce a
healthy-looking incomplete exact answer. Primary vector errors are still errors.
No read-only call starts a rebuild, acquires a mutator or publishes catalog state.
The facade's existing maintenance-request restrictions remain in force.

## Exact and approximate contracts

| Procedure | Result columns | Contract |
|---|---|---|
| `selene.vector_search_nodes` | `node_id: NODE`, `distance: FLOAT64` | Exhaustive primary-value label scan; no ANN fallback |
| `selene.vector_search_nodes_batch` | `query_index: UINT64`, `node_id: NODE`, `distance: FLOAT64` | Same exact contract, `LIST<VECTOR>` queries, zero-based query positions |
| `selene.vector_search_nodes_ann` | `node_id: NODE`, `distance: FLOAT64` | Explicit ANN selection; matching usable ANN registration required when searching |
| `selene.vector_search_nodes_ann_batch` | `query_index: UINT64`, `node_id: NODE`, `distance: FLOAT64` | Batched explicit ANN; no filter parameters on this existing signature |

All distances are lower-is-better: squared Euclidean `sum((q-x)^2)`, cosine
`1-dot(q,x)/(|q||x|)`, negative inner product `-dot(q,x)`. Metric arithmetic
retains the existing `f64` accumulator and safe `wide` SIMD behavior. Exact
defaults to `squared_euclidean`; ANN's omitted/NULL metric resolves from the
matching index. Unknown metrics and missing/mismatched ANN registrations report
GQLSTATUS `22G03`; they do not select a different algorithm silently.

Results sort by ascending distance, then ascending stable node ID for ties among
returned hits. Exact search selects the best `min(k, eligible)` IDs, including
ties at the cutoff. ANN can omit tied or untied neighbors outside its search
budget; sorting its returned ties is not a recall guarantee. Batch results group
by input query position. Negative `k` is invalid; larger-than-available `k` is a
cap, not padding. `k=0` produces no hits. Existing early exits remain: exact zero-k
does not score graph values, empty query batches do no searches, and filtered
ANN zero-k/empty allowlists validate candidate identity but need not resolve an
index. Unfiltered nonempty ANN batches/single calls resolve the matching index
even for zero-k. Batch query dimensions must agree. Early exits are not vector
or index health verification APIs.

### Filter policy and incomplete recall

The single ANN procedure retains its optional indexed node-property value filter
and indexed edge-property endpoint filter. Both supplied filters intersect on
the selected graph. They do not mean "global top-k, then WHERE". A subsequent
GQL WHERE is a separate post-search filter and can further reduce cardinality.

- **HNSW:** traverse globally, including disallowed nodes, with beam
  `max(ef_search, k, 1)`; discard stale/disallowed hits from that bounded beam
  before final top-k. There is **no exact refill or automatic width expansion**.
  A selective filter can return zero or fewer than k despite many eligible nodes.
- **IVF:** select up to `max(ef_search, 1)` centroid lists (capped by list count),
  then admit allowed current entries before scoring/top-k. Unprobed lists can
  contain better or additional eligible vectors. An untrained index scans its
  current entries. Full probing may be exact but is not promised at smaller widths.
- **TurboQuant cosine:** restrict compressed candidate selection to allowed index
  rows, then rerank candidates against primary graph vectors. When
  `max(ef_search, k)` covers all allowed indexed rows, bypass compression and
  exact-rerank those rows. This is part of the explicit ANN API, not an exact API
  choosing approximate search.

Omitted search widths remain HNSW 64, IVF 2, TurboQuant 512. Width is kind-specific,
not a portable recall guarantee. Callers requiring complete filtered results
should produce explicit graph candidates and use the existing exact
`selene.vector_score_nodes` / batched scoring surface. This does not change
`VectorCandidateSet` meaning or turn native retrieval into a policy engine.

## Evidence and transition ownership

`native_vectors` exercises independently hand-derived unit-vector answers for
all three metrics, ties/limits, typed outputs, graph-scoped metadata, failed-write
rollback, seven ANN kind/metric combinations, WAL-only and checkpoint reopen,
deletion/update/insertion, filtered selection and graph replacement.
`runtime::batch::vector_tests` requires the full vector CALL query to finish in
the physical prefix for input/output windows 1/2/3/7/1024. Lower graph tests inject
failed/partial rebuilds, validate foreign/empty candidates, preserve liveness-only
binding and pin HNSW's selective-filter underfill behavior.

The exact-versus-ANN and batch-versus-single comparisons share engine metric
kernels: they measure candidate coverage and adapter consistency, not independent
numeric conformance. The hand-derived examples do not share those kernels.
The repository-root `BENCHMARKS.md` records CPU-only quality, latency, estimated
memory and build/rebuild cost; these synthetic rows make no model-quality claim.
There is no new vector-specific row bridge. The common unsupported-query row
suffix still has F04-PR09 as deletion owner; it is not deleted prematurely here.
