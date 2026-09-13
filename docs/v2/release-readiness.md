# F06-PR01 release readiness and embedding contract

This note describes the `2.0.0-alpha.1` source line, not publication, certification
or permission to release. [F06-PR02](roadmap/Milestone-F06-PR-02.md) owns package,
external-consumer and native release qualification. The version is a source
coordinate until publication is independently verified.

## Declaration and evidence boundary

Selene implements selected ISO/IEC 39075:2024 syntax and semantics plus disclosed
native extensions. The generated gate permits **“ISO-aligned with disclosed
conformance gaps; not a complete selected-profile claim.”** No minimum-conformance
claim is made. Minimum conformance includes all non-optional syntax/semantics and
the required graph, type and Unicode conditions; an optional subset cannot shrink
that minimum. Incorrect agreed behavior remains a defect regardless of wording.

These are separate authorities:

- Canonical profile (`spec/gql-profile/profile.json`): target selections,
  runtime inventory, claim state, choices and extension identities.
- Generated feature matrix (`docs/gql/conformance/features.md`): direct selections,
  Table 10 implication closure and feature-specific evidence references. The
  closure is an identity/integrity check, not a feature-count progress metric.
- Generated Annex B report (`docs/gql/conformance/implementation-defined.md`): each
  applicable choice or explicitly pending decision, rationale and references.
- Generated claim report (`docs/gql/conformance-evidence.md`): static registrations
  and exact blocker IDs. Only executing the runner produces pass/fail observations;
  checked-in static `complete` dispositions are not hand-authored pass records.
- Workspace regression tests: broader behavior evidence, not an independently
  complete normative rule inventory. Parser corpus admission is not execution.

## Known gaps: no silent scope decision

The canonical target still includes unsupported or partial runtime families:
GC03, GE04, GE05, GG02, GG20, GG21, GP16, GQ01, GV66, GV67 and their unsupported
reference/union implications GV60, GV61, GV65. Consult the generated matrix for
the exact current rationale and closure; this list is not a new release selection.
In particular, richer Rust schema construction does not implement the full GQL
closed graph-type grammar, and facade reference carriers do not establish all
graph/table parameter language semantics.

The rule inventory remains `seeded_incomplete`; the claim harness is a seed, not
positive/negative semantic coverage for every selected runtime family. Applicable
Annex B decisions remain pending, including normalization, numeric assignment and
promotion, repeated assignment, source controls, and type-precedence rules. Some
inventory prose still refers to historical owners or lower APIs (for example
IW004's `SharedGraph` transaction mechanism); it must not be read as the stable
facade recipe. Source review and executed tests must resolve those entries before
a complete claim. No evidence or claim state is promoted by this note.

**Owner question:** the target and the completed finish-plan subset are not yet
the same claim boundary. Justin must decide any proposed release-scope change;
otherwise the selected functional/evidence gaps need completion in their owning
lanes. This note does not mark F06-PR01's “every agreed functional slice complete”
acceptance as met, or authorize moving unresolved agreed behavior to post-GA.

## Public embedding contract

Applications depend on `selene-db`. The facade owns catalog paths, stable IDs,
sessions, requests, transactions, diagnostics and immutable result rows. Lower
engine construction types (`SharedGraph`, `Mutator`, physical rows, execution
contexts and WAL writers) are not facade exports. `Value` and normalized `Type`
are the query contract, not a durable serialization format. Intentionally exposed
scalar payload/descriptor types and stable node/edge IDs are not temporary bridges.

- `Database::builder().build()` is infallible and memory-only. `create`/`open` and
  their directory-handle forms are fallible native format-2 operations. There is
  no degraded/background-ready open; required indexes rebuild before return.
- A session retains database ownership and is `Send + 'static`, deliberately
  not `Sync`. Serialize requests on a session. Dropping its originating database
  handle does not invalidate it or release a retained writer lock.
- Catalog/graph/element IDs are stable within their documented store/graph scope,
  not globally unique addresses. `DatabaseId` and graph/node/edge references have
  process-local provenance, cannot be serialized as durable handles, and do not
  rebind after reopen or same-path replacement. Reissue references after reopen.
- Use `execute_request` for the immutable request context and the complete
  primary/additional/nested diagnostic bundle on failure as well as success.
  `execute` is a convenience `Result` adapter. No data, warnings, omitted catalog
  results, regular rows and failures are distinct outcomes. Match typed errors
  and GQLSTATUS, not English message text.
- Serializable single-writer publication and immutable readers are retained.
  Catalog/data mixing and read-only writes reject; failed transactions cannot
  commit earlier staged writes. Session controls have their own controlled state.
- In-memory `MutationIndeterminate` means publication happened. Durable outcomes
  separately report canceled, uncertain or synchronized-but-unacknowledged
  termination, phase and live visibility. `40003` is not a retry instruction;
  reconcile authoritative state before retrying a non-idempotent operation.
- Resource caps reject work rather than promising disk spill or unlimited paths,
  intermediate state or constructed values. `5GQL1` reports program limits where
  applicable; malformed parameters and unsupported features have their own
  categories. Existing bounded-path/batch failure tests are the contract, not a
  promise to convert every host allocation failure into a recoverable error.
- VECTOR/JSON values, vector/text retrieval, indexes, constraints and `selene.*` /
  `algo.*` procedures are disclosed native facilities, not additional standard
  grammar or a loadable third-party extension ABI. Correct scans and independent
  test oracles remain intentional alternatives, not temporary production bridges.

## Unicode and platform assumptions

The profile selects UTF-8 Unicode scalar source values (IV001); the catalog name
profile pins Unicode data 17.0.0, XID start/continue plus underscore (IE003), NFC
decoded names without case folding (IW023), and rejects private-use characters
(IA020). String comparison uses stored Unicode-scalar lexicographic order without
padding (ID022/IA015), not locale collation or visual-confusable detection. These
choices do not close the still-pending profile-wide normalization rule IA003.

Managed filesystem mode supports native Linux and macOS. Windows and other
platforms return typed unsupported-platform errors for this mode. Locks, atomic
publication and directory/file synchronization require a compatible filesystem
and storage stack; local process-crash tests are not power-loss proof. Verification
captures an on-disk view, not acknowledgment, write permission, physical durability
or future freshness. See [durable commit](durable-commit.md),
[checkpoint/reopen](checkpoint-reopen.md), and [verification](recovery-verification.md).

Format 1 is rejected without decoding. There is no 1.x reader, compatibility shim
or migration support. Create a fresh format-2 database from application-owned
source data and recreate schema/index registrations; this is a rebuild, not a
supported migration procedure. Alpha data has no cross-alpha compatibility promise.

## Executable examples and checks

[Catalog smoke](roadmap/examples/facade_smoke.rs) and
[release examples](roadmap/examples/facade_release.rs) are included directly by
`crates/selene-db/tests/release_examples.rs`. They exercise catalog creation, mixed
edges and an owned path, Rust schema constraints, vector/text/JSON retrieval,
negative diagnostics/no publication, and transaction/checkpoint/reopen with fresh
handle provenance. No lower engine constructor is used.

Focused commands (commands are not pass records):

```sh
cargo nextest run -p selene-db --test release_examples --locked
cargo nextest run -p selene-db-profile -p selene-db-testing --locked
cargo nextest run -p selene-db -p selene-db-gql --locked --all-features
cargo test -p selene-db --locked --all-features --doc
cargo run --locked -p selene-db-profile --bin selene-profile -- --check
cargo run --locked -p selene-db-testing --bin selene-conformance -- docs --check --root .
python3 -B scripts/v2_baseline.test.py
scripts/run-benches.sh --profile quick --bench read_write_guard --bench facade_read_write
```

Run both claim requests on an unchanged clean revision with
`scripts/check-conformance-claim.sh <full-sha> iso_aligned` and `selected_profile`.
The latter must remain denied while blockers exist. The wrapper binds source
identity before and after execution; direct local runner results over an edited
worktree are development evidence, not a clean exact-head release authorization.
Retain actual results in the implementation/review handoff, not fabricated
checked-in pass manifests. Run workspace gates and separate doctests as required.
Balanced guard rows are comparisons with the existing workload, not a new
optimization effort. Full native artifact/crash/fuzz qualification remains F06-PR02.
