# Analyzed scalar expression indexes (F05-PR06)

`Catalog::create_expression_index(owner, name, label, source, kind)` creates a
graph-owned node index using the existing typed scalar key engine. This is a
Selene implementation facility, not ISO GQL index grammar. `IM_INDEX_DDL`
identifies the extension; no ISO feature claim is increased.

For issue #1097's retrieval-by-key workload:

```rust,ignore
db.catalog().create_expression_index(
    &graph_path,
    &PathSegment::regular("kind")?,
    "Doc",
    "json_get_path_text(n.body, 'format', 'kind')",
    ScalarIndexKind::String,
)?;
```

The ordinary query remains:

```gql
MATCH (d:Doc)
WHERE json_get_path_text(d.body, 'format', 'kind') = 'jsonl'
RETURN d.id
```

The declaration uses the ordinary parser and immutable semantic/type tree with
one binding, `n`. A bounded structural program, not source text or a callback,
is persisted. Supported operations are `lower`, `upper`, `json_get_path_scalar`
and explicit `json_get_path_text`, including composition (at most 16 operations).
Bare properties use the existing property-index facility. Unknown functions,
parameters, time/randomness, procedures, traversal, lists/records, subqueries and
other operations are rejected. There is no arbitrary expression engine or GIN
containment index.

## JSON and comparison semantics

Paths contain 1–64 constant string keys or signed integer array positions.
Negative indexes count from the end. Variadic selectors and constant JSON-array
documents produced by `json`, `json_parse`, or `CAST(... AS JSON)` canonicalize
to the same typed selectors. Keys are decoded by the existing parsers; dots,
quotes and escapes are key characters, not JSONPath syntax.

Missing properties, GQL null, absent paths, and a selected JSON null produce GQL
null in these scalar functions. `json_has_path` and the JSON-valued retrieval
functions remain available when present-null versus missing must be distinguished.
Wrong selector types for an object/array are errors. Traversing through a scalar
has the existing absent-path behavior. Scalar extraction preserves boolean,
integer/unsigned-integer/finite-float and string values; selected arrays/objects
are errors. Only the explicitly requested text function performs its existing
canonical-text conversion. Index registration never stringifies arbitrary values
to fit a key kind. Binary string collation does not normalize Unicode.

An equality access path is selected only for a structurally equivalent analyzed
program, matching literal/key kind and complete snapshot-local coverage. The
initial conservative planner rule covers a single node scan with one predicate;
siblings, joins, other expression forms and uncertain proofs retain scans.
The original predicate always remains residual. Any evaluation failure or key
kind drift makes the entire expression index scan-only, including on cached-plan
execution, so index omission cannot hide a query error or a matching numeric form.
An index can become usable again when the offending data is removed or corrected.

## Lifecycle and storage

Catalog identity, owner, descriptor revision, generated profile identity and
expression semantics version identify the declaration. Format-2 index configuration
tag 5 encodes the bounded program and key kind; previous tags are unchanged.
The profile hash changes with the extension description under the existing exact
profile-admission policy. This is not a new format version or a migration promise.

Creation builds before facade publication. All dependent expression indexes follow
node creation, replacement of source values, property/label removal and deletion
inside the unpublished mutation snapshot. Rollback discards the derived changes.
Reopen eagerly rebuilds from logical declarations and primary values; compaction
rebuilds against the new private row layout. Graph replacement receives a fresh
owner and does not carry old expression metadata. `drop_declaration` removes an
expression index atomically with dependency RESTRICT. Other index/constraint
activation and removal rules are unchanged.

## Evidence and limitations

`tests/expression_indexes.rs` covers indexed/scan rows and errors, null/missing,
numeric representations, Unicode, negative/document selectors, target rejection,
dependent-key mutation/rollback, WAL/checkpoint reopen and graph replacement.
The private facade regression proves a selective query visits exactly one
candidate among 128 (budget zero fails, one passes), checks EXPLAIN selection and
non-equivalent/sibling-predicate fallback, and exercises cached-plan drift recovery.
The empty-array negative-index fixture also corrects an eager-subtraction panic
in the pre-existing JSON scan oracle.

See `BENCHMARKS.md` for the registered `scalar_expression` benchmark and absolute
query, maintenance, rebuild and command-level memory measurements. Nonselective
lookups are not promised to be faster. Correct scans are a permanent execution
alternative; there is no temporary alternate JSON indexing engine to delete.
