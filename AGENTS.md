# AGENTS.md - selene-db

Repository-specific operating guide for AI agents working in `selene-db`.
`CLAUDE.md` is a symlink to this file, so keep this file current, concise, and
specific to this repository.

The global partner workflow still lives outside the repo:

- Executor summary: `~/.codex/AGENTS.md`
- Full workflow spec: `/Users/justin/Development/_agent_helpers/partner-workflow.md`

This file is not a changelog, milestone ledger, or architectural archive. Do not
put fast-moving PR state here. Use the current source, tests, `BENCHMARKS.md`,
GitHub state, and local `_goalslogs/` notes for that.

## Mission

`selene-db` is a greenfield, embeddable Rust graph database centered on strict
ISO/IEC 39075:2024 GQL and agentic-memory-oriented graph retrieval. It is a
single native engine:

- GQL is the only query and mutation language.
- Graph storage, graph algorithms, dense vectors, vector indexes, persistence,
  and the native procedure registry live in-tree as one cohesive engine.
- Non-standard capabilities are exposed through implementation-defined values,
  indexes, and `CALL selene.*` / `CALL algo.*` procedures, never grammar drift.
- There is no loadable extension pack system.
- There are no downstream compatibility constraints yet. Large refactors are
  allowed when they improve correctness, performance, or engine cohesion and are
  backed by tests and benchmark evidence.

Read the current `Cargo.toml` / `rust-toolchain.toml` for versions. The workspace
uses Rust edition 2024 and a pinned stable toolchain.

The [2.0 line and 1.x end-of-life policy](docs/v2/eol-and-version-policy.md) is
the version-policy source of truth. The 1.x line receives no maintenance or
migration support. Treat `2.0.0-alpha.1` as a source coordinate unless its
crates.io publication has been verified.

## 2.0 Program Contract

[`docs/v2/README.md`](docs/v2/README.md) is the tracked 2.0 program entry
point. Its finalized decisions, machine plan, milestone/work-item projections,
review protocol, issue ownership, risk register, and conformance policy are the
portable execution contract. Untracked local notes are supplementary and must
not be required to execute a work item.

The no-facade, row-executor workspace at `c5c0a985` is the historical 2.0
baseline. The current workspace includes the generated profile authority plus
the `selene-db` facade and `selene-catalog` boundary. M06 replaces row execution,
and M09 adds format 2. Do not claim those targets exist before their owning work
items merge.

The implementer edits repository files and runs tests only. It does not stage,
commit, push, create or update a PR, submit review output, or merge. The
orchestrator owns commits, pushes, non-draft PR creation and updates,
consolidated independent-review comments, and eligible authorized merges. One
independent read-only review is the default (with a focused second lens when
useful for durability, concurrency, or complex semantic risk) without adopting
the implementer or orchestrator's conclusions.

The orchestrator may merge only when the final reviewed head is unchanged,
required exact-head checks are green, final review is Blocker/Major-clean,
repository policy and branch protection permit the merge, scope and worktree
state are clean, and the user has given explicit authorization. A changed
head voids PASS. This role split does not authorize self-approval, auto-merge,
release, publication, tagging, reactions, or branch-protection changes.

Every 2.0 handoff includes:

```text
Plan ID:
PR URL:
Base SHA / Head SHA / commits:
Outcome delivered:
Files/subsystems changed:
Public API and persisted/profile changes:
Commands and results:
Benchmarks/fuzz/crash evidence:
Decisions and deviations:
Temporary bridges and deletion owner:
Known risks/follow-ups:
Reviewer questions:
```

Skipped commands are reported as unrun with the reason; they are never green.

## Hard Rules

1. Preserve strict ISO GQL at the language boundary. Do not add SQL, Cypher,
   SPARQL, or ad hoc grammar.
2. Route mutations through the proper graph/mutation/maintenance funnels. A
   read-only built-in must not mutate or re-enter the write path.
3. Keep `#![forbid(unsafe_code)]` and `missing_docs = "deny"` working
   workspace-wide. Any future unsafe exception needs explicit design rationale.
4. Keep tracked Rust files below 700 LOC. Split modules before they approach the
   cap.
5. Prefer no new dependencies. A dependency must be maintained, current,
   license-compatible, and clearly better than local code. Well-supported
   SIMD/NEON/acceleration crates are acceptable when evidence justifies them.
6. Do not hand-roll crypto, TLS, async runtimes, or serialization primitives.
7. Keep the rustls-only posture. No `native-tls`, no `openssl-sys`.
8. Preserve dual MIT OR Apache-2.0 licensing and third-party attribution:
   `NOTICE`, `THIRDPARTY.md`, and per-file attribution for adapted third-party
   code.
9. Benchmark claims require commands, scales, and numbers.
10. Do not optimize beyond the evidence. Queue larger research when the next
    step needs a broader design.

Use GQLSTATUS codes for query/runtime errors. Do not introduce SQLSTATE-style
codes or SQL-only syntax.

## Values And Defaults

Native values are engine data, not side channels. Property-default support must
preserve the same value invariants as runtime writes:

- `PropertyDefaultValue` is durable schema metadata and must stay serde/rkyv
  round-trippable.
- `LIST<T>` property defaults are recursive list descriptors. Validate their
  elements against `PropertyElementType`, including nested lists and
  `LIST<VECTOR>`, instead of relying on container-only
  `PropertyValueType::matches`.
- `RECORD` property defaults are recursive field descriptors. Open records
  preserve source field names; closed records must validate against
  `RecordFieldTypes`, including nested records, lists, vectors, and JSON fields.
- `SHOW NODE TYPES`, insert materialization, WAL replay, and graph snapshots are
  part of the same default-value contract.

## Workspace Map

`selene-db` is the stability-promised facade. Keep lower dependency direction
intentional:

`selene-core -> selene-graph -> selene-algorithms -> selene-gql`

`selene-profile` feeds GQL and catalog consumers. `selene-catalog` depends only
on `selene-core` and `selene-profile`; the facade composes the lower layers.
`selene-persist` depends on `selene-core` and stays below graph semantics.
`selene-testing` provides fixtures, corpus helpers, OpenRouter/local embedding
support, and benchmark profiles for dev-dependencies.

Managed persistence I/O uses retained `StoreDirectory` handles, never diagnostic
locators. Initial path wrappers anchor once; callers may also supply an already
open directory file. Native filesystem mode supports Linux/macOS only and
returns typed unsupported-platform errors elsewhere. See
[`docs/store-directory-control.md`](docs/store-directory-control.md) for file,
lock, platform, and empty-control guarantees. The infallible builder remains
memory-only; fallible facade create/open/checkpoint use one format-2 authority.
See [durable commit](docs/v2/durable-commit.md) for separate typed outcomes and
[checkpoint/reopen](docs/v2/checkpoint-reopen.md) for serialized checkpoint writes,
eager all-index reconstruction, Rust schema construction and non-destructive open.
`Database::verify`/`verify_in` share full open readiness through a read-only,
existing-only selection epoch and artifact lease; they never acquire writer LOCK,
synchronize or publish a Database. Reports cover only the captured on-disk view,
not acknowledgment, physical durability, write permission or subsequent freshness.
See [recovery verification](docs/v2/recovery-verification.md).
Unrotated PR05 selections verify the full WAL prefix. Explicit checkpoint adopts
the [rotating lifecycle](docs/v2/rotation-retention.md); rotating reopen verifies
only the independently selected new segment. Prune is explicit, never automatic,
and retains the latest two completed checkpoints plus active artifact leases.
The richer
Rust schema builder does not enable unsupported GQL catalog GG02 grammar.

Persistence-directory readers participate in the same epoch lock domain as
rotation and prune. Legacy recovery and backup-style reads hold
`PersistenceReadGuard` from authoritative MANIFEST selection through snapshot
and WAL use. Format-2 `LogicalReader` instead pins an independently opened
immutable manifest with a shared file lock before releasing the selection epoch.
That lock retains its named snapshot/WAL dependencies through consumption without
blocking publication. Prune probes those locks nonblockingly under writer proof
and the exclusive epoch, retaining and accounting for leased names until release
or process death. It revalidates and synchronizes CURRENT and selected dependencies
before unlink, including after uncertain publication/reopen.
`CheckpointOutcome` paths are not retention leases.
Writer ownership is one permanent `LOCK` domain; WAL/audit composites share its
owned lease. Every managed publication/prune requires that StoreWriter proof;
standalone conveniences acquire it, while online callers pass the existing
lease. Exclusive epoch guards retain the proof; read guards remain independent.
Empty-control reopen validates CURRENT plus its self-contained selected manifest,
not unselected ancestor files. Parent links are provenance, not retention leases
or authenticated rollback protection. `SharedGraph::recover` locks an existing `wal.log` before the shared epoch
guard; a missing-WAL bootstrap verifies recovery under the guard before a
non-blocking writer open. Recovery callbacks must not re-enter same-directory
epoch mutation.

| Crate | Owns |
|---|---|
| `selene-profile` | Typed GQL profile source, validation, canonical hashing, and checked-in runtime/documentation generation. It has no engine-crate dependencies. |
| `selene-catalog` | Storage-neutral catalog descriptors, typed stable IDs, immutable snapshots, and mutation drafts. It has no engine-crate dependencies. |
| `selene-core` | Foundation values and identifiers: `Value`, `VectorValue`, `JsonValue`, vector metrics/top-k helpers, `DbString`, schema/value types, the profile compatibility adapter, property maps, codecs, and changesets. |
| `selene-graph` | In-memory graph storage, `SharedGraph`, `Mutator`, row/id maps, property/composite indexes, vector indexes, exact/ANN/candidate vector search, exact BM25 text search, exact JSON search, reusable BM25 postings indexes, recovery provider, compaction, and graph type enforcement. |
| `selene-persist` | WAL, snapshots, MANIFEST recovery, audit log, retention, and prune. It does not own graph semantics. |
| `selene-algorithms` | Projection catalog plus native structural, pathfinding, centrality, and community algorithms. It never depends on GQL. |
| `selene-gql` | Parser, AST, analyzer, planner, optimizer, executor, procedure tiers, and the concrete native `BuiltinProcedureRegistry`. |
| `selene-db` | Stable database builder, ownership root, lifetime-free facade sessions, facade diagnostics, and summary outcomes. Lower engine types are not re-exported by default. |
| `selene-testing` | Shared fixtures, graph generators, OpenRouter/local embedding corpus and client support, benchmark profiles, and snapshot-harness support. |

## Query And Procedure Surface

- `spec/gql-profile/profile.json` is the feature and implementation-choice
  source. `selene-profile` exposes the generated typed capability, identity,
  and Annex B lookup APIs used by runtime consumers.
- `ProcedureRegistry` is the planner/executor/test seam. It is not a third-party
  extension point.
- `BuiltinProcedureRegistry` is the production registry. Procedure names,
  counts, signatures, and metadata are pinned by source and surface tests; update
  those tests and docs together when the surface changes.
- Procedure tiers are load-bearing:
  - graph tier: read-only health, feature status, verify, vector search/score,
    vector candidate-state discovery/composition, vector index stats, property
    index drift/cardinality stats, BM25 text
    search/candidate scoring, and JSON candidate search;
  - mutation tier: property, vector, and text index create/drop;
  - maintenance tier: vector index rebuild and rebuild recommendation.

Keep native procedure APIs policy-neutral. Agentic-memory use cases should be
able to compose graph, vector, text, and JSON primitives without the engine
hard-coding one retrieval policy.

## Vectors

Vectors are first-class engine data:

- `Value::Vector(VectorValue)` is the native value variant.
- `VectorValue` stores finite, non-empty `f32` components behind shared storage.
- GQL admits `VECTOR` as an implementation-defined type name.
- `CAST(<LIST<numeric>> AS VECTOR)` is the native GQL producer for finite `f32`
  vector values; keep vector-literal grammar out unless a later spec/design
  decision explicitly earns it.
- `VECTOR` property defaults accept numeric list literals and persist as
  canonical `PropertyDefaultValue::Vector` component bits.
- `MAX_VECTOR_DIMENSION` is `u16::MAX`.
- Supported metrics are lower-is-better `squared_euclidean`, `cosine`, and
  `negative_inner_product`.
- Core vector kernels use safe SIMD through `wide` where available while
  preserving `f64` score semantics.

Native vector indexes are graph indexes over `(label, property)`:

- `Flat`
- `HnswSquaredEuclidean`, `HnswCosine`, `HnswNegativeInnerProduct`
- `IvfSquaredEuclidean`, `IvfCosine`, `IvfNegativeInnerProduct`
- `TurboQuantCosine`

Index registrations are durable schema state. HNSW/IVF/TurboQuant accelerators
are derived in-memory state and must be rebuildable from primary graph values.
Delete, update, rebuild, WAL/snapshot recovery, and stale-index visibility are
required correctness concerns for vector-index changes.

Current production vector primitives include:

- exact node search and batch exact search;
- ANN node search and batch ANN search;
- explicit candidate scoring and batch candidate scoring;
- one-hop graph-neighbor scoring and batch neighbor scoring;
- graph-expanded root scoring and batch graph-expanded root scoring;
- ANN-root graph expansion with exact rerank, plus the batched companion;
- reusable `VectorCandidateSet` for canonical sorted/deduped `NodeId` sets;
- candidate-set scoring, batch candidate-set scoring, candidate-set algebra, and
  conversion from vector-search hits;
- `MaintainedCandidateStateProvider` for named, provider-owned graph-derived
  candidate sets with generation checks, WAL/snapshot recovery, and discovery
  through `selene.vector_candidate_states()`;
- maintained-state scoring, maintained-state + explicit-node algebra, and
  maintained-state + graph-expanded-root composition, including the batched
  GQL state-expanded root scorer for multi-query graph-root workloads;
- state-gated ANN-root graph expansion when benchmarks prove quality, but avoid
  adding more ANN/state surfaces by default when real-embedding rows show poor
  precision;
- vector index stats, create/drop, rebuild, and recommended rebuild.

Keep new compression and alternative ANN ideas out of production until evidence
earns a design. TurboQuant cosine is the first production compressed candidate
index and must preserve exact `VECTOR` values with exact final rerank. PQ,
IVF+PQ, binary quantization, OPQ, ScaNN, and DiskANN remain research/benchmark
code first.

## Graph Algorithms And Retrieval

`selene-algorithms` is mandatory native engine functionality. Algorithms operate
over `GraphProjection`, not live mutable graph state. Keep algorithm
implementations independent of GQL; GQL bindings are registry adapters over the
native Rust API.

Graph-accelerated vector retrieval is an active product track. Prefer
composable primitives:

- graph-derived candidate sets;
- `VectorCandidateSet` composition;
- exact vector scoring over explicit candidates;
- graph-neighbor vector scoring;
- graph-expanded support/provenance/root expansion;
- maintained or rebuildable active/current/support sets when benchmarks justify
  them;
- graph algorithm priors only when they improve candidate coverage, diversity,
  correctness, or latency.

Do not bake one agent-memory policy into the engine. Build reusable graph/vector
substrate and benchmark product-shaped retrieval rows. Current OpenRouter and
legacy local embedding evidence says graph-authored hints and support expansion
can restore precision where vector-only ANN/exact search fails under ambiguous
language; the remaining work is better root production, maintained-state
ownership, and invalidation/recovery, not piling on ANN fallback surfaces.

## JSON

JSON is native engine data for agentic workloads:

- `Value::Json(JsonValue)` is the native value variant.
- `JsonValue` stores validated RFC 8259 JSON behind shared storage and renders
  deterministic compact JSON with sorted object keys.
- Serde/postcard boundaries encode canonical JSON text and validate on decode.
- `PropertyValueType::Json` / `PredefinedValueType::Json` allow graph schema
  declarations such as `payload :: JSON`.
- JSON property defaults are accepted through JSON-typed string literals such as
  `payload :: JSON DEFAULT '{"kind":"episodic"}'`; the catalog validates and
  canonicalizes the JSON text and `SHOW` renders it as an escaped string
  literal.
- `selene-gql` exposes `JSON` as an implementation-defined type name with typed
  parameters, `IS TYPED JSON`, `CAST(<string> AS JSON)`, `CAST(<json> AS
  STRING)`, and scalar functions `json`, `json_parse`, `json_stringify`,
  `json_type`, `json_array`, `json_object`, `json_array_length`,
  `json_object_keys`, `json_contains`, `json_merge_patch`, `json_patch`,
  `json_get`, `json_get_text`, `json_get_scalar`, `json_get_path`,
  `json_get_path_text`, `json_get_path_scalar`, and `json_has_path`.
- `json_array` and `json_object` construct JSON through the scalar-function
  surface. They convert SQL null/bool/string/i64/u64/finite-float/list/JSON
  values to JSON; wider exact numerics, temporal values, bytes, graph
  references, vectors, and records need explicit user-side conversion first.
  `json_object` rejects duplicate keys before construction.
- JSON equality is value equality. JSON is not an order-comparable family; range
  comparisons must reject rather than inventing nested document order.
- `json_get` is a shallow object-key / array-index selector; it is not JSONPath.
- `json_get_path` / `json_get_path_text` are bounded path selectors over object
  keys and array indexes with a 64-selector cap. They accept either variadic
  selector arguments or the same selector-array JSON document used by
  `selene.json_path_*` procedures. They are not JSONPath.
- `json_get_scalar` / `json_get_path_scalar` extract selected JSON null,
  boolean, number, and string leaves as native GQL scalar values. They reject
  selected arrays/objects instead of canonical-stringifying containers.
- `json_has_path` is the companion bounded existence predicate with the same
  variadic-or-selector-array path forms. It returns true for selected JSON null
  values and false for absent paths.
- `json_contains` is recursive subset containment over JSON values: object
  candidates require matching contained keys, array candidates require each
  candidate element to be contained by some target element, and scalar candidates
  match by value or by membership in a target array.
- `json_merge_patch(target, patch)` applies RFC 7396 object merge-patch
  semantics copy-on-write. It removes object members whose patch value is JSON
  null, recursively merges object members, and replaces the target for
  non-object patches.
- `json_patch(target, patch)` applies RFC 6902 JSON Patch arrays copy-on-write
  using RFC 6901 JSON Pointer paths. It supports `add`, `remove`, `replace`,
  `move`, `copy`, and `test`; invalid patch documents or failed operations are
  data exceptions and do not expose partial document state.
- `selene.json_contains_nodes(label, property, candidate, k)` is the exact
  graph-tier candidate producer for JSON-valued node properties. It is the
  correctness oracle before maintained JSON/path indexes exist.
- `selene.json_path_exists_nodes(label, property, path, k)` is the exact
  graph-tier candidate producer for JSON selector-array path existence. Path
  arrays use string object keys and integer array indexes, including negative
  indexes from the end; this is intentionally not JSONPath.
- `selene.json_path_contains_nodes(label, property, path, candidate, k)` is the
  exact graph-tier candidate producer for recursive containment over the JSON
  value selected by a selector-array path.
- `selene.json_path_value_nodes(label, property, path, k)` is the exact
  graph-tier companion that returns `node_id` plus the selected JSON `value`.
  JSON null path values are returned as `Value::Json`, not SQL `NULL`, so
  callers can distinguish present null values from absent paths.

Keep JSON grammar strict. Defer JSON literals, RFC 9535 JSONPath,
maintained JSON indexes, and hybrid JSON/text/vector retrieval surfaces until
they have focused design, recovery semantics, tests, and benchmarks.

## BM25 / Full Text

BM25/full-text is now a native first slice, not grammar syntax:

- `selene-graph` owns the dependency-light exact BM25 scan over `STRING` node
  properties and the reusable in-memory `TextIndex` postings primitive.
- `selene-gql` exposes global search as
  `CALL selene.text_search_nodes(label, property, query, k) YIELD node_id, score`
  and explicit candidate scoring as:
  - `CALL selene.text_score_nodes(label, property, query, nodes, k) YIELD node_id, score`
  - `CALL selene.text_score_nodes_batch(label, property, queries, nodes, k) YIELD query_index, node_id, score`
  - `CALL selene.text_score_candidate_state_expanded_batch(label, property, queries, state_name, roots, edge_label, k, operation?, direction?) YIELD query_index, node_id, score`
- `selene.create_text_index`, `selene.drop_text_index`, and
  `selene.text_index_stats` manage durable maintained text-index registrations.
  Postings are derived in-memory state, maintained through graph mutations, and
  rebuildable from primary graph values during recovery/compaction.
- The exact scan is the correctness oracle and small-corpus path. Maintained
  `TextIndex` lookup is the repeated-query path; candidate-scoped scoring
  requires a registered text index so read calls do not hide transient postings
  builds. State-expanded BM25 scoring composes maintained candidate state with
  graph-expanded roots before text scoring.
- Keep ISO GQL grammar strict; add future surfaces through native values,
  indexes, and procedures as appropriate.
- Ground richer analyzer, segment, or disk-backed designs against Tantivy and
  the old local SeleneDB donor code, but do not dependency-import or copy old
  code without fresh tests and benchmarks.
- Benchmark text-only and hybrid graph/vector/text retrieval against the exact
  BM25 oracle.

## Performance Posture

Expected workload shape is read-heavy but write-relevant, roughly 60% reads and
40% writes.

- Use Rayon only where workload size justifies parallel overhead.
- Use safe SIMD/NEON acceleration where stable and measurable.
- Keep library crates allocator-agnostic. Bench binaries use mimalloc. A global
  engine allocator requires a measured decision.
- WAL durability is often the write-side cost center. Do not over-optimize
  provider/index maintenance paths when durability dominates.
- A larger WAL rewrite/refactor is queued after the current value/spec work.
  Ground it in scalar/vector/JSON payload benchmarks, compression and checksum
  research, replay locality, group commit, and segment/snapshot trade-offs
  before changing the persistence format.
- Prefer product-shaped benchmark rows once API boundaries are correct.
- Treat OpenRouter/live embedding rows as opt-in local validation. CI must
  compile those code paths but must not require embedding services or secret
  `.env` material.

## Validation

Use repository scripts rather than recreating CI inline.

Common full local gate for code changes:

```bash
cargo fmt --all --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked --all-features --profile default
cargo test --workspace --locked --all-features --doc
cargo doc --workspace --no-deps --locked
cargo deny --exclude-dev check bans
cargo deny check licenses sources
cargo audit -d /private/tmp/selene-advisory-db
bash .github/scripts/check-file-size.sh
bash .github/scripts/check-no-secrets.sh
bash .github/scripts/check-thirdparty-current.sh
bash .github/scripts/check-no-rowid-arith.sh
bash .github/scripts/check-no-version-locked-feature-error.sh
bash .github/scripts/check-bench-invocation.sh
bash .github/scripts/check-benchmarks-doc.sh .
bash .github/scripts/check-doc-constants.sh
bash .github/scripts/check-mimalloc-dev-dep.sh
git diff --check
```

Run focused tests and focused benchmark compiles before the full gate when a
change has a narrow surface. Run parser/persist fuzz when parser or decoding
risk changes.

```bash
cd crates/selene-gql/fuzz
cargo +nightly fuzz run parse_gql -- -max_total_time=60
```

Docs-only changes can use cheap gates, but still run `git diff --check`,
formatting, file-size, secret scan, and any doc registry scripts they affect.

## Benchmarks

`scripts/run-benches.sh` is the sanctioned benchmark entry point. Do not run
`cargo bench --workspace`; Cargo can run bench binaries concurrently and pollute
wall-clock medians.

Useful invocations:

```bash
scripts/run-benches.sh --list
scripts/run-benches.sh --smoke
scripts/run-benches.sh --bench vector_graph_retrieval --compile-only
scripts/run-benches.sh --profile quick --bench vector_graph_retrieval
scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter vector
scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter procedure_vector_omlx_query_roots
scripts/run-benches.sh --profile quick --bench single_graph --filter graph_vector_candidate_set --vector-scales 1000
scripts/run-benches.sh --profile quick --bench vector_index_rebuild --vector-scales 10000
scripts/run-benches.sh --profile quick --bench text_search_bm25
scripts/run-benches.sh --profile quick --bench vector_wgpu --filter core_vector_wgpu_prototype
scripts/run-benches.sh --bench vector_index_rebuild --allocator system
```

The runner is Criterion-only. There is no active iai-callgrind/valgrind layer.
Every committed bench target must be registered in `scripts/run-benches.sh` and
documented in `BENCHMARKS.md`.

OpenRouter embedding benches are the preferred opt-in live validation path;
local oMLX remains available by setting `SELENE_EMBEDDING_PROVIDER=omlx` or the
legacy `SELENE_OMLX_*` variables:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=project_source_chunk_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter procedure_vector_omlx_query_roots
```

Never print or commit the local API key. Keep 8B and larger rows opt-in unless a
specific PR needs that stress point.

## CI, Hooks, And PR Flow

- `development` is the integration trunk.
- Release PRs go from `development` to `main`.
- Non-draft PRs to `development` run formatting and repository-policy checks,
  a workspace all-features compile/nextest lane, and the 2.0 plan contract;
  dependency gates remain conditional on manifest changes.
- PRs to `main` run the full release workflow: clippy, nextest, doctests, deny,
  audit, third-party attribution, macOS validation, and fuzz.
- `.githooks/pre-commit` mirrors cheap local checks.
- `.githooks/pre-push` runs fast workspace clippy.

Install hooks once per clone:

```bash
scripts/install-hooks.sh
```

Use conventional commits with meaningful scopes, for example:

```text
feat(vector): add candidate set algebra
bench(algorithms): add graph retrieval pressure rows
docs(workflow): refresh agent instructions
```

Use GitHub connector tools when available for permitted read-only state checks
and CI polling. The implementer returns the tested worktree and handoff without
Git or GitHub mutations. The orchestrator performs the permitted commit, push,
non-draft PR, consolidated review-comment, and eligible merge operations. If a
permitted orchestrator operation is unavailable, use `gh` and request network
escalation when sandboxing blocks it.

After every merged PR in this long-running goal workflow, the orchestrator or
repository owner runs:

```bash
git switch development
git pull --ff-only
cargo clean
```

Delete merged feature branches locally and remotely when appropriate.

## Local Working Docs

Top-level underscore directories are local-only and gitignored:

- `_briefs/`
- `_design/`
- `_review/`
- `_spec/`
- `_plan/`
- `_goalslogs/`

Never commit them. `_goalslogs/` is the preferred place to keep local research
notes, PR logs, benchmark ledgers, and follow-up queues for this long-running
goal. Keep those notes short, dated, and tied to evidence.

Tracked docs such as `AGENTS.md`, `BENCHMARKS.md`, `NOTICE`, `THIRDPARTY.md`,
workflow files, scripts, and crate docs are normal repository artifacts and may
be committed.

## Donor Code

The user owns these donor codebases. They are reference material, not runtime
dependencies:

| Path | Use |
|---|---|
| `/Users/justin/Development/SeleneDB/` | Original prototype. Useful for vector/BM25 archaeology and cautionary examples. |
| `/Users/justin/Development/AetherDB/` | Scope-reduction fork and storage/API lessons. |
| `/Users/justin/Development/AgentAether/aether-db/` | Library-only fork; useful for audit/funnel patterns, not extension-pack revival. |
| `/Users/justin/Development/rusty-bacnet/` | Codec/parser donor patterns. |

Archived MCP services and donor code are not authoritative for this repository.
Current source, tests, benchmarks, and GitHub state win.

## Recurring Footguns

- Do not describe vectors as externalized or out-of-tree.
- Do not revive procedure packs, manifest validation, or loadable extensions.
- Do not treat the exact BM25 scan or transient `TextIndex` builds as durable
  maintained text-index registrations.
- Do not add BM25/full-text as a grammar shortcut or dependency-first import.
- Do not use synthetic vector metrics for real-embedding rows unless the PR is
  explicitly testing that metric; cosine is the default semantic benchmark
  metric.
- Do not commit `_goalslogs` or any other `_*/` working directory.
- Do not bypass row-id mapping. External `NodeId`/`EdgeId` are stable; internal
  `RowIndex` is storage position.
- Do not skip benchmark documentation for new benchmark targets.
- Do not leave build artifacts between merged PRs; run `cargo clean`.
- Do not mix broad refactors into behavior PRs unless the refactor is needed to
  make the behavior correct and testable.
