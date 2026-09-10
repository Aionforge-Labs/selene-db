---
plan_id: F03-PR02
milestone: F03
initial_status: proposed
---

# F03-PR02 — Unify structural types, values and reference boundaries

**Milestone:** [F03: Complete the semantic compiler](Milestone-F03.md)
**Dependencies:** [F03-PR01](Milestone-F03-PR-01.md), [F01-PR02](Milestone-F01-PR-02.md)
**Carries forward:** M05-PR03; see the [complete crosswalk](06-OLD-TO-NEW-MAP.md).
**Issue closure:** None; do not close another PR’s issue.
**Focused skills:** `rust-api-design`; `rust-test-design`; `rust-storage-durability` from `yet-more-skills/skills/<name>/SKILL.md`.

## Outcome

Provide one structural type service and an explicit runtime/stored-value boundary so analysis, results and persistence agree without exposing lower bare-ID reference carriers.

## Start from what exists

The live facade re-exports selene_core::Value and selene_gql::GqlType as temporary bridges. Its database-scoped GraphRef/NodeRef/EdgeRef are different types from the lower bare-ID Value variants. This is a real integration prerequisite for the format-2 codec, not cosmetic API cleanup. Source: S05.

**Observed live entry points:** `crates/selene-db/src/lib.rs`, `crates/selene-db/src/params.rs`, `crates/selene-db/src/outcome.rs`, `crates/selene-db/src/handle.rs`, `crates/selene-core/src`, `crates/selene-gql/src`

Paths are navigation, not a closed edit inventory. New modules are implementation choices. Keep required callers and their tests with the behavior they support. Source IDs resolve in [SOURCE-NOTES.md](SOURCE-NOTES.md).

## Implementation sequence

1. Inventory the selected profile and existing value families before defining descriptors. Reuse correct scalar representations; add normalized structural descriptors for nullability, records/lists, graph references, paths and open/dynamic types actually selected.
2. Define separate operations for assignment, comparability, equality, distinctness, grouping and ordering. Do not force them through one Rust Eq/Hash/Ord implementation when language semantics differ.
3. Keep omitted result, empty binding table and null value distinct. Result schemas retain field names/types, ordering metadata and preferred output column order even when no rows exist.
4. Migrate parameters/results/runtime references away from ambiguous lower bare-ID carriers. Validate database/graph provenance when converting external handles. Persisted values use a separate explicit codec-facing contract; intern IDs and process-local DatabaseId are not durable data.
5. Replace facade GqlType/Value compatibility exposure with intentionally supported facade types or documented stable re-exports. Migrate all existing runtime consumers needed to make that boundary real; old byte codecs are only temporary internal debt until F02-PR08.

## Acceptance and concrete regression cases

- [x] Null comparison produces Unknown where required, but two nulls are not distinct for duplicate/grouping purposes; omitted result is neither of these.
- [x] Equivalent selected structural type descriptions normalize identically; unsupported types fail rather than widening to an untyped catch-all.
- [x] Record field names, list element types, reference provenance and nullable variants survive parameters → execution → typed result.
- [x] Foreign handles with matching numeric IDs are rejected, and deleted referents produce the required invalid-reference diagnostic when accessed.
- [x] Numeric equality/grouping/hash compatibility covers mixed selected exact numeric forms, signed zero and documented floating-point edge cases.
- [x] Stored-value tests reject ephemeral references/candidates or encode only explicitly supported durable semantic reference forms; no descriptor arena index becomes an on-disk type ID.

## Validation and performance

Run pure type-model tests plus facade parameter/result, GQL expression and reference tests. Use generated selected-profile inventories to ensure every supported value family has a case. Keep structural normalization snapshots as regression evidence, not an independent proof of semantic correctness.

Measure descriptor lookup/interning, conversion allocation and common scalar evaluation. Avoid global intern pools that keep database-owned types alive indefinitely.

Use the shared [validation guide](05-VALIDATION-AND-RELEASE.md) for runner mechanics and required PR/RC gates. These are planned checks, not reported passes.

## Keep out of this PR

No all-optional-type expansion, universal comparison feature by accident, typed errors collapsed to null, byte-format design before this boundary or compatibility aliases that preserve the ambiguity.

## Bridge/deletion boundary

The public lower Value/GqlType bridge ends here. Old encoded variants may remain only in isolated legacy persistence code until F02-PR08; temporary semantic lowering is deleted in F03-PR04.

## Owner decision and structural contract

The owner selected **query-only references** for IV011. Node and edge references,
paths, graph/table references, candidate tokens and process-local identities are
not property data. The restriction applies recursively to lists/records,
defaults, native mutation and logical property payloads. There is no durable
reference representation or StoreId serialization decision in this work item.
Scalars, native JSON/vectors and supported named recursive containers remain
storable. A positional `RecordTypeId` is not a substitute for semantic field
names in a stored record.

`selene-core::StructuralType` owns normalized nullability, scalar envelopes,
list element/bound metadata, named record fields and explicit analysis/reference
families. It uses owned structure with shared record field sets, not an immortal
intern pool. The facade's `Type` is this documented structural contract, not an
AST alias. `ExprTypeTable` retains the authoritative descriptor and lazily derives
its current-planner view through `type_adapter`; F03-PR04 deletes that view.
Request parameter descriptors enter analysis separately from immutable source,
including catalog re-preparation. Result schema and ordering are declared even
when execution returns zero rows.

The selected/admitted GV66/GV67 spellings have existing bounded membership and
cast behavior. Closed unions retain their component descriptors, flattened and
deduplicated with nullability carried once; they do not widen to `Dynamic`.
Preserving this tested subset does not change the profile's unsupported status
for the complete dynamic-union capability.

`StoredValue` is the checked semantic value boundary. It intentionally implements
neither serde nor rkyv: F02-PR03 owns its explicit byte encoding. Current property
map/diff codecs remain legacy adapters, with forbidden values rejected rather
than encoded as a process-local reference. This contract does not claim format-2
query durability. The facade exposes its own query `Value`, opaque
ownership-bearing reference values and validated paths. Lower runtime carriers
are private conversion inputs: `RecordTyped`, `Extended` and table references
have no facade value variant. Legacy serialized carriers remain isolated until
F02-PR08; F03-PR04 owns current-planner type lowering.

Annex D label corrections do not select capabilities: GA04 is universal
comparison, GA09 is path comparison, and GV70–GV72 describe immaterial/null/empty
value types. Null values remain supported independently of optional null-type
syntax. GQ01/GP16 bounded status, unsupported GT03 and `release_claimable=false`
remain unchanged. IV002 selects lexicographic constructed-value and stable
reference ordering; IV008 selects the owned structural normal form. IV010 is
inapplicable while GA04 is outside the selected closure. Dynamic noncomparable
families fail with `22G04`; paths support distinctness and ordering by their
element lists, while equality still requires unsupported GA09.

Duration comparison uses exact total months within the year/month group and
total nanoseconds within the day/time group. Equivalent units share predicate,
grouping, UNIQUE and index identity; zero belongs to either group. Mixed
nonzero groups fail comparability, including indexed probes. Canonical keys are
transient and leave the existing duration serialization unchanged.
Zoned temporal UNIQUE keys use timestamp identity, matching runtime equality
across zone spellings while preserving zone information in stored values.
Incremental and complete-state UNIQUE validation share the runtime's recursive
comparison-domain authority, independently per entity kind, declared type and
property. Incompatible domains fail before publication with `22G04`.

Foreign database/graph references fail before execution, even when raw IDs
collide. A deleted reference can be copied as an identity; property, label, path
construction, native graph procedure and mutation access reports `22G11`.
Repeated deletion of an already invalidated referent remains idempotent.
Detached transactions share graph-scoped monotonic allocation counters across
rollback, failure, overlapping drafts and runtime replacement. Issued reference
identities remain consumed even when their transaction does not publish.
Lexical `USE` results retain the graph selected for that statement, including
cached execution, without changing the ambient session. Focused-write syntax
retains its existing `42N01` rejection.

Shape admission is bounded before recursive matching/conversion. Native defaults,
legacy serde defaults and archived graph types reject query-only values and
excessive nesting before publication or replay. These checks preserve existing
legacy tags and framing; F02-PR03 owns the new stored-value byte format.

## Standards and reviewer focus

§§4.4, 4.12–4.17; §18.9; §19; §20; §§22.11–22.20. Check the exact selected numeric and reference rules, not host-language defaults.

**Independent review question:** Can the runtime, compiler and durable codec disagree about what a value means even though their Rust types compile?

Use [Luna execution guidance](03-LUNA-EXECUTION.md) for material decisions and the compact handoff. A necessary adjacent caller or mechanical migration is not itself a reason to stop; an incompatible semantic, public or durable contract is.
