# Semantic and durability notes for implementers

These notes paraphrase the supplied ISO/IEC 39075:2024 and the retained Selene decisions. Clause numbers are the primary anchors; printed pages differ from the uploaded PDF viewer by 16 pages in these sections. The PDF itself is not redistributed. This is an engineering companion, not a replacement for reading the exact rule when implementing it. [S12]

## Binding tables, values and types

A binding table may contain duplicates and can be ordered or unordered (§4.3.6). A unit table has one empty record; an empty table has no records. Physical batches must preserve that difference, especially when a mutation or expression begins from unit input. An omitted result, an empty regular result and a null value are also distinct (§4.8.4).

Null comparison and null grouping are not the same operation. A Boolean Unknown is the null value, ordinary comparison with null has its required three-valued result, and two nulls are not distinct in the relevant duplicate/group context (§§4.4.3–4.4.5, 4.16.2, 22.12–22.15). Do not use Rust Eq/Hash/Ord as a universal semantic service. Numeric/collation normalization must be compatible with the specific equality/grouping/index operation.

Open and dynamic types are explicit descriptors, not absent metadata (§§4.12–4.17). A value representation does not alone determine every site's declared type. The facade's temporary lower Value/GqlType re-exports must be resolved before durable encoding assumes they are permanent public or on-disk types.

Optimization must preserve the required observable semantics. Where the standard explicitly permits implementation-dependent evaluation or diagnostic choices, test the permitted outcome set rather than demanding byte-identical diagnostic text or evaluating errors the language does not require. Do not use this freedom to suppress a required failure or silently lose rows.

## Identity has several different lifetimes

| Identity | Required interpretation |
|---|---|
| Durable StoreId/epoch | Identifies the persisted store and relevant lifecycle; survives ordinary reopen |
| Facade DatabaseId/handle provenance | Identifies the live ownership context; do not serialize and reuse it blindly |
| GraphId/NodeId/EdgeId | Stable semantic identities within their documented scope; not storage positions |
| Physical layout/workspace tokens | Retained, snapshot/workspace-local provenance for row-backed candidates |
| Batch selection position | Internal position in a binding batch, not a graph identity or graph storage row |

GQL object/reference rules (§§4.3.1, 4.4.4) do not require exposing Selene's physical coordinates. CandidateSet may validate graph, generation, physical layout, workspace binding and live ID pairing. A fresh independent graph with equal numeric IDs/generation is not the same candidate domain. Old immutable snapshots must remain usable with their own candidates while newer layouts reject them.

Generic binding of stable IDs is liveness-only. Missing/non-vector properties are a later vector-scoring concern. Do not conflate that helper with a schema/index membership filter.

## Mixed edges and path semantics

A graph may be a mixed multigraph; an edge has one identity, its endpoints and intrinsic directionality (§4.3.5). An undirected edge is not a pair of directed edges. Path orientation is the way a step uses an edge, not an invented stored source for an undirected relationship.

WALK is the default path mode; an omitted graph **match mode** is an implementation-defined choice (§§4.11.7–4.11.9). They are not interchangeable settings. SIMPLE and TRAIL are not a hierarchy on mixed graphs. For a single undirected A—B edge, A,B,A satisfies the SIMPLE first/last-repeat allowance but repeats the edge and violates TRAIL (printed p58 / PDF p74).

A questioned path primary and a {0,1}-quantified primary differ in exposed binding degree: conditional singleton versus group (§4.11.3, printed p55 / PDF p71). An optimizer cannot normalize away that distinction merely because both may traverse zero or one step.

Selective prefixes operate by endpoint partition (§4.11.8, §22.4). The shortest path for A→B may be length 1 while the shortest for C→D is length 3; both are selected in their own partitions. Evaluate the required path-local conditions before final selection. A shortest topological route rejected by a condition does not prove no longer qualifying route exists.

The permanent oracle must model bindings/multiplicity/history, not merely endpoints. The included [selector reference](examples/path_selection_reference.py) deliberately covers only the selection/mode predicates over supplied bindings. It is **not** a complete graph-pattern interpreter, a proof of the optimized automata engine or a substitute for the licensed rules.

## Transactions and durable outcomes

The standard’s default model is serializable; relaxations are implementation-defined extensions (§4.6). Selene's selected design remains one writer/publication authority with immutable reader snapshots, not a new MVCC or weaker-isolation project.

The transaction spans its successful statements; a failed executed procedure causes an attempt to roll back the current transaction (§4.6.2). Do not preserve earlier staged writes after a required transaction rollback by implementing only per-statement undo. Catalog/data mixing follows the selected GP18 policy. Session changes and graph mutations are different side-effect classes.

The durable order is **validate/prepare → append → synchronize → publish → acknowledge**. Prepare any work that could make publication fail before the irreversible durable boundary where possible. Flushing a userspace buffer, calling a file sync operation, atomically renaming a file and durably publishing the new directory entry are distinct actions.

| Failure seam | Required reasoning/test |
|---|---|
| Before authoritative append | No new live/replayed mutation; definite cancellation is possible |
| Partial append or uncertain sync | The tail may or may not become durable; fence the writer and classify honestly |
| Truncation after failure | Definite cancellation requires establishing durable rollback/control state, not merely successful set_len |
| Cleanup synchronization fails | Remain indeterminate; do not report a proven rollback |
| WAL synchronized, publication/ack interrupted | Recovery must preserve the whole durable transaction; never call it definitely canceled |
| Derived provider fails | Cannot undo a committed authoritative transaction; required constraint state blocks unsafe writes/open |

The in-memory MutationIndeterminate case documents a mutation already visible.
F02-PR04 preserves it and adds a separate [durable outcome contract](../durable-commit.md):
proven cancellation, uncertain durability, and synchronized-but-unacknowledged state
carry phase and live visibility independently. Named-type admission is aligned in
both modes; unused instance-local declarations remain valid without relaxing the
shared named constraint.

GQL commit failures under §8.4 and connection-related unknown statuses are not interchangeable labels. Choose diagnostic codes from the applicable situation and retain useful causes/phase context (§23). Cancellation must not interrupt an irreversible commit and then pretend the caller’s canceled task proves the transaction canceled. No automatic retry is safe solely because a request returned an indeterminate error.

## Recovery and derived indexes

One authoritative WAL covers the whole transaction, including catalog and all affected graphs. Vector/text/JSON/algorithm accelerators are derived. Active unique/key constraints, however, require a complete valid backing index before writes; “rebuildable” does not mean safe to ignore enforcement until later.

Select manifest/snapshot/WAL lineage under a retained epoch/retention lease from selection through consumption. A stored path is not a lease. Directory anchoring prevents pathname redirection; retention prevents chosen artifacts being removed. Both are needed.

An allowed incomplete final unsealed frame is not the same as an interior/sealed corruption or an arbitrary bad checksum. A tail is discardable only when the authority protocol proves the discarded bytes cannot contain acknowledged work. Recovery is non-destructive by default. Corrupt authoritative data must not be silently skipped to manufacture a healthy database.

A process-kill test leaves OS caches intact. Report it as process-crash evidence and state native filesystem/device assumptions; do not label it power-loss proof.

## Constraints and expression indexes

Composite constraints use typed tuples and their declared equality/collation/null policy. Component order and boundaries matter. Do not concatenate displayed values into a synthetic unique string. Evaluate transaction deltas against final staged state so permitted key swaps are not rejected because one row happened to be visited first.

Expression targets must be deterministic, pure, scalar and same-element under the selected Selene policy. JSON scalar paths reuse that target service; GIN-style containment is deferred. The planner may use an index only with equivalent semantics and complete snapshot-valid state. A correct scan is preferable to an incomplete index answer.

## Standards claim boundary

Minimum conformance (§24.2, printed p513 / PDF p529) includes every non-optional syntax/semantic rule and the required graph/type/Unicode conditions. Selected optional features add their implication closure (§24.3); they do not permit inventing a smaller minimum. Formal implementation claims require the in-scope implementation-defined elements/actions (§24.5), and the flagger provisions apply (§24.6).

The profile registry/harness provides infrastructure. Runtime evidence and its coverage determine a claim. A capability list, grammar acceptance, test count or internally generated expected result is not independent conformance evidence. Use the retained ISO-aligned wording only with exact gaps, and never call it certification.
