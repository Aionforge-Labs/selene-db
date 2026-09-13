# Native text, JSON and maintained candidates (F04-PR08)

These procedures use the [catalog-resolved native CALL boundary](native-calls.md),
not a second dispatcher. Names, signatures, defaults, output metadata and the
69-procedure inventory are unchanged. Graph-read calls emit typed physical
batches; selected mutations still use the facade transaction authority. There
is no GQL grammar addition, external search service or loadable provider API.

## Text contract

`selene.create_text_index` creates a graph-owned `IndexConfiguration::Text`
declaration through the mutation funnel; `selene.drop_text_index` removes it.
`selene.text_index_stats` reports the existing registration/statistics surface.
Only the declaration persists. Postings, lengths and frequencies rebuild from
primary string values during runtime reconstruction, including reopen.

The fixed tokenizer uses Unicode alphanumeric runs and lowercase conversion;
there is no configurable stemmer, stopword list or analyzer language. Empty or
tokenless documents and non-string/missing values do not enter the population.
BM25 uses k1=1.2 and b=0.75, deduplicated query terms, descending score and
ascending stable node ID for ties, including ties at the cutoff. Zero k and
tokenless queries return no hits. Negative k is invalid.

The derived index records an in-memory tokenizer/BM25 contract version. Changing
tokenization, eligibility or scoring requires bumping that version. Mismatched
indexes are declined by graph lookup and checked standalone searches reject
them, even at zero k. Rebuild replaces the old version; maintenance cannot
make mixed old/new statistics query-eligible. This is not a durable analyzer
selection field or a promise that scores never change across engine versions.

| Surface | Result | Scoring population |
|---|---|---|
| `text_search_nodes` | `node_id: NODE, score: FLOAT64` | All eligible documents in selected graph's label/property; maintained index if usable, otherwise exact primary scan |
| `text_score_nodes` | same | Same full-index population, but only explicit candidates can be returned |
| `text_score_nodes_batch` | `query_index: UINT64, node_id: NODE, score: FLOAT64` | Same population per query, with per-query node lists |
| `text_score_candidate_state` / `text_score_candidate_state_nodes` | node and score | Same population, restricted by the named state and optional set algebra |
| `text_score_candidate_state_expanded_batch` | query index, node and score | Same population, per-query graph expansion composed with named state |

Candidate scorers require a usable registered index; they do not build postings
inside a read call. Indexed node/edge filters on global search restrict eligible
results **before top-k**, not corpus statistics. The unindexed filtered path
still scans the entire label population to calculate those statistics. An
ordinary subsequent GQL WHERE remains a separate post-search filter.

All text candidate scoring now passes through a snapshot-owned typed-candidate
entry point. Maintained results validate graph/generation/layout/workspace before
conversion for existing set algebra; foreign empty sets are not valid shortcuts.

## JSON contract

Global `json_contains_nodes`, `json_path_exists_nodes`, `json_path_contains_nodes`
and `json_path_value_nodes`, plus their explicit-candidate companions, remain
exact scans. They use primary JSON values and graph-owned candidate binding.
There is no expression/containment index and no claim of index acceleration.
JSON expression-index planning remains **F05-PR06**.

Containment is recursive subset containment, not textual matching. JSON number,
string and boolean values remain distinct. An absent path does not match path
existence; a present JSON null does. `json_path_value_nodes` returns selected
null as `JSON`, not GQL NULL. Results are in ascending stable ID order, capped by k.
Paths are bounded selector arrays (1–64 string object keys or integer array
indexes, with negative indexes from the end), not JSONPath. Invalid documents,
empty paths and malformed selectors report GQLSTATUS 22G03. Existing scalar
JSON functions retain their separate missing/null/error semantics.

## Maintained-provider ownership and recovery

`Catalog::declare` now admits graph-owned Ready `NativeBinding::CandidateState`
rules after validation and complete private rebuild. Other caller-asserted Ready
native/index/constraint declarations remain rejected. The declaration's display
name is the lookup name (use a delimited name to pin spelling). Replacement and
`drop_declaration` retain shared-dependency RESTRICT checks. Inactive/building/
failed declarations remain inspectable and do not install runtime state; asking
to score an unavailable state returns 22G03 instead of silently searching all nodes.

For an existing facade database and graph `path`, a policy-neutral rule can be
declared entirely with facade types (the integration example supplies setup):

```rust
use selene_db::*;

db.catalog().declare(
    &path,
    &PathSegment::delimited("current")?,
    DeclarationDefinition::Native(NativeDeclaration {
        metadata: DeclarationMetadata::new(DeclarationState::Ready),
        binding: NativeBinding::CandidateState(NativeCandidateState {
            required_label: Some("Memory".into()),
            require_outgoing: vec![],
            require_incoming: vec![],
            exclude_outgoing: vec!["SUPERSEDED_BY".into()],
            exclude_incoming: vec![],
        }),
    }),
    CreatePolicy::Strict,
)?;
db.session(&path)?.execute(
    "CALL selene.text_score_candidate_state('Memory', 'body', 'memory', 'current', 10) YIELD node_id, score"
)?;
```

This call requires a text index; the rule itself does not create one or prescribe
an application retrieval policy.

Each detached runtime constructs its own first-party provider from the bound
catalog rules and primary values. It never borrows the prior publication's
mutable provider. Creation, staged writes, rollback, graph replacement and
reopen therefore cannot overwrite state held by another graph or transaction.
Native lower-graph commits retain existing observer fanout and generation checks.
Duplicate direct/catalog CSET registration is rejected, not installed twice.

F02-PR08 removed the historical `recover_guarded`/provider attachment callbacks.
This slice does not resurrect those adapters: ownership is private before rebuild,
and no external recover callback can see or re-enter a partially attached runtime.
Fallible construction precedes publication; failure or canceled outer publication
drops the private instance. There is no new callback under the facade's excluding
writer/lifecycle locks. Existing lower observer reentry tests remain applicable to
post-publication callbacks, not evidence that the removed recovery API still exists.

Ready declarations are never silently skipped on rebuild failure. Catalog
dependency admission rejects missing, stale or non-Ready providers required by an
active constraint. This does not introduce a candidate-backed constraint engine;
the existing unique enforcement stays authoritative and cannot be disabled by
declaring metadata. Provider members and text accelerators are not WAL voters.

## Evidence and costs

`native_text_json` is the deterministic facade memory-document example: identical
labels in separate graphs, independent BM25 arithmetic, filtered/global parity,
updates/removals/deletes, rollback, JSON scalar distinctions, malformed paths,
candidate replacement/drop, graph replacement, WAL-only and checkpoint reopen,
and read-only verification. Physical tests cover windows 1/2/3/7/1024 and typed
JSON, NODE, FLOAT64 and UINT64 outputs without a row suffix. Additional tests cover
contract-version rejection, foreign/old candidates, canceled outer attachment and
active-constraint dependency readiness.

See `BENCHMARKS.md` for absolute CPU-only measurements and estimated text-index
memory. The facade still reconstructs detached runtimes, including candidate
state, rather than incrementally sharing mutable providers across publications.
That correctness-first cost is not hidden as an indexed-query speedup. F04-PR09
removed the common row suffix; there is no text/JSON/provider-specific row bridge.
See [batch-only execution](batch-execution.md).
