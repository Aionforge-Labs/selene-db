---
plan_id: F03-PR01
milestone: F03
initial_status: proposed
---

# F03-PR01 — Introduce immutable semantic analysis through a working query slice

**Milestone:** [F03: Complete the semantic compiler](Milestone-F03.md)
**Dependencies:** [PLAN-01](PLAN-01.md)
**Carries forward:** M05-PR01, M05-PR02; see the [complete crosswalk](06-OLD-TO-NEW-MAP.md).
**Issue closure:** None; do not close another PR’s issue.
**Focused skills:** `rust-api-design`; `module-design`; `rust-test-design` from `yet-more-skills/skills/<name>/SKILL.md`.

## Outcome

Establish source AST → immutable semantic tree with real name/scope resolution, first through ordinary named-graph queries and then through the selected statement families.

## Start from what exists

Sessions, request contexts, parameters and generation-safe reuse already exist. The source/semantic separation is still pending. Inspect analyzer and AST module names in the current tree; this plan does not invent an existing semantic/ directory. Source: S09.

**Observed live entry points:** `crates/selene-gql/src`, `crates/selene-db/src/session.rs`, `crates/selene-db/src/request.rs`, `crates/selene-db/src/session_context.rs`

Paths are navigation, not a closed edit inventory. New modules are implementation choices. Keep required callers and their tests with the behavior they support. Source IDs resolve in [SOURCE-NOTES.md](SOURCE-NOTES.md).

## Implementation sequence

1. Find the live parser-to-analyzer-to-plan entry path and capture representative positive/negative behavior. Keep syntax spelling and spans in an immutable source tree; resolved objects belong in a separate semantic representation.
2. Deliver a real vertical slice for selected-graph MATCH/FILTER/RETURN and parameter resolution, with catalog-aware references and deterministic semantic snapshots. Do not merge only a directory of unused node enums.
3. Implement distinct namespaces for parameters, binding variables, graph variables and catalog objects. Resolve working schema/graph selection lexically, preserving the already-implemented persistent session controls.
4. Carry source origins through desugaring and record precise dependencies on catalog/profile/procedure generations. Data snapshot changes and schema changes are different invalidation causes.
5. Extend the same immutable resolver to the remaining supported statement families. The owner-authorized bounded ISO grammar admission below supersedes the original no-grammar-change restriction. A single explicit semantic-to-current-plan adapter may remain until F03-PR04; there is no hidden fallback to analysis of mutable syntax.

## Acceptance and concrete regression cases

- [ ] Request parameters override same-named session defaults as specified, while binding variables remain a distinct namespace.
- [ ] Nested ISO `USE <graph expression>` / `AT <schema reference>` scope does not leak into unrelated lexical scopes or permanently rewrite session state. Positive nesting uses one graph identity; selecting a second graph fails with `25G04` in implicit and explicit transactions.
- [ ] Duplicate declarations, unresolved names, wrong object kinds and out-of-scope references carry useful original spans.
- [ ] The same source and semantic environment produce stable snapshots independent of hash iteration order.
- [ ] Catalog object replacement invalidates dependent analysis, while a safe independent lookup does not rely on stale numeric identity.
- [ ] Parser-only tests require no database construction and analyzer output does not mutate the source AST.

## Validation and performance

Run existing parser/analyzer negative and positive corpora, new semantic snapshots and facade parameter/scope tests. Compare observable diagnostics, not only snapshot formatting. Add one mutation experiment in development to confirm that wrong namespace selection is caught; do not commit the mutation.

Measure parse versus analyze time and retained source/semantic memory separately. Avoid repeatedly cloning the source string into every annotated node.

Use the shared [validation guide](05-VALIDATION-AND-RELEASE.md) for runner mechanics and required PR/RC gates. These are planned checks, not reported passes.

## Keep out of this PR

No parser-generator replacement, new GQL spellings, full physical planner, public AST serialization promise or duplicate semantic authority in the facade.

## Bridge/deletion boundary

One semantic-to-current-plan adapter is allowed with deletion at F03-PR04. Future types/effects are completed in F03-PR02/03; unsupported incomplete families remain explicit failures, not guessed semantics.

### Owner-resolved scope corrections (F03-PR01)

- Admit the bounded existing ISO productions needed for the working-query slice:
  `USE <graph expression>` in focused read queries and `AT <schema reference>`
  at read-procedure heads. These are not `USE GRAPH` or `AT SCHEMA` wrappers.
- Preserve unsupported GT03: one graph per implicit or explicit transaction.
  Successful different-graph nesting is **not** required here. A second graph
  produces `25G04` through normal request/transaction failure handling. An unused
  ambient session default does not count as a query data access; an explicit
  transaction retains its existing pinned graph authority.
- The bounded executable forms are absolute/working-schema-relative catalog
  graph names, explicit `./name`, current-graph expressions, same-graph focused
  queries, single-linear-body braces, CALL bodies, and VALUE bodies. Absolute
  AT references resolve lexically and restore at sibling boundaries. Graph-valued
  bindings/parameters, focused mutations, multiple focused parts, and composed
  AT/nested bodies remain unsupported. GQ01's corrected name is “USE graph
  clause”; GQ01/GP16 have selected parser admission but remain complete-capability
  unsupported and unclaimed in the generated profile.
- `analyze/ast.rs` owns shared immutable source separately from the semantic
  scope/expression trees, resolved applications, and catalog dependencies.
  `plan/lowering.rs` and its children are the **single current-plan adapter**:
  they borrow unchanged syntax payloads required by the existing row plan and
  consume semantic identities/types/defaults. F03-PR04 deletes this mixed-payload
  adapter and its expression-shape lookup. No mutable-source analysis fallback
  is retained. Defensive corruption helpers exist only under private `cfg(test)`.

## Standards and reviewer focus

§4.7 working scopes; §4.10 variables/parameters; §§16.1–16.2; §17 references; §22.1 annotation.

**Independent review question:** Is the first slice actually exercised through the facade, and are namespaces and lexical scope independent of parser storage?

Use [Luna execution guidance](03-LUNA-EXECUTION.md) for material decisions and the compact handoff. A necessary adjacent caller or mechanical migration is not itself a reason to stop; an incompatible semantic, public or durable contract is.
