# Embedding Guide

This guide is for engineers integrating `selene-db` into a Rust application. It
covers the facade first, then segregates lower engine APIs for advanced work.

selene-db is a single native graph engine — there is no extension/procedure-pack model. Graph algorithms are inlined in the mandatory `selene-algorithms` crate and exposed both as a native Rust API and via `CALL algo.*`; see [`docs/graph-algorithms.md`](graph-algorithms.md).

## 1. What "embedding" means here

`selene-db` is a **library-only** engine. ISO/IEC 39075:2024 Clause 4.2.3
does not normatively define a wire format. The engine ships:

- no server process,
- no transport (HTTP, gRPC, BACnet, anything),
- no bundled authentication or authorization service,
- no principals table, role catalog, credential store, or session store,
- no metrics endpoint, no admin UI.

The stable entry point is the `selene-db` facade. It opens the in-process engine
and runs GQL without exposing graph storage handles. Lower workspace crates are
advanced/internal APIs unless the facade intentionally re-exports a type.

Everything that touches the outside world is the **embedder's** responsibility:

| Concern | Where it lives |
|:---|:---|
| Transport (HTTP, gRPC, IPC, BACnet, MQTT, &c.) | Embedder |
| TLS termination | Embedder (with `rustls`; transitive crypto choice is enforced by `cargo-deny`) |
| Authentication of callers | Embedder |
| Session principal mapping and authorization | Embedder through facade `PrincipalProvider` and `AuthorizationPolicy` hooks |
| Multi-tenancy / per-tenant isolation | Embedder |
| Backups, replication, off-host durability | Embedder (selene provides WAL + snapshot primitives; off-host placement is yours) |
| Metrics, tracing exports | Embedder (selene emits `tracing` spans; export to your sink) |

The engine's job ends at the public crate APIs. Everything outside the in-process boundary is yours.

## 2. Workspace dependencies

Applications should depend only on package `selene-db`. The examples use the
current source coordinate, `2.0.0-alpha.1`, which may not yet be published.

The crate set is layered so transitive footprint stays small:

| Tier | What you can do | Crates to add |
|:---|:---|:---|
| Stable facade | Manage schemas and named graphs, then execute GQL through selected stable-ID sessions | `selene-db` |
| Core graph | Open a `SharedGraph`, mutate via `Mutator`, read snapshots | `selene-core`, `selene-graph` |
| Core graph + GQL | Run ISO GQL statements (no `CALL`, no persistence) | + `selene-gql` |
| Core graph + persistence | Direct mutation with WAL + snapshot recovery | + `selene-persist` |
| Graph algorithms (native API) | `selene-algorithms` free functions + the `GraphAlgorithms` trait, off the GQL path | + `selene-algorithms` |
| GQL `CALL` (platform built-ins + `algo.*`) | Wire `CALL selene.*` / `CALL algo.*` via the frozen native `BuiltinProcedureRegistry` | + `selene-gql`, `selene-algorithms` |

### Stable facade

```toml
[dependencies]
selene-db = { version = "2.0.0-alpha.1" }
```

The facade owns one immutable catalog. The infallible builder is memory-only.
Create a schema and graph through the
catalog lifecycle service, then select the graph when constructing a session:

```rust
use selene_db::{CreatePolicy, Database, ObjectPath, SchemaPath};

fn main() -> Result<(), selene_db::Error> {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let schema = SchemaPath::regular("selene", "memory")?;
    catalog.create_schema(&schema, CreatePolicy::Strict)?;

    let path = ObjectPath::regular("selene", "memory", "episodes")?;
    catalog.create_graph(&path, None, CreatePolicy::Strict)?;
    let session = database.session(&path)?;
    session.execute("INSERT (:Episode { summary: 'catalog lifecycle' })")?;
    session.execute("MATCH (e:Episode) RETURN e")?;
    Ok(())
}
```

The session stores the selected catalog identity and re-resolves the runtime
graph for each request. Dropping and recreating the same path does not make an
old session refer to the replacement. `Session::execute(source)` remains the
empty-parameter compatibility entry point. `Session::execute_request(Request)`
adds deterministic typed parameters, one immutable request timestamp, an
observable `RequestContext`, and a `RequestOutcome` that covers returned
validation, compilation, catalog-dispatch, and runtime failures.

Session parameters are controlled upserts. A request snapshots them, then
shadows exact case-sensitive names with its `RequestParams`; it never mutates
the session dictionary. Graph-backed values are checked against the selected
graph before execution. The lower runtime resolves binding-table references
through one request-owned registry; the facade exposes neither physical tables
nor raw registry access. The session is `Send` but not `Sync`, so an embedder
must serialize access rather than issue concurrent requests. Explicit facade
transactions and session controls share the same outer authority. Successful regular results retain immutable row
values and analyzer-declared field descriptors.

### Fallible format-2 facade lifecycle

The containing directory must already exist and be empty for strict creation:

```rust
use selene_db::{CreatePolicy, Database, ObjectPath, SchemaPath};

fn restart(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let database = Database::create(path)?;
    database.catalog().create_schema(&SchemaPath::regular("selene", "memory")?, CreatePolicy::Strict)?;
    let graph = ObjectPath::regular("selene", "memory", "episodes")?;
    database.catalog().create_graph(&graph, None, CreatePolicy::Strict)?;
    let session = database.session(&graph)?;
    session.execute("START TRANSACTION")?;
    session.execute("INSERT (:Episode {n: 1})")?;
    session.execute("COMMIT")?;
    database.checkpoint()?;
    session.execute("INSERT (:Episode {n: 2})")?;
    drop(session); // Sessions retain database ownership, including LOCK.
    drop(database);
    let reopened = Database::open(path)?;
    assert_eq!(reopened.session(&graph)?.execute("MATCH (n:Episode) RETURN n")?.row_count(), Some(2));
    Ok(())
}
```

Use `DatabaseDirectory::from_file` and `create_in`/`open_in` for caller-owned open
directory authority. `Database::open_mode()` reports the actual instance mode;
builder configuration does not accept ignored persistence settings. Checkpoint
holds the serial write reservation through I/O. Open is non-destructive and
eagerly rebuilds all retained supported indexes before returning; no optional
degraded or background-ready mode exists. Old owning sessions must be dropped
before another writer can open the directory. Old process-local references never
rebind to the fresh DatabaseId.

`GraphTypeDefinition`, `NodeTypeDefinition`, `EdgeTypeDefinition` and
`PropertyDefinition` construct richer **Rust** named schemas with the existing
`Type`/`Value`, defaults, nullability, UNIQUE and immutability. Install the type
through `Catalog::create_graph_type` and bind a graph to its path. Endpoints name
node types; there is no ignored directed-only flag. Use existing mutation calls
for supported indexes. This does not expand the property-free GQL catalog
graph-type subset. See the tested rustdoc and
[checkpoint/reopen contract](v2/checkpoint-reopen.md) for exact bounds, outcomes,
compatibility and remaining PR06/07/08 responsibilities. This is not a GA or
durable-preview/conformance announcement. Lower legacy persistence recipes below
are advanced historical APIs and are not the facade's format-2 open path.

## Advanced lower-engine APIs

The remaining crate-assembly sections are for engine development. They are not
the normal application entry point and do not carry the facade's 2.x stability
promise.

### 2.1 Plain core graph

```toml
[dependencies]
selene-core = { package = "selene-db-core", version = "2.0.0-alpha.1" }
selene-graph = { package = "selene-db-graph", version = "2.0.0-alpha.1" }
```

Use this when you only need the in-memory property graph: nodes, edges, label/property indexes, the `Mutator` write funnel. No parser, no executor, no disk.

### 2.2 With GQL

```toml
[dependencies]
selene-core = { package = "selene-db-core", version = "2.0.0-alpha.1" }
selene-graph = { package = "selene-db-graph", version = "2.0.0-alpha.1" }
selene-gql = { package = "selene-db-gql", version = "2.0.0-alpha.1" }
```

Adds the Pest grammar, AST, semantic analyzer, planner, optimizer, and row-at-a-time executor. You can now `parse → analyze → plan → execute_statement`. `CALL` is still off (`EmptyProcedureRegistry` always returns `None`).

### 2.3 With persistence

```toml
[dependencies]
selene-core = { package = "selene-db-core", version = "2.0.0-alpha.1" }
selene-graph = { package = "selene-db-graph", version = "2.0.0-alpha.1" }
selene-gql = { package = "selene-db-gql", version = "2.0.0-alpha.1" }
selene-persist = { package = "selene-db-persist", version = "2.0.0-alpha.1" }
```

Adds the WAL writer (`SLDB` magic), the snapshot writer (`SLSN` magic), and the two-step recovery driver. `selene-persist` is graph-blind: it takes `&[Change]` slices and routes them by provider tag.

### 2.4 With `CALL` (platform built-ins + graph algorithms)

```toml
[dependencies]
selene-core = { package = "selene-db-core", version = "2.0.0-alpha.1" }
selene-graph = { package = "selene-db-graph", version = "2.0.0-alpha.1" }
selene-gql = { package = "selene-db-gql", version = "2.0.0-alpha.1" }
selene-persist = { package = "selene-db-persist", version = "2.0.0-alpha.1" }
selene-algorithms = { package = "selene-db-algorithms", version = "2.0.0-alpha.1" }
```

For local engine development, keep the `package = "selene-db-*"` aliases and
add `path = "path/to/selene-db/crates/<crate>"` alongside each version. The
[version policy](v2/eol-and-version-policy.md) defines the alpha and 1.x support
posture.

selene-db is a single native engine — there is no extension/procedure-pack model and nothing to load at runtime. `CALL` is served by the one frozen native `BuiltinProcedureRegistry` (`selene-gql/src/runtime/builtin_registry.rs`), constructed with `BuiltinProcedureRegistry::new()`. It registers exactly 69 procedures, fixed at construction:

- 50 platform built-ins covering health, feature reporting, verification, compaction stats, scalar/vector/text index management, graph reachability, vector search and scoring, BM25 scoring, JSON candidate production, Reciprocal Rank Fusion, and maintenance;
- 19 `algo.*` procedures (projection lifecycle, PageRank, betweenness, label propagation, Louvain, triangle count, WCC, SCC, topological sort, articulation points, bridges, Dijkstra, SSSP, APSP), binding `CALL algo.*` directly over the `selene-algorithms` native API.

`BuiltinProcedureRegistry` implements `selene_gql::ProcedureRegistry`; pass `&registry` everywhere the pipeline asks for `&dyn ProcedureRegistry` (see §6). The `CALL` grammar is plain ISO `CALL` (IW010), unchanged. `SHOW PROCEDURES` enumerates all 69:

```rust
use selene_gql::{BuiltinProcedureRegistry, Session};
use selene_graph::SharedGraph;
use selene_core::GraphId;

let graph = SharedGraph::new(GraphId::new(1));
let registry = BuiltinProcedureRegistry::new();
let mut session = Session::new(&graph);

// Run a graph algorithm over an ephemeral projection.
session.execute_source("CALL algo.projection_build('p', NULL, NULL, NULL)", &registry)?;
session.execute_source("CALL algo.pagerank('p', NULL, NULL, NULL, NULL) YIELD node_id, score", &registry)?;
// Platform health built-in.
session.execute_source("CALL selene.health() YIELD node_count, edge_count", &registry)?;
```

The native algorithms are also callable directly off the GQL path — `selene-algorithms` free functions plus the `GraphAlgorithms` trait — when you don't need the `CALL` surface. See [`docs/graph-algorithms.md`](graph-algorithms.md).

## 3. Opening a graph

`SharedGraph` is the per-graph runtime handle. It owns:

- the immutable graph snapshot (read by lock-free `ArcSwap::load_full`),
- the single write lock (acquired by `begin_write`),
- the `IdAllocator` for node/edge IDs,
- the fixed `Vec<Arc<dyn IndexProvider>>` registered at construction time.

Construct one per graph:

```rust
use selene_core::GraphId;
use selene_graph::SharedGraph;

let graph = SharedGraph::new(GraphId::new(1));
```

If you want a closed graph (Spec 02 GG02) bound to a `GraphTypeDef`, or if you want to register index providers, use the builder:

```rust
use std::sync::Arc;
use selene_core::GraphId;
use selene_graph::SharedGraph;

let graph = SharedGraph::builder(GraphId::new(1))
    // .bound_to(my_graph_type)?   // closed graph
    // .with_provider(Arc::new(my_index_provider))
    .build()?;
```

`SharedGraph` is `Send + Sync`. To share it across threads, wrap it in `Arc`:

```rust
use std::sync::Arc;
use selene_core::GraphId;
use selene_graph::SharedGraph;

let graph = Arc::new(SharedGraph::new(GraphId::new(1)));

// Cheap clones; each clone shares the same snapshot and write lock.
let reader_graph = Arc::clone(&graph);
std::thread::spawn(move || {
    let snapshot = reader_graph.read();
    println!("node_count = {}", snapshot.node_count());
});
```

`SharedGraph::read()` returns `Arc<SeleneGraph>` — an immutable snapshot. The lock-free read path never blocks on writers.

`SharedGraph` does **not** implement `Drop`-driven persistence. The graph lives entirely in memory; durability is added by wiring `selene-persist` (see §7).

## 4. The transaction model

selene-db runs **one writer at a time** per graph and **unbounded concurrent readers**. Isolation is **strict-serializable** under a single write lock with lock-free reads.

### 4.1 Begin a write

```rust
let mut tx = graph.begin_write();
```

`begin_write` acquires the `RwLock` write side and the allocator mutex. Other writers queue normally. Readers continue to observe the previously-published snapshot through `ArcSwap`.

### 4.2 Mutate

```rust
let mut mutator = tx.mutator();
let node_id = mutator.create_node(labels, props)?;
```

`tx.mutator()` returns a `Mutator<'_, 'g>` — a borrowed mutation builder tied to this transaction. It accumulates `Change` records on the transaction; nothing is published until `commit`.

You can take and drop multiple `Mutator` handles inside one transaction; they share the transaction's pending change list.

### 4.3 Commit

```rust
let outcome = tx.commit()?;
println!(
    "published generation {} with {} changes",
    outcome.generation,
    outcome.changes.len()
);
```

Commit, in order:

1. Bumps `meta.generation` and seals the next-ID watermarks.
2. (Closed graphs only) runs `type_validator::validate_change` per change and `validate_entity_state` on any schema change.
3. Publishes the new `Arc<SeleneGraph>` through `ArcSwap`.
4. Holds the write lock and allocator mutex across **provider fanout** (`on_change` is delivered to every registered `IndexProvider` in registration order).
5. Returns a `CommitOutcome` with `generation`, `changes`, `principal`, `next_node_id`, `next_edge_id`.

If you want to attach opaque audit bytes (a tenant ID, a request principal, a session UUID) to the commit, use `commit_with_principal(Some(bytes))`. selene-db treats the bytes as an opaque token; the WAL writer will fold them into the entry header.

### 4.4 Rollback

```rust
tx.rollback();
```

`rollback` drops the transaction without publishing. The pending change list is discarded; the `ArcSwap` snapshot still points at the pre-transaction graph, so readers never observed the uncommitted state.

A transaction also rolls back when `tx` is dropped without calling `commit`:
the snapshot is never published, and the write lock is released.

### 4.5 Concurrency invariants

| What | How it's enforced |
|:---|:---|
| One writer at a time | `parking_lot::RwLock` write side |
| Many concurrent readers | `ArcSwap` load_full + immutable snapshots |
| No read blocks on a writer | Snapshot is published atomically; readers never touch the write lock |
| Provider fanout is serialized | Write lock is held across `notify_providers` |
| Same-thread re-entrant write panics fast | Thread-local fanout counter checked by `begin_write` |

Cross-thread re-entry — a provider that spawns a worker which then calls `begin_write` and waits — is documented misuse. See the `IndexProvider` rustdoc.

## 5. Direct mutation API

`Mutator` is the only path that writes the graph (the **mutation funnel**, Spec 03 §4.3). Every method records a `Change` so providers, the WAL, and replicas all see the same event stream.

The full surface:

### 5.1 Create a node

```rust
use selene_core::{LabelSet, PropertyMap, Value, db_string};

let person = db_string("Person")?;
let name = db_string("name")?;

let mut tx = graph.begin_write();
let mut props = PropertyMap::new();
props.set(name, Value::String(db_string("Ada")?))?;
let node_id = tx.mutator()
    .create_node(LabelSet::single(person), props)?;
tx.commit()?;
```

Emits `Change::NodeCreated`. Returns the assigned `NodeId`.

### 5.2 Create an edge

```rust
use selene_core::{PropertyMap, db_string};

let knows = db_string("KNOWS")?;

let mut tx = graph.begin_write();
let edge_id = tx.mutator().create_edge(
    knows,
    source_id,
    target_id,
    PropertyMap::new(),
)?;
tx.commit()?;
```

Emits `Change::EdgeCreated`. Verifies both endpoints are alive; returns `GraphError::NodeNotAlive` otherwise.

### 5.3 Update a node

```rust
use selene_core::{DbString, LabelDiff, PropertyDiff, Value, db_string};

let label_diff = LabelDiff::new(
    std::iter::empty::<DbString>(),
    std::iter::empty::<DbString>(),
)?;
let props_diff = PropertyDiff::new(
    [(db_string("status")?, Value::String(db_string("active")?))],
    [db_string("draft_until")?],
)?;

let mut tx = graph.begin_write();
tx.mutator()
    .update_node(node_id, label_diff, props_diff)?;
tx.commit()?;
```

Emits `Change::NodeUpdated`. Label adds/removes and property set/remove are applied as a diff; the engine reconciles label index bitmaps and the typed property indexes.

### 5.4 Update an edge

```rust
use selene_core::{DbString, PropertyDiff, Value, db_string};

let props_diff = PropertyDiff::new(
    [(db_string("weight")?, Value::Float(0.42))],
    std::iter::empty::<DbString>(),
)?;

let mut tx = graph.begin_write();
tx.mutator().update_edge(edge_id, props_diff)?;
tx.commit()?;
```

Edge labels are immutable; only property diffs are accepted. Emits `Change::EdgeUpdated`.

### 5.5 Delete a node

```rust
let mut tx = graph.begin_write();
tx.mutator().delete_node(node_id)?;
tx.commit()?;
```

Cascades to delete every incident edge first, then the node. Emits `Change::EdgeDeleted` (one per incident edge) and `Change::NodeDeleted`.

### 5.6 Delete an edge

```rust
let mut tx = graph.begin_write();
tx.mutator().delete_edge(edge_id)?;
tx.commit()?;
```

Emits `Change::EdgeDeleted`.

### 5.7 Schema change

```rust
use selene_core::SchemaChange;

let mut tx = graph.begin_write();
tx.mutator().schema_change(my_schema_change);
tx.commit()?;
```

Appends `Change::SchemaChanged` to the WAL stream. Catalog graph mutation and closed-graph validation are run at commit time when a `bound_type` is present.

The record is stamped with this graph's own id, which the mutator reads from the live transaction. Recovery refuses a directory whose WAL carries a foreign graph id, so this is not a value an embedder supplies.

## 6. Running GQL

The full pipeline is `parse → analyze → plan → execute_statement`. Optimization runs as a refinement pass on the lowered plan and is folded into typical execution flows by callers.

### 6.1 The four stages

| Stage | Function | Input | Output | Error |
|:---|:---|:---|:---|:---|
| Parse | `selene_gql::parse` | `&str` | `Statement` | `ParserError` |
| Analyze | `selene_gql::analyze` | `Statement`, `&dyn ProcedureRegistry`, `Option<&GraphTypeDef>` | `AnalyzedStatement` | `AnalysisError` |
| Plan | `selene_gql::plan` | `&AnalyzedStatement`, `&dyn ProcedureRegistry` | `ExecutionPlan` | `PlannerError` |
| Execute | `selene_gql::execute_statement` | `&ExecutionPlan`, `&mut Session`, `&dyn ProcedureRegistry` | `StatementOutput` | `ExecutorError` |

### 6.2 The Session

`Session<'g>` is the executor-facing handle bound to a single `SharedGraph`. Construct it once per logical "session" (which is whatever you want — per request, per tenant connection, per task):

```rust
use selene_gql::Session;

let mut session = Session::new(&graph);
```

If you want commit principal bytes recorded on every auto-commit transaction:

```rust
use std::sync::Arc;
use selene_gql::Session;

let principal: Arc<[u8]> = b"tenant=acme;user=ada".to_vec().into();
let mut session = Session::with_principal(&graph, principal);
```

A `Session` also tracks explicit transaction state for `START TRANSACTION` / `COMMIT` / `ROLLBACK` statements. Read-only statements bypass the write lock; mutation statements either join the active explicit transaction or auto-commit.

### 6.3 The full flow

```rust
use selene_gql::{
    EmptyProcedureRegistry, Session, StatementOutput,
    analyze, execute_statement, parse, plan,
};

let registry = EmptyProcedureRegistry;
let statement = parse("MATCH (p:Person) RETURN p.name")?;
let analyzed = analyze(statement, &registry, None)?;
let planned = plan(&analyzed, &registry)?;
let mut session = Session::new(&graph);
let output = execute_statement(&planned, &mut session, &registry)?;

match output {
    StatementOutput::Rows(table) => {
        for row in table.rows() {
            println!("name = {:?}", row.get(0));
        }
    }
    StatementOutput::Empty => {
        // Mutation, DDL, or transaction-control statement.
    }
}
```

The `analyze` schema argument is `Option<&GraphTypeDef>`. Pass `None` for open graphs (GG01); pass `Some(&type_def)` for closed graphs so the analyzer can run static label/property/edge validation at plan time. Closed-graph validation runs again at commit time inside `WriteTxn::commit`.

### 6.4 Schema validation

`CREATE NODE TYPE` and `CREATE EDGE TYPE` accept `STRICT` and `WARN`
validation modes. `STRICT` is the default: if a graph is bound to a
`GraphTypeDef`, writes are checked against that type during analysis and again
at commit, and violations return `G2000`. `WARN` permits relaxed writes and
emits `01N01` (`VALIDATION_MODE_RELAXED_WRITE`) through the session warning
sink after commit.

`DEFAULT` and `NOT NULL` are independent in the AST. A property with
`DEFAULT <expr>` but no `NOT NULL` is nullable. Catalog defaults are validated,
stored in the graph type, shown by `SHOW NODE TYPES` / `SHOW EDGE TYPES`, and
materialized when an inserted node or edge omits the property.

### 6.5 Warning channel

Runtime warnings are opt-in. A `Session` without a warning sink silently
discards warnings; embedders that need visibility attach a sink with
`Session::with_warning_sink`. The live warning today is aggregate
NULL-elimination (`01G11`).

```rust
use selene_gql::{ExecutorWarning, Session, WarningSink};

#[derive(Default)]
struct WarningLog(Vec<ExecutorWarning>);

impl WarningSink for WarningLog {
    fn emit(&mut self, warning: ExecutorWarning) {
        self.0.push(warning);
    }
}

let mut session = Session::new(&graph).with_warning_sink(WarningLog::default());
```

### 6.6 `StatementOutput` variants

`StatementOutput` is `#[non_exhaustive]`. Match all variants you handle:

- `StatementOutput::Rows(BindingTable)` — the statement produced a row-bearing result. Iterate `table.rows()` for `&[Binding]`; each `Binding` is an ordered `&[Value]`. Use `table.schema().columns` to recover column names and types.
- `StatementOutput::Empty` — the statement completed without rows (mutations with no `RETURN`, DDL, transaction control).

Mutation statistics (rows inserted, edges updated, &c.) are not currently
exposed as a separate variant. If you need counts, plan an explicit
`RETURN count(*)` or instrument via the WAL `Change` stream.

### 6.7 Extracting values

`Value` is a non-exhaustive enum with scalar, composite, graph-reference,
temporal, byte-string, vector, and JSON variants. Strings are stored as
`Value::String(DbString)`:

```rust
use selene_core::Value;

let Some(Value::String(value)) = row.values().first() else {
    return Err(...);
};
let s: &str = value.as_str();
```

Construct graph labels, property keys, aliases, and string values through
`selene_core::db_string(...)` so the IL013 string-size guard is applied.

### 6.8 Reusing a plan

`ExecutionPlan` is `Clone` and stable across runs against the same graph topology. Embedders that issue the same statement many times should plan once and execute many times.

## 7. Wiring persistence

`selene-persist` ships:

- `WalWriter` — append-only WAL file (`SLDB` magic).
- `SnapshotBuilder` — atomic snapshot envelope (`SLSN` magic, hard-link rename).
- `recover` — two-step recovery: apply the latest snapshot, then replay the WAL tail.

`selene-persist` is graph-blind. It knows about `Change` payloads and `ProviderTag` keys; it does not know about node row layouts or label indexes.

### 7.1 The canonical path: `with_wal`

Build the graph with a WAL and the engine owns durability end to end:

```rust
use std::path::Path;
use selene_core::GraphId;
use selene_graph::{CommitBatching, DEFAULT_WAL_FILE_NAME, SharedGraph, WalConfig};

let dir = Path::new("/var/lib/myapp/graph-1");
std::fs::create_dir_all(dir)?;

let graph = SharedGraph::builder(GraphId::new(1))
    .with_wal(dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default())?
    .with_commit_batching(CommitBatching::Off)
    .build()?;
```

The full create → commit → close → reopen round trip is a **compiled and
executed doctest** on `selene_graph::SharedGraph`, so it cannot drift away from
the API the way this excerpt could. Read it with `cargo doc --open -p
selene-db-graph`, or run it with `cargo test -p selene-db-graph --doc`.

`with_wal` registers the WAL as a **commit-critical durable provider**, not an
`IndexProvider`. That distinction is the whole point of this section: the two
hooks run at different times and have different failure semantics.

Reopening an existing directory is `SharedGraph::recover(dir, graph_id)`, never
`with_wal` — attaching to a directory that already holds a committed store is
refused with `GraphError::ExistingStore`, because attaching does not replay.

### 7.2 What the commit actually guarantees

Every commit runs **append → flush → publish → acknowledge**, in that order, on
a single per-graph committer thread:

1. **Append** — the entry is written to the WAL.
2. **Flush** — one `fsync` per run, the durability barrier.
3. **Publish** — the new snapshot enters the `ArcSwap`, becoming visible to
   readers.
4. **Acknowledge** — `commit()` returns `Ok(CommitOutcome)` carrying
   `durable_at`.

The barrier sits before publication, so **no reader ever observes a commit that
is not already durable**, and no caller is told "committed" before the fsync.

Two consequences worth internalising:

- **`with_wal` overrides `WalConfig.sync_policy` to `OnFlushOnly`.** Whatever
  you pass is discarded. The committer is the sole fsync caller; a
  per-append policy would double-sync and break the group barrier.
- **`CommitBatching`, not `SyncPolicy`, controls fsync grouping.**
  `CommitBatching::Off` is one fsync per commit. `CommitBatching::On { .. }`
  coalesces a contiguous run into one fsync, trading latency for throughput.

#### Do not use an `IndexProvider` to write the WAL

Earlier revisions of this guide showed a `WalSinkProvider` — an `IndexProvider`
relaying each `Change` into a `WalWriter` — and called it the canonical
durability pattern. **It is not, and it never provided the atomicity that text
claimed.** If you have that pattern in your codebase, replace it with
`with_wal`.

`IndexProvider::on_change` fan-out runs **after** publication, and callback
errors and panics are logged and swallowed. So the pattern:

- writes one WAL entry per `Change` rather than one per commit, inflating the
  log and losing the transaction boundary;
- reports a successful commit even when the WAL write failed, because the
  fan-out cannot fail a commit that has already been published;
- provides no barrier, so a reader can observe a commit whose WAL entry does
  not exist.

`IndexProvider` is for derived in-memory state that can be rebuilt from the
graph. Durability is `DurableProvider`, and `with_wal` registers it for you.

#### Low-level `WalWriter` is for offline tooling

`WalWriter::open` remains public for offline replication, inspection, and
repair tools that own their own sequencing on a quiesced directory. It holds an
exclusive OS-level file lock, so a second open on the same path returns
`PersistError::WriterLockHeld` — including against a live `SharedGraph`. Do not
point one at a directory a `SharedGraph` has open.

For that path only, `SyncPolicy` means what it says: `EveryN(N)` fsyncs after
every N appends, `OnFlushOnly` defers to an explicit `flush()`.

### 7.3 Durable failures require reopen, not retry

A commit that fails **after** it reaches the durable path returns
`GraphError::IndeterminateOutcome` (GQLSTATUS `40003`, *transaction rollback —
statement completion unknown*) and poisons the committer.

`Err` here does **not** mean the transition did not happen. The WAL record may
already be written, or written and fsynced, and a reopen replays it. Retrying
blind double-applies.

```rust
match txn.commit() {
    Ok(outcome) => { /* durable at outcome.durable_at */ }
    Err(error) if error.requires_reopen() => {
        // Quiesce, drop the handle, SharedGraph::recover, read back to see
        // whether it landed, and only then decide whether to retry.
    }
    Err(error) => { /* definite rejection; the handle is still usable */ }
}
```

`GraphError::requires_reopen()` is the supported test — prefer it to matching a
variant. It covers checkpoint failures too. Once poisoned, every later commit,
compaction, and checkpoint on that handle fails the same way.

See `docs/persistence-and-recovery.md` for the full commit/checkpoint outcome
contract.

### 7.4 Writing snapshots

Snapshots are atomic envelopes containing zero or more **sections** keyed by `(provider, sub)` tag pair. The engine-owned graph state lives under the `CORE` provider (`META`, `NODE`, `EDGE`, `SCMA` sub-tags); extension providers own their own sections under their own provider tags.

For a live graph opened with `.with_wal(...)`, call
`SharedGraph::checkpoint(CheckpointConfig::default())`. The coordinated facade
prepares every engine-owned provider at one graph generation, reserves a fresh
physical WAL sequence with a checkpoint watermark, and performs the crash-safe
MANIFEST rotation. Graph generation and snapshot sequence are intentionally
distinct; never substitute `CommitOutcome::generation` for the checkpoint
sequence.

Direct `SnapshotBuilder` construction is for offline tooling or a host that has
already quiesced writes and owns its sequence policy:

```rust
use selene_persist::{SectionCompression, SnapshotBuilder, SnapshotConfig};

let config = SnapshotConfig {
    dir: wal_dir.to_path_buf(),
    sequence: offline_snapshot_sequence,
    compression: SectionCompression::DEFAULT,  // zstd level 1, per-section
    fsync: true,
};
let mut builder = SnapshotBuilder::new(config);

// Write the CORE provider's sections (META/NODE/EDGE/SCMA).
// Each extension provider also contributes via write_section(sub_tag).
builder.add_section(*b"CORE", *b"META", core_meta_bytes)?;
// ...
let snapshot_path = builder.finalize()?;
```

`finalize` writes `snapshot.{seq}.snap.tmp`, fsyncs it, then **hard-links** to `snapshot.{seq}.snap`. Hard-link gives `AlreadyExists` on collision (race-safe; no `unsafe` `renameat2`).

In practice, the `CoreProvider` (from `selene-graph`) and any extension providers all implement `IndexProvider::write_section`; you iterate `declared_sub_tags()` on each and feed the bytes into `SnapshotBuilder::add_section`.

### 7.5 Two-step recovery

```rust
use std::sync::Arc;
use selene_persist::{ProviderRegistry, recover};

let mut registry = ProviderRegistry::new();
registry.register(Arc::clone(&core_recovery_provider))?;
registry.register(Arc::clone(&extension_recovery_provider))?;
// ...

let outcome = recover(wal_dir, &registry)?;
println!(
    "applied snapshot seq={} last WAL seq={} providers={:?}",
    outcome.applied_snapshot_seq,
    outcome.last_wal_seq,
    outcome.providers_invoked,
);
```

`recover` runs in two stages after selecting the MANIFEST-authoritative
snapshot (or the highest snapshot in a legacy MANIFEST-less directory):

1. **Snapshot apply.** Verify the selected snapshot's body hash and call `read_section` on every section in section-table order, routed by `provider` tag.
2. **WAL replay.** Read every WAL frame with `sequence > applied_snapshot_seq`. Physical checkpoint watermarks advance `last_wal_seq` but are skipped. Logical commit frames, including unflagged empty commits, advance `wal_commit_entries_applied`; their changes fan out to registered providers in deterministic tag order. On the legacy path, the WAL must extend the snapshot epoch or recovery returns `PersistError::WalSnapshotMismatch`.

The convenience `recover` function holds a shared `PersistenceReadGuard`
across both stages, so checkpoint rotation and retention pruning cannot switch
or delete the selected epoch during replay. If an embedder already holds a
guard for a larger read transaction, use `recover_guarded(&guard, &registry)`.
Provider callbacks must not invoke same-directory checkpoint, prune, rotation,
or MANIFEST publication while the shared guard is held.

Both `IndexProvider` and `RecoveryProvider` exist because `selene-graph` and `selene-persist` are separately layered. Most providers implement both traits with thin shims so that the same derived state is written at snapshot time and re-read at recovery time.

#### A successful recovery can still have dropped a commit

`SharedGraph::recover` returning `Ok` does not mean nothing was lost. If the
process died mid-append, the WAL's final frame can be short, corrupt, or
zero-filled. That frame was never acknowledged to any caller, so discarding
exactly it is correct crash recovery — but a commit a client believed it had
submitted may have been inside it.

Recovery reports what it discarded:

```rust
let graph = SharedGraph::recover(dir, graph_id)?;
if let Some(repair) = graph.recovery_tail_repair() {
    // reason: ShortFrame | CorruptFinalFrame | ZeroFilledTail
    tracing::warn!(
        ?repair.reason,
        offset = repair.offset,
        discarded_bytes = repair.discarded_bytes,
        "recovery discarded an unacknowledged WAL tail",
    );
}
```

`None` means the WAL was intact. It is also what a graph built through
`SharedGraph::builder` returns, since building refuses a directory that already
holds a store and so has no tail to repair — if you need to tell those apart,
track which constructor you used. The value describes this reopen and does not
change as the graph runs.

This is a report, not an error: recovery succeeded either way, and there is
nothing to retry. Its use is reconciliation — deciding whether to re-drive
in-flight work whose acknowledgement you never received.

## 8. Principals and authorization

Per ISO/IEC 39075:2024 Clause 4, the spec calls out `IW011` (external
procedures), `ID001` (principal representation), `IW002` (authorization
identifier lifecycle), and `ID003` (authorization privileges) as
implementation-defined. The generated profile selects facade hooks owned by
the embedder. The engine has no principal table, role catalog, credential
store, network authentication, or `GRANT` syntax.

### 8.1 Where the authz boundary goes

The embedder authenticates callers outside Selene and constructs a string
`AuthorizationId`. `Database::session_with_options` resolves that ID through an
object-safe `PrincipalProvider`, resolves optional declared home paths to stable
catalog descriptors, and asks an `AuthorizationPolicy` to allow or deny session
creation. Both hooks are synchronous, `Send + Sync`, and receive facade types;
neither runs under catalog lifecycle locks or graph request leases.

`PrincipalId` and `AuthorizationId` use non-empty database-string semantics.
Optional principal audit bytes are separate opaque data forwarded to graph
commits. `Session::context()` exposes immutable inspection of both values and
the copied catalog/profile defaults. The built-in anonymous configuration does
not invoke a provider and uses the local allow-all policy.

This policy hook authorizes session creation from current/home descriptors. It
does not inspect each analyzed statement and is not a privilege language.
Embedders that need statement-specific policy can still use the lower advanced
analyzer boundary described below until a later facade contract owns that
surface.

### 8.2 Inspecting the write set

`AnalyzedStatement` carries a `MutationWriteSet` (when the statement is a mutation) enumerating every `WriteSetEntry` the statement will perform. Each entry has a `WriteKind`:

- `InsertNode { label_expr, property_keys, .. }`
- `InsertEdge { label_expr, property_keys, .. }`
- `SetProperty { target, element, key, .. }`
- `SetLabel { target, element, label }`
- `RemoveProperty { target, element, key, .. }`
- `RemoveLabel { target, element, label }`
- `DeleteElement { target, element, mode }`

A read-side write-set lookup in the embedder looks like this:

```rust
use selene_gql::{
    AnalyzedStatementKind, MutationWriteSet, WriteKind,
    analyze, parse,
};

fn write_keys(analyzed: &selene_gql::AnalyzedStatement) -> Vec<&selene_core::DbString> {
    match &analyzed.statement {
        AnalyzedStatementKind::Mutate(pipeline) => {
            let write_set: &MutationWriteSet = &pipeline.write_set;
            write_set
                .entries
                .iter()
                .filter_map(|entry| match &entry.kind {
                    WriteKind::SetProperty { key, .. } => Some(key),
                    WriteKind::RemoveProperty { key, .. } => Some(key),
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}
```

(The exact field shape of `MutationPipeline::write_set` should be verified against the analyzer module before relying on it in production — see §10.)

### 8.3 Reading the statement category

`AnalyzedStatement::category` is one of `ReadOnly`, `DataModifying`, `CatalogModifying`, `TransactionControl`. This is the cheapest authz gate: deny `CatalogModifying` for tenants without DDL rights, &c., without walking the write set.

### 8.4 Principal bytes on commits

Once the embedder has authorized the statement, it threads the principal through `Session::with_principal(graph, principal_bytes)`. The bytes are opaque to the engine; they flow through `commit_with_principal` into the `CommitOutcome::principal` field and into the WAL header. Replay later sees the same bytes on every audited change.

## 9. Multi-tenancy

selene-db has no built-in tenant model. Two patterns are common.

### 9.1 Per-tenant `SharedGraph`

Each tenant gets its own `Arc<SharedGraph>` and its own WAL directory. This is the cleanest model: strict-serializable isolation is per-graph, so two tenants cannot block each other on the write lock.

```rust
struct TenantRuntime {
    graph: Arc<SharedGraph>,
    wal_dir: PathBuf,
}

let tenants: HashMap<TenantId, TenantRuntime> = HashMap::new();
```

The native `BuiltinProcedureRegistry` is stateless across graphs (it carries only ephemeral per-`GraphId` projection catalogs), so a single shared `BuiltinProcedureRegistry::new()` serves every tenant — pass `&registry` into each tenant's pipeline. Call `registry.forget_graph(graph_id)` when a tenant graph is dropped to reclaim its projection catalog.

Trade-offs:

- Highest isolation; one tenant's bug cannot leak into another's graph.
- More memory: each `SharedGraph` carries its own snapshot and indexes.
- More descriptors: one WAL writer per graph.

This is the default recommendation for any embedder serving multiple distinct tenants.

### 9.2 Per-tenant subspace within one graph

Use a single `SharedGraph` and isolate tenants by label prefix or by a tenant-ID property. Authorization is enforced by the embedder's wrapper layer.

Trade-offs:

- Single write lock — tenant A's busy writer blocks tenant B.
- Tenant scans need extra filtering; you lose label-index purity unless every label includes the tenant ID.
- One bug in the authz wrapper exposes one tenant's data to another.

Use only when tenants are administratively trusted (e.g. departments inside one org) and graphs are tiny.

The procedure surface is the same for every tenant: the frozen native `BuiltinProcedureRegistry` registers a fixed set of 69 procedures (50 platform built-ins plus 19 `algo.*`) at construction. There is no per-tenant procedure surface to configure — selene-db is a single native engine with no loadable extensions. If a tenant must not be allowed to `CALL` a given procedure, gate it in the embedder's authorization wrapper (§8), not by handing out a different registry.

## 10. Error handling

selene-db errors are layered by crate. Each layer maps to a `GQLSTATUS` code for ISO conformance, but Rust callers handle them as typed enums.

| Crate | Error type | Pipeline stage |
|:---|:---|:---|
| `selene-core` | `CoreError` | Interner, codec, schema |
| `selene-graph` | `GraphError` | Storage, mutation, providers |
| `selene-gql` | `ParserError`, `AnalysisError`, `PlannerError`, `ExecutorError`, `ProcedureError` | Each respective pipeline stage; `ProcedureError` covers native `CALL` dispatch (unknown procedure, tier mismatch, bad argument) |
| `selene-persist` | `PersistError` | WAL, snapshot, recovery |

### 10.1 Recovering from parse / analyze / plan errors

```rust
match parse(source) {
    Ok(statement) => statement,
    Err(parser_error) => {
        // ParserError carries a SourceSpan; surface it as a diagnostic.
        return Err(ApiError::BadRequest(parser_error.to_string()));
    }
};
```

`AnalysisError` and `PlannerError` are structurally similar — both expose stable error variants with diagnostics. Static (non-IO) errors at these stages mean the statement is malformed in some way the embedder should report to the caller verbatim.

### 10.2 Recovering from runtime errors

```rust
match execute_statement(&plan, &mut session, &registry) {
    Ok(StatementOutput::Rows(table)) => Ok(table),
    Ok(StatementOutput::Empty) => Ok(BindingTable::empty(plan.output_schema.clone())),
    Err(ExecutorError::GraphMutation { source, .. }) => {
        // Wrap GraphError (e.g. NodeNotAlive, IdOverflow, IndexValueRejected).
        Err(ApiError::Conflict(source.to_string()))
    }
    Err(ExecutorError::InFailedTransaction { .. }) => {
        // Session is in a failed explicit transaction; the caller must ROLLBACK.
        session.abort();
        Err(ApiError::FailedTransaction)
    }
    Err(other) => Err(ApiError::Internal(other.to_string())),
}
```

A failed auto-commit transaction rolls back automatically — `execute_statement` calls `txn.rollback()` on the error path. A failed statement inside an **explicit** `START TRANSACTION` block sets `session.aborted = true`; subsequent non-`ROLLBACK` statements return `ExecutorError::InFailedTransaction`. The embedder must call `session.abort()` or run `ROLLBACK`.

### 10.3 Recovering from persistence errors

**Live `SharedGraph` writes.** Test `GraphError::requires_reopen()` first. When
it is true the committer is poisoned and the outcome is unknown: quiesce, drop
the handle, reopen with `SharedGraph::recover`, read back to establish whether
the work landed, and only then decide whether to retry. Retrying blind
double-applies. When it is false the failure was definite and the handle is
still usable, so an ordinary retry is safe. This applies to `checkpoint` as well
as `commit`.

**Offline `WalWriter` tooling.** WAL append errors truncate the file back to the
last fully-committed offset, so the embedder can retry the append after
addressing the I/O cause. Snapshot finalize errors leave the temp file; clean up
at startup.

**Recovery.** Errors classifying as `PersistError::ChecksumMismatch` or
`WalSnapshotMismatch` indicate corruption — surface to the operator.
`PersistError::WalMidLogCorruption` means the damage is not a torn tail and
recovery refused rather than silently truncating; the store needs operator
attention, not a retry.

## 11. Embedding patterns

Three patterns cover most embedders.

### 11.1 In-process library

The simplest case: your app calls selene-db directly, on the request thread. Suitable for CLI tools, single-process services, edge runtimes, embedded devices.

```rust
fn run_query(graph: &SharedGraph, registry: &dyn ProcedureRegistry, source: &str)
    -> Result<BindingTable, MyError>
{
    let stmt = parse(source)?;
    let analyzed = analyze(stmt, registry, None)?;
    let plan = plan(&analyzed, registry)?;
    let mut session = Session::new(graph);
    let output = execute_statement(&plan, &mut session, registry)?;
    match output {
        StatementOutput::Rows(t) => Ok(t),
        StatementOutput::Empty => Ok(BindingTable::empty(plan.output_schema.clone())),
    }
}
```

### 11.2 Tokio service wrapper

selene-db is **synchronous**. Wrap it for an async service by running every query on a `spawn_blocking` task. The graph is `Arc<SharedGraph>`; reads are lock-free, writes serialize on the graph's write lock — Tokio's blocking pool naturally provides backpressure.

```rust
async fn handle_request(state: Arc<AppState>, source: String)
    -> Result<BindingTable, MyError>
{
    let state = Arc::clone(&state);
    tokio::task::spawn_blocking(move || {
        run_query(&state.graph, &*state.registry, &source)
    })
    .await
    .map_err(|join| MyError::Internal(join.to_string()))?
}
```

Do **not** call selene-db from inside an async context without `spawn_blocking`: a query that takes longer than a Tokio polling slice will starve other tasks.

### 11.3 Embedded edge runtime

For static-build edge deployments (single-binary services, BACnet bridges, sensor aggregators), enable only the crates you need:

- `selene-core + selene-graph + selene-persist`: WAL-durable property graph with no parser or `CALL`.
- Add `selene-gql` if the device runs ad-hoc queries. `CALL selene.*` / `CALL algo.*` is served by the in-tree `BuiltinProcedureRegistry`; if the device never issues `CALL`, run the pipeline with `EmptyProcedureRegistry` and skip the projection-catalog state entirely.
- Add `selene-algorithms` only when you need graph algorithms (whether through `CALL algo.*` or the native Rust API).

The workspace is `#![forbid(unsafe_code)]` and pulls in no async runtime at any layer. `rustls`-only TLS is enforced by `cargo-deny`. There is no hand-rolled crypto, TLS, async runtime, or serialization primitive in the engine.

## 12. What the embedder NEVER does

| Don't | Why |
|:---|:---|
| Mutate `SeleneGraph` fields directly | Bypasses the mutation funnel; corrupts label indexes, property indexes, adjacency, and the WAL stream. Only `Mutator` writes are sound. |
| Construct `Change` values and feed them directly into an `IndexProvider` | The provider expects the engine's published snapshot to already reflect the change; out-of-band events drift the provider. |
| Bypass `db_string` when constructing label / property keys | Label and property keys are owned `DbString` values guarded by IL013. Construct keys through `selene_core::db_string` and clone the `DbString` when an API consumes the key. |
| Take a long-running lock inside a `Mutator` callback | The write lock is held; readers continue, but the next writer blocks for the full duration. Keep transactions short. |
| Block on async I/O on a read thread | `SharedGraph::read()` is lock-free, but a thread blocked on async I/O still holds its `Arc<SeleneGraph>`. Use `spawn_blocking`. |
| Re-enter `SharedGraph::begin_write` from inside an `IndexProvider::on_change` callback | Engine panics fast on same-thread re-entry; the outer commit still completes but the chained mutation does not. Cross-thread re-entry is documented misuse. |
| Open two `WalWriter`s on the same path | Exclusive OS-level file lock fails the second writer with `PersistError::WriterLockHeld`. |
| Reorder `Value` enum variants in a custom serializer | The variant order is canonical and append-only (Spec 02). Reordering breaks durability. |
| Embed a per-tenant database with one `SharedGraph` and trust the wrapper layer for isolation | One bug in the wrapper exposes cross-tenant data. Per-tenant `SharedGraph` is the safe default. |

## See also

- [`docs/architecture.md`](architecture.md) — decision log and crate layout.
- [`docs/graph-algorithms.md`](graph-algorithms.md) — the native `selene-algorithms` API and `CALL algo.*`.
- [`docs/gql-reference.md`](gql-reference.md) — supported ISO GQL surface.
- `README.md` — capability matrix, CI gates, license posture.
