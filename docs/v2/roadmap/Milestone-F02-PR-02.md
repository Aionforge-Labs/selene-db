---
plan_id: F02-PR02
milestone: F02
initial_status: proposed
---

# F02-PR02 — Unify catalog metadata for constraints, indexes and native registrations

**Milestone:** [F02: Bring durable embedding forward](Milestone-F02.md)
**Dependencies:** [PLAN-01](PLAN-01.md)
**Carries forward:** M08-PR01; see the [complete crosswalk](06-OLD-TO-NEW-MAP.md).
**Issue closure:** None; do not close another PR’s issue.
**Focused skills:** `rust-api-design`; `rust-storage-durability`; `rust-test-design` from `yet-more-skills/skills/<name>/SKILL.md`.

## Outcome

Make durable descriptor ownership explicit now, so persistence and native reintegration do not invent incompatible private registries later.

## Start from what exists

The catalog/facade are already implemented. This PR extends that ownership to the remaining index and constraint descriptors and establishes the registration data contract consumed by F04-PR06. It does not reimplement schema or named-graph lifecycle.

**Observed live entry points:** `crates/selene-db/src/catalog.rs`, `crates/selene-db/src/catalog_snapshot.rs`, `crates/selene-db/src/catalog_stage.rs`, `crates/selene-db/src/transaction.rs`, `crates/selene-catalog/src`

Paths are navigation, not a closed edit inventory. New modules are implementation choices. Keep required callers and their tests with the behavior they support. Source IDs resolve in [SOURCE-NOTES.md](SOURCE-NOTES.md).

## Implementation sequence

1. Inspect existing lower index, graph-type, provider and native procedure registries. Separate durable declarations from runtime handles, code pointers and derived accelerator state.
2. Add stable catalog identities, graph/type ownership, generation/dependency information and an explicit lifecycle for declarations: building/inactive versus validated/usable. These states must not make an unenforced constraint look active.
3. Define index targets as analyzed target descriptions with semantics/profile identity, not opaque closures. Reserve the expression representation that F05-PR06 completes; unsupported targets remain rejected.
4. Use the existing detached catalog draft and outer publication for descriptor create/drop/replace. Capture dependency invalidation so dropping an owned object cannot leave an advertised usable index or procedure binding.
5. Expose introspection and logical persistence records. Existing enforcement remains active until its replacement is complete; do not introduce a second authoritative registry.

## Acceptance and concrete regression cases

- [ ] Create/drop/rollback of an index or constraint declaration changes catalog and ownership together.
- [ ] Duplicate names, wrong graph/type owners and dangling dependencies fail with a stable structured diagnostic.
- [ ] A building or failed index is never selected by the planner or used as a complete constraint proof.
- [ ] Catalog generations invalidate dependent plans while an unrelated data publication does not masquerade as schema replacement.
- [ ] Logical descriptor serialization inputs contain stable IDs and declarative configuration, not pointer addresses or physical rows.
- [ ] An existing uniqueness rule stays enforced during migration; declarations without a supported implementation cannot be activated.

## Validation and performance

Run catalog lifecycle/draft rollback, dependency invalidation and profile tests. Add introspection fixtures and a lower-registry-to-catalog consistency test. Catalog-only tests are not sufficient evidence for constraint activation; F05-PR05 supplies that behavior.

Measure descriptor lookup and catalog snapshot clone/publication overhead at realistic graph/index counts. Avoid copying accelerator payloads into catalog snapshots.

Use the shared [validation guide](05-VALIDATION-AND-RELEASE.md) for runner mechanics and required PR/RC gates. These are planned checks, not reported passes.

## Keep out of this PR

No independent publication root, callable code in persisted descriptors, active-but-unenforced constraints, new DDL grammar without profile ownership or complete expression-index execution yet.

## Bridge/deletion boundary

Any lower registry retained is a derived runtime view with a named owner: constraints/index execution F05-PR05/06; native registry integration F04-PR06/08.

### Implemented authority and downstream handoff

```text
Existing typed input / unpublished native mutation changes
  → DatabaseDraft catalog declarations + checked allocation high-water marks
  → whole-catalog ownership/dependency/profile validation
  → derived owner-local graph bindings and existing constraint validator
  → publish_database_draft (one outer store)
  → immutable facade inspection / logical catalog records
```

`CatalogPayload::Index`, `Constraint`, and `Procedure` carry typed data rather
than unit markers. Graph/type declaration names have owner-local namespaces;
primary graph/type/schema naming is unchanged. Shared dependencies pin stable
IDs and descriptor revisions. Owner removal cleans its children only after the
existing drop admission; foreign dependants cause RESTRICT failure. Publication
rejects changed descriptors without advancing revisions or matching runtime
replacements. Pure data writes do not regenerate declarations or advance their
catalog generation.

`CatalogReadSnapshot::{declarations,native_procedures,logical_catalog}` exposes
logical inspection. `Catalog::{declare,drop_declaration}` administers inactive,
building or failed declarations; a caller cannot assert ready activation. Active
index creation/drop stays in the existing native/DDL mutation funnel. Active
uniqueness cannot be disabled by removing metadata. Replacement uses fresh IDs;
logical record exports include allocation bounds that retain deleted IDs and
deterministic create/replace/drop events for F02-PR03. They are not a WAL codec,
graph snapshot codec, reopen implementation, or durable facade commit.
Native signature deserialization enforces its 64-level nesting budget while
decoding, not only after allocation. Independent postcard fixtures cover the
64/65-level boundary and reject a 10,000-level hostile input; bounded descriptor
proptests complement the logical record round trips.

| Registration | Current binding and limits | Owning next step |
|---|---|---|
| Scalar node/edge, composite node, vector, text indexes | Unpublished schema events become declarations before the outer store. Native indexes are derived implementations. Planner discovery requires the bound declaration and current native configuration; scalar/composite completeness still depends on live drift state. | F05-PR05/06 |
| Existing arity-one uniqueness | Closed-type input annotations normalize to graph-owned declarations. The existing missing/null-skipping whole-state/commit validator remains enforced; runtime annotations are a checked derived adapter, not a new uniqueness switch. No backing index is manufactured. | F05-PR05 replaces the adapter with complete named composite/key enforcement. |
| Known native procedures | The existing closed specification inventory produces both frozen runtime adapters and logical signatures, defaults, outputs, effects, symbolic bindings and profile coordinates. Engine-owned declarations do not create user-visible builtin schemas. Publication cannot change this frozen inventory or activate unknown code. | F04-PR06 |
| Candidate-provider / projection configuration | Typed inactive contracts only. Direct lower provider attachment and graph-scoped ephemeral algorithm caches are not durable facade registrations. Read-tier cache activity remains read-tier; it does not become a catalog write. | F04-PR06/08 |
| Expressions / named composite unique and keys | Expression shapes are typed reserved data; construction is rejected pending analysis/execution. Unsupported constraints cannot become ready. | F05-PR05/06 |

Graph bindings retain only their owner's immutable descriptor subset, not a
whole historical catalog per graph. Neither logical snapshots nor bindings copy
accelerator contents. A ready descriptor is durable admission metadata, not a
permanent assertion that a current accelerator is complete. The facade still
rejects maintenance requests with its existing selected-mode diagnostic.

Shared vector construction caps now live with core declarative configuration;
catalog and graph use the same bounds. The pre-delivery correction additionally:

- rejects cross-kind `declare` collisions for Strict, IfNotExists and OrReplace
  before allocation/publication, preserving same-kind policy behavior;
- binds each physical registration to exactly one declaration by owner,
  effective explicit/generated name, target and configuration, leaving unrelated
  inactive alternatives unbound;
- resolves trusted drop events by bound stable identity, including ordered
  create/drop/recreate lists, rather than deleting all declarations at a target;
- gates equality/union/range/prefix candidates and inherited cardinality paths;
  required GQL filters use a metadata-only eligible view and still distinguish
  unavailable indexes from admissible-but-drifted indexes needing a scan;
- preserves and revalidates owner-local bindings through advanced graph
  compaction, without enabling facade maintenance or retaining physical rows;
- keeps vector/property diagnostics physical and auditable independently of
  query eligibility; no unchecked query/row API was added for diagnostics;
- compiles keyed binding metadata at admission so scalar/vector lookups do not
  scan all owner declarations or reconstruct String/Vec keys per probe. Current
  native configuration/name and data-dependent completeness checks remain.

The matched pre/post runtime-access fixture and refreshed publication rows are
in `BENCHMARKS.md`; older worktree numbers there are explicitly historical.

The dependency direction remains acyclic:
catalog consumes only core/profile; graph and GQL consume catalog data, never the
reverse. No external package was added and no persisted legacy enum was reshaped.

Measurement commands, scales, clock boundaries and absolute intervals are in
the catalog section of `BENCHMARKS.md`. Structural memory accounting explicitly
excludes declaration payload heap allocations; it is not total retained memory.

## Standards and reviewer focus

§4.2.5 catalog; §4.13 graph types; native constraints/index declarations are implementation facilities unless specifically standardized.

**Independent review question:** Can every usable runtime registration be explained by one catalog declaration and the current graph/profile generation?

Use [Luna execution guidance](03-LUNA-EXECUTION.md) for material decisions and the compact handoff. A necessary adjacent caller or mechanical migration is not itself a reason to stop; an incompatible semantic, public or durable contract is.
