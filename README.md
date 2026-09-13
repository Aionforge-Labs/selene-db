# selene-db

`selene-db` is an embeddable Rust property-graph engine built around
ISO/IEC 39075:2024 GQL, native graph algorithms, first-class dense vectors,
first-class JSON metadata, BM25 full-text search, and local persistence.

It is a library, not a service. There is no bundled database server, wire
protocol, auth layer, cloud control plane, extension loader, or procedure-pack
system. Applications depend on the `selene-db` facade, own their network and
security boundary, and run the engine in process. Lower workspace crates remain
available for advanced engine work without the facade's 2.x stability promise.

The project north star is strict ISO GQL at the language boundary plus
pragmatic native retrieval primitives for graph-heavy and agentic-memory
workloads. Non-standard capabilities are exposed through implementation-defined
values, indexes, and `CALL selene.*` / `CALL algo.*` procedures, not by adding
SQL, Cypher, SPARQL, or ad hoc grammar.

## Release Status

The active source line is `2.0.0-alpha.1`. The 1.x line is end of life: it
receives no fixes, security patches, compatibility work, new releases, or data
migration support. The alpha coordinate may not yet be published; use path
dependencies from a source checkout when it is unavailable on crates.io. See
the [2.0 line and 1.x end-of-life policy](docs/v2/eol-and-version-policy.md).
The [tracked 2.0 program](docs/v2/README.md) owns finalized decisions,
milestones, work-item contracts, review roles, issue ownership, and
evidence-gated conformance wording.
The [release-readiness note](docs/v2/release-readiness.md) separates the current
embedding contract, executable regression evidence and unresolved claim gaps.

## What Is Here

| Area | Current surface |
|---|---|
| Facade | Memory-only builder or fallible format-2 create/open, catalog-owned schemas and named graphs, database-owning sessions, structured diagnostics, and immutable result rows with declared descriptors. |
| GQL | Parser, analyzer, planner, optimizer, executor, parameter binding, source-string plan cache, feature-status reporting, and ISO-oriented errors. |
| Graph storage | In-memory property graph with stable external IDs, dense internal rows, immutable reader snapshots, typed property indexes, composite indexes, and one mutation funnel. |
| Transactions | Serialized writers, snapshot readers, rollback by non-publication, and provider fanout under the write lock. |
| Persistence | WAL, snapshots, MANIFEST-led recovery, retention pruning, audit-log primitives, and rebuildable provider state. |
| Procedures | Native `BuiltinProcedureRegistry` for health/feature reporting, index DDL, vector search/scoring, JSON candidate production, BM25 search/scoring, maintenance, and `algo.*` graph algorithms. |
| Algorithms | Structural, pathfinding, centrality, and community algorithms through `selene-algorithms` and GQL `CALL algo.*` adapters. |
| Vectors | `Value::Vector`, exact scoring/search, HNSW and IVF indexes, batch scoring, graph-expanded scoring, maintained candidate state, index stats, and rebuild APIs. |
| JSON | `Value::Json`, typed GQL properties/parameters/casts, bounded selectors, containment/path predicates, and exact JSON candidate search. |
| Text | BM25 exact scan, maintained text indexes, candidate-scoped scoring, batched scoring, graph/state-expanded scoring, and text-index stats. |
| Safety | Workspace-wide `#![forbid(unsafe_code)]`, `missing_docs = "deny"`, rustls-only dependency posture, file-size checks, secret scans, license checks, fuzz targets, and benchmark hygiene guards. |

## Workspace Crates

`selene-db` is the stability-promised embedding surface. The other crates are
advanced/internal boundaries unless the facade intentionally re-exports a
type.

| Crate | Owns |
|---|---|
| [`selene-db`](crates/selene-db) | Stable database builder, catalog lifecycle, selected graph sessions, facade errors, and summary outcomes. |
| [`selene-catalog`](crates/selene-catalog) | Immutable catalog descriptors, stable typed IDs, canonical names, read snapshots, and storage-neutral mutation drafts. |
| [`selene-profile`](crates/selene-profile) | Generated GQL profile, implementation-choice, and conformance-evidence authority. |
| [`selene-core`](crates/selene-core) | Foundation types: `Value`, `VectorValue`, `JsonValue`, IDs, `DbString`, labels, property maps, schema metadata, codecs, origins, changesets, and core vector kernels. |
| [`selene-graph`](crates/selene-graph) | Graph storage, transactions, mutation funnel, property/composite/vector/text indexes, exact and ANN vector search, BM25 search, exact JSON search, maintained candidate state, graph type validation, compaction, and recovery providers. |
| [`selene-persist`](crates/selene-persist) | WAL, snapshots, MANIFEST recovery, retention pruning, and audit log files. It stays below graph semantics. |
| [`selene-algorithms`](crates/selene-algorithms) | Native graph algorithms, projection catalogs, free functions, and the `GraphAlgorithms` convenience trait. |
| [`selene-gql`](crates/selene-gql) | GQL grammar, AST, analysis, planning, optimization, execution, procedure traits, and the built-in procedure registry. |
| [`selene-testing`](crates/selene-testing) | Test fixtures, graph generators, benchmark corpora, opt-in local/remote embedding helpers, and snapshot-harness utilities. |

The main engine dependency direction is:

```text
selene-core -> selene-graph -> selene-algorithms -> selene-gql -> selene-db
```

`selene-profile` feeds runtime profile consumers. `selene-catalog` is
storage-neutral and has no engine-crate dependencies; `selene-db` privately
composes the catalog and engine layers. `selene-persist` depends on
`selene-core` and remains graph-blind. `selene-testing` is for tests and
benchmarks.

## Quickstart

The example follows the current source coordinate; confirm the alpha is
published before using it as a registry dependency. A source checkout can use
the equivalent path dependency.

```toml
[dependencies]
selene-db = { version = "2.0.0-alpha.1" }
```

Build an in-memory database, create a schema and named graph, then select that
graph for a facade session:

```rust
use selene_db::{CreatePolicy, Database, ObjectPath, SchemaPath};

fn main() -> Result<(), selene_db::Error> {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let schema = SchemaPath::regular("selene", "memory")?;
    catalog.create_schema(&schema, CreatePolicy::Strict)?;
    let graph_path = ObjectPath::regular("selene", "memory", "episodes")?;
    catalog.create_graph(&graph_path, None, CreatePolicy::Strict)?;
    let session = database.session(&graph_path)?;

    session.execute("INSERT (:Person { name: 'Ada' })")?;
    let output = session.execute("MATCH (p:Person) RETURN p")?;
    assert_eq!(output.row_count(), Some(1));

    Ok(())
}
```

The facade returns immutable row values, analyzer-declared result descriptors,
and structured diagnostics. A session retains the selected graph's stable
identity and revalidates it for each request; drop or replacement makes the old
session stale. Transaction and supported `SESSION` controls use facade-owned
state. Sessions are `Send` and intentionally not `Sync`; serialize each session's
requests. GQL catalog DDL routes to the same catalog lifecycle service used above.
The [Embedding Guide](docs/embedding-guide.md) identifies the stable entry point
and documents the lower advanced APIs separately.

## GQL Boundary

GQL is the only query and mutation language. The parser rejects grammar drift
instead of accepting other languages as aliases.

Examples of supported ISO-oriented surface:

```gql
MATCH (p:Person)-[:KNOWS]->(friend)
WHERE p.name = 'Ada'
RETURN friend.name AS name
ORDER BY name
LIMIT 10
```

```gql
INSERT (:Person {name: 'Grace'})
FINISH
```

```gql
CALL selene.feature_status()
YIELD feature_id, status, rationale, feature_name, surface,
      profile_relation, claim_state, evidence_status, evidence_count, profile_hash
RETURN feature_id, status, surface, profile_hash
```

Quantified edge bindings are list-valued. Projecting a property from one of
those bindings returns a path-ordered list of property values, with `NULL` for
missing element properties:

```gql
MATCH (p:Person)-[r:KNOWS*1..3]->(friend)
RETURN r.score AS path_scores
```

Native procedures are part of the engine, not loadable extensions. Tests can
inject alternate `ProcedureRegistry` implementations, but production code uses
the in-tree `BuiltinProcedureRegistry`.

The facade's Rust `GraphTypeDefinition` builder supports closed graph schemas,
typed properties and recursively validated durable defaults, including lists,
records, JSON and vectors. This does **not** imply complete GG02 GQL graph-type
syntax: the GQL catalog subset remains property-free named node types. Do not
use historical `CREATE/ALTER NODE TYPE` or `ALTER EDGE TYPE` recipes as facade
grammar. See the [schema/reopen contract](docs/v2/checkpoint-reopen.md) and the
[runnable facade examples](docs/v2/roadmap/examples/facade_release.rs).

## Native Retrieval

The retrieval stack is deliberately composable. selene-db provides graph,
vector, text, and algorithm primitives; embedders decide the memory or search
policy that fits their application.

Graph reachability can produce bounded transitive candidate windows from
explicit roots before vector, text, JSON, or fusion scoring:

```gql
CALL selene.reachable_nodes($heads, 'SUPPORTS', 1000, 3, 'outgoing')
YIELD node_id, depth
RETURN node_id, depth
ORDER BY depth, node_id
```

### Vectors

Vectors are first-class values:

- node properties can store `Value::Vector(VectorValue)`;
- GQL can produce vectors through `CAST(<LIST<numeric>> AS VECTOR)`;
- `VECTOR` properties can use numeric list defaults, for example
  `embedding :: VECTOR DEFAULT [0.0, 0.0, 0.0]`;
- components are finite `f32` values behind shared storage;
- supported metrics are `squared_euclidean`, `cosine`, and
  `negative_inner_product`;
- exact scoring is the correctness oracle;
- HNSW, IVF, and TurboQuant indexes provide in-memory ANN paths;
- graph-expanded and maintained-state scorers compose topology with vectors;
- rebuild and stats procedures make derived index state observable.

Representative GQL calls:

```gql
RETURN CAST([0.12, 0.34, 0.56] AS VECTOR) AS query_embedding
```

```gql
CALL selene.create_vector_index('Document', 'embedding', 1536, 'hnsw', NULL, 'cosine')
```

```gql
CALL selene.create_vector_index('Document', 'embedding', 1536, 'turbo_quant')
```

```gql
CALL selene.vector_search_nodes_ann(
  'Document', 'embedding', $query, 10, 'cosine', 64
)
YIELD node_id, distance
RETURN node_id, distance
```

Global ANN search can also admit candidates through node-property filters and
indexed edge-property filters. Pass `NULL, NULL` for the node filter when the
candidate gate is edge-only, and choose the admitted endpoint with
`'source'`, `'target'`, or `'both'`.

```gql
CALL selene.vector_search_nodes_ann(
  'Document', 'embedding', $query, 10, 'cosine', 64,
  NULL, NULL, 'SUPPORTS', 'confidence', [0.9, 1.0], 'target'
)
YIELD node_id, distance
RETURN node_id, distance
```

```gql
CALL selene.vector_score_candidate_state_expanded_batch(
  'embedding', $queries, 'current_support_facts',
  $root_sets, 'SUPPORTS', 10, 'intersection', 'outgoing', 'cosine'
)
YIELD query_index, node_id, distance
RETURN query_index, node_id, distance
```

#### Vector Performance Anchors

Vector benchmarks are run through `scripts/run-benches.sh` so candidate search,
exact rerank, storage accounting, and benchmark docs stay tied to the same
fixture definitions. The current production TurboQuant cosine rows use a
10,000-node clustered fixture with eight representative queries, exact cosine
rerank against primary `VECTOR` properties, and `recallbp10000` against the
exact top-k oracle. The latency column is the measured eight-query Criterion
workload; plain TurboQuant rows issue independent single-query calls, while
fused batch rows score the query batch together.

Command:

```bash
scripts/run-benches.sh --profile quick --bench vector_turbo_projection
```

| Path | Dimension | Candidate scope | Workload latency | Recall | Index storage |
|---|---:|---:|---:|---:|---:|
| TurboQuant cosine | 128 | 10k rows | 3.2961 ms | 10000 bp | 0.8 MiB index / 4.9 MiB vectors |
| TurboQuant cosine | 768 | 10k rows | 5.3861 ms | 10000 bp | 3.9 MiB index / 29.3 MiB vectors |
| TurboQuant cosine | 1536 | 10k rows | 7.8667 ms | 10000 bp | 7.6 MiB index / 58.6 MiB vectors |
| Fused batch TurboQuant | 128 | 10k rows x 8 queries | 2.3379 ms | 10000 bp | 0.8 MiB index / 4.9 MiB vectors |
| Fused batch TurboQuant | 768 | 10k rows x 8 queries | 4.5446 ms | 10000 bp | 3.9 MiB index / 29.3 MiB vectors |
| Fused batch TurboQuant | 1536 | 10k rows x 8 queries | 7.1180 ms | 10000 bp | 7.6 MiB index / 58.6 MiB vectors |
| Filtered TurboQuant | 128 | 4,243 candidates/query | 3.0005 ms | 10000 bp | 0.8 MiB index / 4.9 MiB vectors |
| Filtered TurboQuant | 768 | 4,243 candidates/query | 5.0961 ms | 10000 bp | 3.9 MiB index / 29.3 MiB vectors |
| Filtered TurboQuant | 1536 | 4,243 candidates/query | 7.6167 ms | 10000 bp | 7.6 MiB index / 58.6 MiB vectors |
| Filtered batch TurboQuant | 128 | 4,243 candidates/query x 8 queries | 2.4093 ms | 10000 bp | 0.8 MiB index / 4.9 MiB vectors |
| Filtered batch TurboQuant | 768 | 4,243 candidates/query x 8 queries | 3.9310 ms | 10000 bp | 3.9 MiB index / 29.3 MiB vectors |
| Filtered batch TurboQuant | 1536 | 4,243 candidates/query x 8 queries | 6.0436 ms | 10000 bp | 7.6 MiB index / 58.6 MiB vectors |

These are quick-profile Criterion anchors, not fixed latency guarantees. The
compressed index is derived state: primary vectors remain the source of truth,
and approximate candidate paths exact-rerank against those primary values before
returning distances. Filtered TurboQuant is intended for graph/state-gated
candidate windows; filtered batch search handles multi-query workloads with one
candidate window per query.

### BM25 Text

BM25 is native over string node properties:

- exact scan remains the small-corpus oracle;
- maintained text indexes keep rebuildable postings for repeated queries;
- candidate-scoped scoring lets graph-produced node sets feed BM25 directly;
- batch and maintained-state-expanded procedures support multi-query
  retrieval rows.

Representative GQL calls:

```gql
CALL selene.create_text_index('Document', 'body')
```

```gql
CALL selene.text_search_nodes('Document', 'body', $query, 10)
YIELD node_id, score
RETURN node_id, score
```

```gql
CALL selene.text_search_nodes(
  'Document', 'body', $query, 10,
  NULL, NULL, 'CITES', 'kind', ['primary'], 'source'
)
YIELD node_id, score
RETURN node_id, score
```

```gql
CALL selene.text_score_nodes_batch('Document', 'body', $queries, $candidates, 10)
YIELD query_index, node_id, score
RETURN query_index, node_id, score
```

### JSON Metadata

JSON is a first-class value for structured agent payloads and metadata:

- node properties can store `Value::Json(JsonValue)`;
- `JSON` participates in GQL typed parameters, typed predicates, and casts;
- closed graph schemas can declare canonical JSON defaults with a string literal
  such as `payload :: JSON DEFAULT '{"kind":"episodic"}'`;
- scalar functions provide parse/stringify/type, array/object construction,
  array/object shape introspection, bounded variadic and selector-array path
  selectors, native scalar leaf extraction, existence checks, recursive
  containment, RFC 7396 merge-patch updates, and RFC 6902 JSON Patch updates;
  `json_object` constructor keys must be unique;
- `selene.json_contains_nodes`, `selene.json_path_exists_nodes`,
  `selene.json_path_contains_nodes`, and `selene.json_path_value_nodes` turn
  JSON metadata predicates into graph node candidates and can return selected
  JSON path values without a second graph lookup;
- candidate-scoped companions
  (`selene.json_contains_candidate_nodes`,
  `selene.json_path_exists_candidate_nodes`,
  `selene.json_path_contains_candidate_nodes`, and
  `selene.json_path_value_candidate_nodes`) apply the same JSON predicates to a
  `LIST<NODE>` produced by graph, vector, or text retrieval.

Representative GQL calls:

```gql
CALL selene.json_contains_nodes(
  'Document', 'payload', json('{"memory":{"kind":"episodic"}}'), 10
)
YIELD node_id
RETURN node_id
```

```gql
CALL selene.json_path_exists_nodes(
  'Document', 'payload', json_array('memory', 'score'), 10
)
YIELD node_id
RETURN node_id
```

```gql
CALL selene.json_path_contains_nodes(
  'Document', 'payload', json_array('memory'), json('{"kind":"episodic"}'), 10
)
YIELD node_id
RETURN node_id
```

```gql
CALL selene.json_path_value_nodes(
  'Document', 'payload', json_array('memory', 'score'), 10
)
YIELD node_id, value
RETURN node_id, json_stringify(value)
```

```gql
MATCH (topic:Topic)-[:SUPPORTS]->(candidate:Document)
WITH collect_list(candidate) AS candidates
CALL selene.json_path_contains_candidate_nodes(
  'Document',
  'payload',
  json_array('memory'),
  json('{"kind":"episodic"}'),
  candidates,
  10
)
YIELD node_id
RETURN node_id
```

### Graph Algorithms

`selene-algorithms` owns native algorithms over immutable projections. GQL
exposes them through `CALL algo.*`, including PageRank, shortest paths,
weakly connected components, label propagation, Louvain, triangle count, and
related structural primitives.

```gql
CALL algo.pagerank('projection_name', 0.85, 100, 1e-6, NULL)
YIELD node_id, score
RETURN node_id, score
ORDER BY score DESC
LIMIT 20
```

```gql
CALL algo.pagerank(
  'projection_name', 0.85, 100, 1e-6, NULL, 'NATURAL',
  NULL, 'Document', NULL, NULL,
  'CITES', 'kind', ['primary'], 'target'
)
YIELD node_id, score
RETURN node_id, score
```

## Persistence

Use `Database::create` on an existing empty directory and `Database::open` to
reopen it. These fallible APIs use format 2 only on native Linux/macOS. Open is
non-destructive and eagerly reconstructs retained supported indexes; the
infallible builder remains memory-only. Checkpoint serializes writes. Prune is
explicit and respects active artifact leases; verification is read-only, not a
writer-readiness or physical-durability guarantee.

Drop all owning sessions/catalog handles before reopening as another writer.
Stable graph/element IDs survive ordinary reopen, but process-local references
must be issued again. An indeterminate result is **not** permission to retry:
inspect its phase, live visibility and durable outcome, then reconcile. See
[durable commit](docs/v2/durable-commit.md) and
[recovery verification](docs/v2/recovery-verification.md).

Format 1 is rejected, not decoded or migrated. Rebuild a fresh database from
application-owned source data; the project provides no 1.x migration support.

The library crates are allocator-agnostic. Benchmark binaries use mimalloc by
default so allocator A/B rows can be measured without forcing an embedder-wide
allocator policy.

## ISO GQL Posture

The current engine is GQL-oriented and implements selected ISO/IEC 39075:2024
syntax and semantics plus namespaced extensions. `spec/gql-profile/profile.json`
and the generated `selene-profile` API are the authority for parser admission,
runtime inventory, claim state, and evidence. Those dimensions remain separate,
and the inventory is not a formal 2.0 conformance claim. See the
[2.0 conformance policy](docs/v2/conformance-policy.md).

Important boundaries:

- vectors are implementation-defined values surfaced through the `VECTOR` type
  name, casts, properties, parameters, and native procedures;
- open and closed graph modes are supported;
- default isolation is serializable for one graph instance;
- ISO GQL does not define a wire format, so this repository does not ship one;
- auth, tenancy, network policy, and deployment topology are embedder concerns.

## Development

The workspace uses a pinned stable Rust toolchain. Read
[`rust-toolchain.toml`](rust-toolchain.toml) for the active version.

Common validation for code changes:

```bash
cargo fmt --all --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked --all-features --profile default
cargo test --workspace --locked --all-features --doc
cargo doc --workspace --no-deps --locked
cargo deny check bans licenses sources
cargo audit -d /private/tmp/selene-advisory-db
bash .github/scripts/check-file-size.sh
bash .github/scripts/check-no-secrets.sh
bash .github/scripts/check-thirdparty-current.sh
bash .github/scripts/check-no-rowid-arith.sh
bash .github/scripts/check-no-version-locked-feature-error.sh
bash .github/scripts/check-bench-invocation.sh
bash .github/scripts/check-benchmarks-doc.sh .
git diff --check
```

Docs-only changes can use the relevant subset, but formatting, file-size,
secret scan, benchmark-doc checks, and whitespace checks should stay green.

Install local hooks once per clone:

```bash
scripts/install-hooks.sh
```

## Benchmarks

Use the serialized runner. Do not run `cargo bench --workspace`; Cargo can run
bench binaries concurrently and pollute wall-clock medians.

```bash
scripts/run-benches.sh --list
scripts/run-benches.sh --smoke
scripts/run-benches.sh --profile quick --bench vector_graph_retrieval
scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter vector
scripts/run-benches.sh --profile quick --bench text_search_bm25
scripts/run-benches.sh --bench vector_index_rebuild --allocator system
```

Local embedding rows are opt-in and require an ignored `.env` file plus either
a local OpenAI-compatible oMLX endpoint or an OpenRouter embedding key:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_CORPUS=scaled_ambiguous_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter query_root
```

See [BENCHMARKS.md](BENCHMARKS.md) for registered bench targets, commands, and
current local evidence.

## Project Docs

- [2.0 Program](docs/v2/README.md)
- [Getting Started](docs/getting-started.md)
- [Embedding Guide](docs/embedding-guide.md)
- [GQL Reference](docs/gql-reference.md)
- [Architecture](docs/architecture.md)
- [Graph Algorithms](docs/graph-algorithms.md)
- [Persistence And Recovery](docs/persistence-and-recovery.md)
- [Observability](docs/observability.md)
- [Performance](docs/performance.md)
- [Contributing](docs/contributing.md)
- [Benchmark Registry](BENCHMARKS.md)

## Platform Support

| Platform | Status |
|---|---|
| Linux x86_64 / aarch64 | Primary deployment target. |
| macOS Apple Silicon / Intel | Primary development target. |
| Windows | Out of scope for now. |

## License

Dual-licensed under MIT OR Apache-2.0. See [LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE), [NOTICE](NOTICE), and
[THIRDPARTY.md](THIRDPARTY.md).
