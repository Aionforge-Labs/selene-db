# Graph Algorithms

This guide is for engineers running graph algorithms over a selene-db property graph: who own a `SharedGraph` (built per [`docs/embedding-guide.md`](embedding-guide.md)) and want to call PageRank, betweenness, Louvain, Dijkstra, or any of the structural primitives, either through the `selene-algorithms` Rust API or through the GQL `CALL algo.*` procedures registered by the native `BuiltinProcedureRegistry` in `selene-gql`.

The grammar for `CALL` is documented in [`docs/gql-reference.md`](gql-reference.md) §8. This document covers what each procedure does, what it expects as input, what it returns, and how the underlying algorithm is parameterized.

## 1. What's available

The [`selene-algorithms`](../crates/selene-algorithms) crate ships **15 algorithms** across four families:

| Family       | Count | Surface                                                                                  |
| :----------- | ----: | :--------------------------------------------------------------------------------------- |
| Structural   |     5 | `wcc`, `scc`, `topological_sort`, `articulation_points`, `bridges` (plus `wcc_count` / `scc_count` counters). |
| Pathfinding  |     3 | `dijkstra` (single source-target), `sssp` (single-source), `apsp` (all-pairs).           |
| Centrality   |     2 | `pagerank` (damped power iteration), `betweenness` (Brandes, optional sampling).         |
| Community    |     3 | `label_propagation` (Raghavan), `louvain` (single-pass modularity), `triangle_count`.    |

Every algorithm is a pure function of a frozen `GraphProjection` — no internal mutation, no hidden state, no async, no globals. The crate depends only on `selene-core` and `selene-graph`; it never touches the parser, the planner, the executor, or persistence.

The sole frozen native [`BuiltinProcedureRegistry`](../crates/selene-gql/src/runtime/builtin_registry.rs) in `selene-gql` binds the same algorithms as **19 `algo.*` procedures** (15 algorithms + 4 projection-management procedures) directly over the native algorithms API, consumable through GQL `CALL`. selene-db is a single native engine — there is no loadable procedure-pack apparatus. The procedure name table is in §7 of this document.

## 2. The projection model

Every algorithm in selene-db operates over a `GraphProjection` — a **frozen, filtered view** of the graph at a specific generation. A projection is not the live graph; it is a snapshot summary built once and reused across many algorithm runs.

### 2.1 Why a projection layer

Algorithms have very different access patterns from row-at-a-time GQL execution:

- They scan every edge many times (PageRank touches each edge once per iteration; APSP runs SSSP from every node).
- They need O(1) neighbor lookup by `NodeId`, ideally over a compact contiguous layout.
- They need stable results: re-running PageRank against a graph mid-mutation would mix two snapshot states.
- They benefit from filtering once (label / edge-type / scope) instead of filtering inside each algorithm.

A projection answers all four needs. It pins a `meta.generation` value, applies the node-label / edge-label / scope filter once, and pre-computes both directions of CSR adjacency. Algorithm code then walks `out_neighbors(node)` and `in_neighbors(node)` as cheap slice reads.

Intrinsic undirected edges appear in both traversal directions with the same
`EdgeId`; `edge_count()` counts logical identities once, not reciprocal arcs.
Directed algorithms interpret these edges as reciprocal connectivity. Community
algorithms count each logical incidence once (including loops), preserving
distinct parallel edges. See the [mixed-edge contract](gql/mixed-edge-orientation.md)
for predicates, orientation unions, and the directed-algorithm interpretation.

### 2.2 Building a projection in Rust

```rust
use selene_algorithms::{GraphProjection, ProjectionConfig};
use selene_core::db_string;

let config = ProjectionConfig {
    name: "person_graph".to_string(),
    node_labels: vec![db_string("Person")?],
    edge_labels: vec![db_string("KNOWS")?, db_string("WORKS_WITH")?],
    weight_property: Some(db_string("strength")?),
};

let snapshot = graph.read();
let projection = GraphProjection::build(&snapshot, &config, None)?;
```

`build` returns a `GraphProjection` keyed by `config.name`. Its key contracts:

- `node_labels: vec![]` means "all alive nodes".
- `edge_labels: vec![]` means "all edge types".
- `weight_property: None` means "all weights are `1.0`" (unweighted).
- The third argument (`scope: Option<&RoaringBitmap>`) lets the caller AND-intersect the node set with an external row bitmap (e.g., for tenant scoping). Pass `None` when not used.
- Missing or non-numeric weight values default to `1.0` per the projection spec. For strict weight validation, preprocess at write time.

The returned projection is immutable. Useful accessors:

```rust
projection.name();                      // &str — projection name
projection.node_count();                // usize — nodes in the filtered view
projection.edge_count();                // usize — outgoing edges total
projection.generation();                // u64  — snapshot generation pinned at build
projection.contains(node_id);           // bool — is this node in the projection
projection.out_neighbors(node_id);      // &[ProjNeighbor] sorted ASC by node_id
projection.in_neighbors(node_id);       // &[ProjNeighbor] sorted ASC by node_id
projection.is_weighted();               // bool — does the projection carry weights
projection.iter_nodes();                // impl Iterator<NodeId> in ASC order
```

`ProjNeighbor` is a `Copy` struct exposing `node_id: NodeId`, `edge_id: EdgeId`, and `weight: f64`.

### 2.3 The catalog

`ProjectionCatalog` is the named-cache layer that lets embedders reuse projections across many algorithm calls — and detect when the underlying graph has moved on.

```rust
use selene_algorithms::ProjectionCatalog;

let catalog = ProjectionCatalog::new();
catalog.project(&graph.read(), &config)?;
// ... later, in another request handler ...
catalog.ensure_fresh(&graph.read(), "person_graph")?;
let projection_ref = catalog.get("person_graph").expect("registered above");
let result = selene_algorithms::pagerank(
    projection_ref.projection(),
    pagerank_config,
);
```

`ensure_fresh` compares the projection's stored `generation` against the snapshot's `meta.generation`:

- **Equal** — no-op; the cached projection is current.
- **Different** — rebuild from the stored `ProjectionConfig`. Catalog projections are always **unscoped**: `project` takes no scope bitmap, so the stored config is the complete rebuild recipe and the rebuild reproduces exactly what was registered. If you need a scoped view, build it directly with `GraphProjection::build` outside the catalog.
- **Not registered** — returns `AlgorithmsError::NoSuchProjection`. The catalog is a cache, not a factory.

`ProjectionRef` holds a read guard for its lifetime. Drop the ref before calling `project`, `drop_projection`, or a rebuilding `ensure_fresh` on the same catalog — otherwise the writer blocks on your read lock.

`ProjectionCatalog` is `Send + Sync`. Per-graph catalogs (the native registry's engine-internal `AlgorithmCatalogs` keeps one `ProjectionCatalog` per `GraphId`) keep tenants from sharing projection names.

### 2.4 Equivalent GQL

```gql
CALL algo.projection_build(
  'person_graph',
  ['Person'],
  ['KNOWS', 'WORKS_WITH'],
  'strength'
)
```

The four projection-management procedures (§7) wrap `ProjectionCatalog` operations and pin the graph by the executor's `GraphContext::graph_id`.

## 3. Structural algorithms

All five surfaces operate on the same projection-view and return ASC-ordered results for stable diffing. Determinism is load-bearing: the SCC and biconnectivity DFS are iterative (no recursion) with neighbor lists sorted by dense index.

### 3.1 Weakly connected components (`wcc` / `wcc_count`)

Treats the projection as **undirected** (union of out- and in-neighbors per node). Returns `(NodeId, component_id)` pairs where `component_id` is the smallest `NodeId` in each component.

```rust
use selene_algorithms::{wcc, wcc_count};

let components: Vec<(NodeId, u64)> = wcc(&projection);
let count: usize = wcc_count(&projection);
```

```gql
CALL algo.wcc('person_graph') YIELD node_id, component_id
RETURN component_id, count(*) AS size
ORDER BY size DESC
```

Complexity: `O((V + E) · α(V))` for the union-find. State arrays are sized by live-node count (not by max-row), so a sparse projection over a graph with many tombstoned deletions stays small.

### 3.2 Strongly connected components (`scc` / `scc_count`)

Iterative Tarjan over the **directed** adjacency (`out_neighbors` only). Same shape as `wcc`: `(NodeId, component_id)` pairs sorted ASC by `NodeId`.

```rust
use selene_algorithms::{scc, scc_count};

let components = scc(&projection);
let count = scc_count(&projection);
```

```gql
CALL algo.scc('person_graph') YIELD node_id, component_id
RETURN component_id, collect(node_id) AS members
ORDER BY size(members) DESC
LIMIT 10
```

Complexity: `O(V + E)`. The DFS uses an explicit work stack (no native recursion) to keep deep call stacks off the thread; a per-DFS neighbor cache amortizes the `proj.out_neighbors` lookup across resume-from-child iterations.

### 3.3 Topological sort (`topological_sort`)

Kahn's algorithm (BFS over in-degree). Returns `(NodeId, topo_position)` pairs ordered by `topo_position` ASC; equal in-degree at any layer breaks by smallest `NodeId` for determinism.

```rust
use selene_algorithms::{topological_sort, TopoSortError};

match topological_sort(&projection) {
    Ok(positions) => { /* DAG */ }
    Err(TopoSortError::NotADag { cycle_hint }) => {
        eprintln!("graph has a cycle (hint: {:?})", cycle_hint);
    }
}
```

```gql
CALL algo.topological_sort('build_order') YIELD node_id, topo_position
RETURN node_id, topo_position
ORDER BY topo_position ASC
```

`cycle_hint: Option<NodeId>` points at a node that retained positive in-degree when the sweep stalled — useful for diagnostics. The procedure adapter raises `ProcedureError::InvalidGraph` carrying the same hint.

Complexity: `O(V + E)`.

### 3.4 Articulation points (`articulation_points`)

Iterative Hopcroft-Tarjan biconnectivity DFS over the **undirected** view. Returns `NodeId`s (cut vertices) sorted ASC.

```rust
use selene_algorithms::articulation_points;

let cut_vertices: Vec<NodeId> = articulation_points(&projection);
```

```gql
CALL algo.articulation_points('network') YIELD node_id
RETURN node_id
```

Complexity: `O(V + E)`. Parallel edges are preserved (the algorithm operates on multi-graph semantics — two parallel friend edges between the same nodes count as two edges); the "parent skip" rule consumes only the first occurrence of the parent in the neighbor list. Subsequent occurrences are legitimate parallel back-edges that update the lowlink.

### 3.5 Bridges (`bridges`)

Shares the biconnectivity pass with `articulation_points` and returns `(source, target)` endpoint pairs with `source.get() < target.get()` canonicalized at insert; outer list sorted ASC by `(source, target)`.

```rust
use selene_algorithms::bridges;

let cut_edges: Vec<(NodeId, NodeId)> = bridges(&projection);
```

```gql
CALL algo.bridges('network') YIELD from_node, to_node
RETURN from_node, to_node
ORDER BY from_node, to_node
```

Complexity: `O(V + E)`.

## 4. Pathfinding

All three pathfinding algorithms operate on the **directed** projection adjacency (`out_neighbors`), with edge weights drawn from `ProjNeighbor::weight`. Weight extraction is infallible at projection build time; **traversal-time** validation catches negative and NaN weights only when the algorithm actually traverses the offending edge.

The deterministic predecessor rule on equal-cost ties is part of the contract. Spec 16 §E16 mandates that when two paths reach the same node at equal cost, Dijkstra's `prev` array records the predecessor with the smaller dense index (= smaller `NodeId` per the projection's ASC ordering). This matters for path reconstruction: ambiguous shortest paths now have one canonical representative.

### 4.1 Dijkstra single-source single-target (`dijkstra`)

```rust
use selene_algorithms::{dijkstra, PathResult, PathfindingError};

let result: Result<Option<PathResult>, PathfindingError> =
    dijkstra(&projection, from_node, to_node);

match result {
    Ok(Some(PathResult { nodes, cost })) => { /* path found */ }
    Ok(None) => { /* from or to not in projection, or no path */ }
    Err(PathfindingError::NegativeWeight { source_node, target_node, weight }) => { /* ... */ }
    Err(PathfindingError::NaNWeight { source_node, target_node }) => { /* ... */ }
    Err(_) => { /* TooLarge does not fire here */ }
}
```

```gql
CALL algo.dijkstra('road_network', $from, $to) YIELD cost, path, length
RETURN cost, path, length
```

`from == to` (both in the projection) returns the zero-step path `PathResult { nodes: vec![from], cost: 0.0 }`. The procedure surface returns zero rows when there is no path or either endpoint is outside the projection.

Complexity: `O((V + E) log V)` with a binary-heap priority queue. Early-exit on settled target is delayed by one heap pop relative to a naive cut-off so the §E16 tie-break has a chance to lock in the canonical predecessor.

### 4.2 Single-source shortest path (`sssp`)

```rust
use selene_algorithms::sssp;

let distances: Vec<(NodeId, f64)> = sssp(&projection, source)?;
```

```gql
CALL algo.sssp('road_network', $source) YIELD target_node, cost
RETURN target_node, cost
ORDER BY cost ASC
LIMIT 100
```

Returns `(NodeId, f64)` pairs sorted ASC by `NodeId`, **including `(source, 0.0)`**, **excluding unreachable nodes**. Source not in projection → `Ok(vec![])`. Same Dijkstra invariants as the single-pair surface; same negative/NaN-weight error variants.

Complexity: `O((V + E) log V)`.

### 4.3 All-pairs shortest path (`apsp`)

Repeated SSSP — one Dijkstra per source. Gated by a caller-supplied `max_nodes` to make the quadratic-time blow-up explicit at call sites.

```rust
use selene_algorithms::{apsp, ApspConfig, Parallelism};
use std::num::NonZeroUsize;

let config = ApspConfig {
    max_nodes: 1_000,
    parallelism: Parallelism::Auto,
};
let pairs: Vec<(NodeId, NodeId, f64)> = apsp(&projection, config)?;
```

```gql
CALL algo.apsp('road_network', 1000, NULL) YIELD source_node, target_node, cost
RETURN source_node, target_node, cost
```

`PathfindingError::TooLarge { nodes, limit }` fires when `projection.node_count() > config.max_nodes`. Self-pairs are excluded; unreachable pairs are excluded. Results sorted ASC by `(source, target)`.

Complexity: `O(N · (V + E) log V)`. APSP is the workhorse for parallelism in this crate; see §8 for parallel speedups.

## 5. Centrality

Both centrality algorithms operate on the **directed** view and return rankings: `Vec<(NodeId, f64)>` sorted **DESC by score** with **NodeId ASC** tie-break on equal scores. This is asymmetric to the structural-family ASC-by-NodeId result shape — centrality outputs are scoring rankings, not set positions.

### 5.1 PageRank (`pagerank`)

Standard damped power iteration with bulk-applied dangling-node mass:

```text
new[v] = (1 - damping) / N + damping * Σ score[u] / out_degree(u)
```

```rust
use selene_algorithms::{pagerank, PageRankConfig, Parallelism};

let config = PageRankConfig {
    damping: 0.85,
    max_iter: 100,
    tolerance: 1e-6,
    parallelism: Parallelism::Auto,
};
let ranks: Vec<(NodeId, f64)> = pagerank(&projection, config);
```

```gql
CALL algo.pagerank('person_graph', 0.85, 100, 1e-6, NULL)
YIELD node_id, score
RETURN node_id, score
ORDER BY score DESC
LIMIT 20
```

The GQL procedure also accepts trailing optional arguments for orientation,
personalization, result-label/result-node filtering, and indexed edge-property
result filters. The edge filter group is
`edge_filter_label, edge_filter_property, edge_filter_values,
edge_filter_endpoint`; endpoint values admit the matching edge `source`,
`target`, or `both` endpoints as PageRank result candidates.

Contract:

- `damping` must be finite and in `[0.0, 1.0)`. The exclusive upper bound preserves the teleport floor that gives the power iteration a convergence guarantee.
- `max_iter == 0` returns the uniform `1/N` initial scores immediately.
- `tolerance == 0.0` runs all `max_iter` iterations regardless.
- Termination: `max |new[v] - score[v]| < tolerance`.

Dangling nodes (out-degree 0) redistribute their mass evenly across all N nodes via a single bulk-apply pass per iteration (one accumulator + one sweep), not a per-dangling full-scan. On sink-heavy graphs this is the difference between O(N + E) and O(N · D).

Complexity per iteration: `O(N + E)`.

### 5.2 Brandes betweenness (`betweenness`)

For each source, BFS to compute σ (shortest-path counts) and δ (dependencies); accumulate δ values into the centrality vector. Supports endpoint-aware **deterministic sampling** for approximate computation:

```rust
use selene_algorithms::{betweenness, BetweennessConfig, Parallelism};

let config = BetweennessConfig {
    sample_size: Some(64),        // None = exact (every node is a source)
    parallelism: Parallelism::Auto,
};
let centrality: Vec<(NodeId, f64)> = betweenness(&projection, config);
```

```gql
CALL algo.betweenness('person_graph', 64, NULL)
YIELD node_id, score
RETURN node_id, score
ORDER BY score DESC
LIMIT 20
```

When `sample_size = Some(k)` with `0 < k < node_count`:

- Sources are sampled deterministically using `i * (n - 1) / (k - 1)` indexing, which lands at both endpoints and spreads intermediate samples evenly. (The naive `step = n/k` formula biases toward low NodeIds and never reaches the tail.)
- The final centrality vector is scaled by `node_count / k`.

`Some(0)` returns zero centrality for every node. `Some(k)` with `k >= node_count` is equivalent to `None` (exact computation).

Complexity: `O(V · (V + E))` exact; `O(k · (V + E))` sampled.

## 6. Community

Community algorithms operate on the **undirected** view (union of out- and in-neighbors). The three return shapes differ deliberately:

- `label_propagation`: `(NodeId, community_id)` ASC by NodeId.
- `louvain`: `(NodeId, community_id, level)` ASC by NodeId (`level` reserved at `0` for forward-compat with future hierarchical Louvain).
- `triangle_count`: `(NodeId, count)` DESC by count, NodeId ASC tie-break.

### 6.1 Label propagation (`label_propagation`)

Raghavan-style asynchronous-deterministic propagation. Each node starts with its own `NodeId` as label; each iteration visits nodes in ASC `NodeId` order and adopts the most common label among its undirected neighbors. Ties break by smallest label ID. Updates are immediately visible within the same iteration.

```rust
use selene_algorithms::label_propagation;

let communities: Vec<(NodeId, u64)> = label_propagation(&projection, 50);
```

```gql
CALL algo.label_propagation('person_graph', 50) YIELD node_id, community
RETURN community, count(*) AS size
ORDER BY size DESC
```

Converges when no labels change in an iteration, or `max_iter` is reached. `max_iter == 0` returns the initial state (each node in its own community). Current label propagation uses unit weights; weighted propagation is deferred. Isolated nodes retain their initial label.

Complexity per iteration: `O(V + E)` for the visit + `O(d · log d)` per node for the per-node label-frequency tally, where `d` is degree.

### 6.2 Louvain (`louvain`)

Single-pass modularity optimization. Each iteration evaluates the modularity gain for moving each node into each neighboring community; moves only on strictly-positive gain. Iterates candidate communities in sorted-by-ID order to defuse `HashMap` iteration-order non-determinism.

```rust
use selene_algorithms::louvain;

let assignments: Vec<(NodeId, u64, u32)> = louvain(&projection, 50);
```

```gql
CALL algo.louvain('person_graph', 50) YIELD node_id, community, level
RETURN community, count(*) AS size
ORDER BY size DESC
```

The `u32 level` is always `0` for the current single-pass Louvain implementation; the slot is reserved for hierarchical Louvain (multi-level contraction) in a future release. Edge weights are projected via `ProjNeighbor::weight` (unit weights when the projection is unweighted). `max_iter == 0` returns the initial state.

Determinism: with a fixed projection and fixed `max_iter`, Louvain produces the same assignment every run. No RNG is involved.

Complexity per iteration: `O(V + E)` for the per-node sweep + per-candidate gain evaluation.

### 6.3 Triangle count (`triangle_count`)

Per-node count of triangles in the undirected view. Triangles are 3 distinct mutually-connected nodes; self-loops do not form triangles; parallel edges collapse to a single neighbor (via `sort_unstable() + dedup()`).

```rust
use selene_algorithms::{triangle_count, TriangleCountConfig, Parallelism};

let config = TriangleCountConfig {
    parallelism: Parallelism::Auto,
};
let triangles: Vec<(NodeId, usize)> = triangle_count(&projection, config);
let total: usize = triangles.iter().map(|(_, c)| c).sum::<usize>() / 3;
```

```gql
CALL algo.triangle_count('person_graph', NULL)
YIELD node_id, triangle_count
RETURN node_id, triangle_count
ORDER BY triangle_count DESC
LIMIT 20
```

Algorithm: for each node u, for each pair (v, w) of u's neighbors with `v < w`, binary-search `adj[v]` for w. Each triangle contributes 1 count to each of its 3 vertices, so the total triangle count is `sum / 3`.

Complexity: `O(V · d²)` worst case where `d` is the max undirected degree. Suitable for moderately-dense graphs; for very dense graphs (`d² > V`) prefer matrix-multiplication approaches outside this crate.

## 7. The 19 `algo.*` procedures

| Procedure                  | Signature                                                                                                | Output columns                                       |
| :------------------------- | :------------------------------------------------------------------------------------------------------- | :--------------------------------------------------- |
| `algo.projection_build`    | `(name: STRING, node_labels: LIST<STRING>?, edge_labels: LIST<STRING>?, weight_property: STRING?)`       | (none)                                               |
| `algo.projection_get`      | `(name: STRING)`                                                                                         | `name, generation, node_count, edge_count`           |
| `algo.projection_drop`     | `(name: STRING)`                                                                                         | (none)                                               |
| `algo.projection_list`     | `()`                                                                                                     | `name, generation, node_count, edge_count` per entry |
| `algo.wcc`                 | `(projection_name: STRING)`                                                                              | `node_id, component_id`                              |
| `algo.scc`                 | `(projection_name: STRING)`                                                                              | `node_id, component_id`                              |
| `algo.wcc_count`           | `(projection_name: STRING)`                                                                              | `count`                                              |
| `algo.scc_count`           | `(projection_name: STRING)`                                                                              | `count`                                              |
| `algo.topological_sort`    | `(projection_name: STRING)`                                                                              | `node_id, topo_position`                             |
| `algo.articulation_points` | `(projection_name: STRING)`                                                                              | `node_id`                                            |
| `algo.bridges`             | `(projection_name: STRING)`                                                                              | `from_node, to_node`                                 |
| `algo.dijkstra`            | `(projection_name: STRING, from: NODE, to: NODE)`                                                        | `cost, path, length` (single row, or zero rows)      |
| `algo.sssp`                | `(projection_name: STRING, source: NODE)`                                                                | `target_node, cost`                                  |
| `algo.apsp`                | `(projection_name: STRING, max_nodes: INTEGER, parallelism: INTEGER?)`                                   | `source_node, target_node, cost`                     |
| `algo.pagerank`            | `(projection_name: STRING, damping: FLOAT?, max_iterations: INTEGER?, tolerance: FLOAT?, parallelism: INTEGER?, orientation: STRING?, personalization: LIST<RECORD>?, result_label: STRING?, limit: INTEGER?, result_nodes: LIST<NODE>?, edge_filter_label: STRING?, edge_filter_property: STRING?, edge_filter_values: LIST<VALUE>?, edge_filter_endpoint: STRING?)` | `node_id, score`                              |
| `algo.betweenness`         | `(projection_name: STRING, sample_size: INTEGER?, parallelism: INTEGER?)`                                | `node_id, score`                                     |
| `algo.label_propagation`   | `(projection_name: STRING, max_iter: INTEGER?)`                                                          | `node_id, community`                                 |
| `algo.louvain`             | `(projection_name: STRING, max_iter: INTEGER?)`                                                          | `node_id, community, level`                          |
| `algo.triangle_count`      | `(projection_name: STRING, parallelism: INTEGER?)`                                                       | `node_id, triangle_count`                            |

The table above is the canonical list of all 19 `algo.*` names; the live registry enumerates them (alongside the 50 `selene.*` platform built-ins, 69 total) through `BuiltinProcedureRegistry::iter_handles`, which backs `SHOW PROCEDURES`.

### 7.1 Nullable arguments and defaults

Arguments marked `?` accept `NULL` and resolve to documented defaults:

| Algorithm         | Argument          | Default on `NULL`                                                  |
| :---------------- | :---------------- | :----------------------------------------------------------------- |
| `algo.pagerank`   | `damping`         | `0.85` (`native_algorithms::centrality::DEFAULT_DAMPING`)          |
| `algo.pagerank`   | `max_iterations`  | `100` (`native_algorithms::centrality::DEFAULT_MAX_ITERATIONS`)    |
| `algo.pagerank`   | `tolerance`       | `1e-6` (`native_algorithms::centrality::DEFAULT_TOLERANCE`)        |
| `algo.pagerank`   | `orientation`     | `natural`                                                          |
| `algo.pagerank`   | result filters    | `NULL` (all matching nodes)                                        |
| `algo.label_propagation` | `max_iter` | `50`                                                               |
| `algo.louvain`    | `max_iter`        | `50`                                                               |
| `algo.betweenness`| `sample_size`     | `None` (exact computation; every node is a source)                 |
| any `parallelism` | `parallelism`     | `Parallelism::Auto` (current Rayon pool)                           |

### 7.2 Parallelism argument encoding

Every procedure that has a `parallelism: INTEGER?` slot uses this encoding:

| GQL value    | Rust value                            | Meaning                                  |
| :----------- | :------------------------------------ | :--------------------------------------- |
| `NULL`       | `Parallelism::Auto`                   | Current Rayon pool, or global pool size. |
| `0`          | `Parallelism::Sequential`             | Force single-threaded.                   |
| `n > 0`      | `Parallelism::Threads(NonZeroUsize)`  | Use exactly `n` threads.                 |
| negative     | (rejected)                            | `ProcedureError::InvalidArgument`.       |
| `> 1024`     | (rejected)                            | Adapter-side cap to prevent runaway.     |

### 7.3 Worked end-to-end example

```gql
-- Build the projection once.
CALL algo.projection_build(
  'social',
  ['Person'],
  ['KNOWS'],
  NULL
);

-- Run PageRank; rely on defaults for damping/max_iter/tolerance.
CALL algo.pagerank('social', NULL, NULL, NULL, NULL)
YIELD node_id, score
RETURN node_id, score
ORDER BY score DESC
LIMIT 10;

-- Inspect projection status.
CALL algo.projection_list()
YIELD name, generation, node_count, edge_count
RETURN name, generation, node_count, edge_count;

-- Drop when done.
CALL algo.projection_drop('social');
```

## 8. Parallelization

Four algorithms have rayon-parallel implementations: `apsp`, `betweenness`, `pagerank`, `triangle_count`. The remaining algorithms are sequential-only — either because they would not benefit (the structural primitives are already O(V + E) and dominated by graph traversal) or because their data dependencies (Louvain's strictly-sequential per-node moves) preclude clean parallelization.

### 8.1 How parallelization is gated

Every parallel-aware procedure accepts a `Parallelism` enum:

```rust
pub enum Parallelism {
    Sequential,
    Auto,                      // current Rayon pool (default)
    Threads(NonZeroUsize),     // explicit non-zero thread count
}
```

`Auto` uses the ambient Rayon pool; `Threads(n)` builds a fresh `ThreadPool` with `n` workers and installs the work inside it. Outside Rayon, `Auto` falls back to the global default pool size. `Parallelism::default() == Parallelism::Auto`.

### 8.2 Real numbers from BENCHMARKS.md

From [BENCHMARKS.md](../BENCHMARKS.md) §4, measured on Apple M5 (10 physical cores):

| Algorithm        | Scale   | Sequential | Auto       | Speedup |
| :--------------- | ------: | ---------: | ---------: | ------: |
| `betweenness`    | 100k    | 264.7 ms   | **110.2 ms** | 2.40×   |
| `betweenness`    | 50k     | 128.4 ms   | 46.59 ms   | 2.76×   |
| `betweenness`    | 10k     | 25.05 ms   | 8.60 ms    | 2.91×   |
| `apsp`           | 1k      | 34.54 ms   | **8.46 ms**  | 4.08×   |
| `apsp`           | 500     | 8.57 ms    | 2.19 ms    | 3.92×   |
| `apsp`           | 200     | 1.52 ms    | 466.1 µs   | 3.27×   |
| `triangle_count` | 100k    | 10.33 ms   | 8.86 ms    | 1.17×   |
| `pagerank`       | 100k    | 2.94 ms    | 4.49 ms    | 0.65×   |
| `pagerank`       | 10k     | 245.5 µs   | 524.1 µs   | 0.47×   |

Two notes worth internalizing:

- **APSP and betweenness are the workhorses of parallelism on selene-db.** They run an independent SSSP per source, which is embarrassingly parallel.
- **PageRank/Auto is slower than PageRank/Sequential on the default bench fixture (~3 edges/node).** Per-iteration work (3·N FP multiplications) doesn't outweigh Rayon's thread-coordination cost on a sparse graph. On dense graphs the picture flips. Use `Parallelism::Sequential` explicitly when your graph is sparse and you've measured.

### 8.3 Determinism under parallelism

`betweenness`, `triangle_count`, and `apsp` produce the same result under Sequential and Auto (the accumulator merge is associative; the source list is enumerated up front). `pagerank` is deterministic to within floating-point reduction-order; document this if your downstream consumer is bit-comparing scores.

`louvain` has no parallel mode — its per-node move loop is strictly sequential by construction.

## 9. GraphProjection caching

### 9.1 Generation-based staleness

`SeleneGraph::meta.generation` advances monotonically on every committed write transaction. `GraphProjection::generation()` records the generation pinned at build time. The cache trigger is exact equality:

```text
projection.generation() == snapshot.meta.generation  → fresh
projection.generation() != snapshot.meta.generation  → stale, rebuild
```

`ProjectionCatalog::ensure_fresh` runs this check on every call. The fast path (cached + fresh) takes only a read-lock acquisition.

### 9.2 When to invalidate

You generally do **not** need to call `drop_projection` manually. `ensure_fresh` rebuilds from the stored config when the generation advances. Manual invalidation is needed only when:

- The original `ProjectionConfig` no longer reflects what you want to query (new label filter, new weight property).
- You need a scoped view (`scope: Option<&RoaringBitmap>`). The catalog never holds scoped projections — `project` does not accept a scope, because the stored config could not retain it across rebuilds. Build the scoped projection directly with `GraphProjection::build` and rebuild it yourself when the generation advances.
- You want to free the memory eagerly (a 100k-node projection holds two CSR adjacency arrays plus a `RoaringBitmap`; large graphs benefit from explicit drop).

### 9.3 Cost shapes

| Operation                              | Cost                                                          |
| :------------------------------------- | :------------------------------------------------------------ |
| `ProjectionCatalog::ensure_fresh` (fresh) | One read-lock acquisition + one generation comparison.     |
| `ProjectionCatalog::ensure_fresh` (stale) | Read-lock check + write-lock acquisition + full rebuild.   |
| `GraphProjection::build`               | O(V + E) over the filtered subgraph; one bitmap intersection + two CSR builds. |
| `projection.out_neighbors(node)`       | O(1) slice lookup once the row index is known.                |

For the `CALL algo.*` surface, the cache lives inside the native registry's engine-internal `AlgorithmCatalogs`. One `ProjectionCatalog` is keyed per `GraphId`, so per-tenant graph isolation extends to projections automatically.

## 10. Performance

Headline numbers from [BENCHMARKS.md](../BENCHMARKS.md):

| Bench                      | Scale                                | Median           |
| :------------------------- | :----------------------------------- | ---------------: |
| `pagerank` (sequential)    | 100k (≈3 edges/node)                 | **2.94 ms**      |
| `betweenness` (Auto)       | 100k                                 | **110.2 ms**     |
| `triangle_count` (Auto)    | 100k (≈6 edges/node, ~N/64 communities) | **8.86 ms**   |
| `louvain` (sequential)     | 100k (planted communities)           | **55.43 ms**    |
| `apsp` (Auto)              | 1k                                   | **8.46 ms**      |

Invoking an algorithm through `CALL algo.*` adds GQL parse + plan + execute overhead per call, on top of the algorithm itself — that pipeline, not the underlying computation, dominates the per-call cost.

If you are calling the same algorithm many times in a tight loop, prefer the Rust API over `CALL`: you save the parse + plan + execute pipeline per invocation and reuse the projection directly.

## 11. Custom algorithms

There are two extension paths.

### 11.1 Add a function to `selene-algorithms`

For algorithms that fit the projection-based model (read-only over a frozen view), add a crate-local function:

1. Pick a family module (`structural`, `pathfinding`, `centrality`, `community`) or create a new one.
2. Take `&GraphProjection` (plus any caller-supplied config struct).
3. Use `RowIndex` from `selene-algorithms::structural::row_index` to translate sparse `NodeId`s to dense indices for state arrays.
4. Walk `proj.out_neighbors(node)` / `proj.in_neighbors(node)`; both return slices sorted ASC by `node_id`.
5. Return a `Vec<...>` sorted per the family's contract (structural: ASC by `NodeId`; centrality: DESC by score with NodeId ASC tie-break; community: see §6 for per-algorithm shape).
6. For NaN-soundness, use `f64::total_cmp` for any float ordering.
7. Re-export from `selene_algorithms::lib.rs`.

For parallelism, follow the pattern in `centrality::pagerank` or `pathfinding::apsp`: dispatch on `Parallelism`, install a `ParallelRunner`, then use `rayon::prelude` inside the closure.

### 11.2 Expose a new algorithm via `CALL`

selene-db is a single native engine — there is no procedure-pack apparatus. To surface a new algorithm through GQL `CALL`, wire it directly into the one native registry:

1. Add a native free function for the algorithm in `selene-algorithms` (and, if useful, a `GraphAlgorithms` trait method) per §11.1.
2. Register an `algo.<name>` procedure in `selene_gql::runtime::builtin_registry::BuiltinProcedureRegistry`, calling the native API directly (no external-procedure indirection). Mirror an existing `algo.*` procedure for the argument-coercion and YIELD-column contract.
3. The registry is frozen (`registry_version()` constant `0`); the procedure set is fixed at construction.

## See also

- [`docs/embedding-guide.md`](embedding-guide.md) — embedder workflow and registry wiring.
- [`docs/gql-reference.md`](gql-reference.md) §8 — `CALL ... YIELD` grammar.
- [`BENCHMARKS.md`](../BENCHMARKS.md) §4 and §5 — algorithm and adapter benchmarks.
- [`crates/selene-algorithms`](../crates/selene-algorithms) — algorithm sources + the native Rust API.
- [`crates/selene-gql/src/runtime/builtin_registry.rs`](../crates/selene-gql/src/runtime/builtin_registry.rs) — the sole frozen native `BuiltinProcedureRegistry` binding `CALL algo.*`.
