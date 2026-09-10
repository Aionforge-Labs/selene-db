# Catalog lifecycle and selected-session contract

M02-PR02 defines catalog identity and immutable metadata. M02-PR03 adds the
in-memory lifecycle service, M02-PR04 routes GQL lifecycle statements through
that service, M02-PR05 makes catalog-selected sessions the facade execution
root, M03-PR04 Part 1 puts every facade mutation behind one in-memory
publication authority, and Part 2 adds detached session transaction state and
demarcation exclusively over that authority. Catalog and graph changes are not
durable yet; M09 owns their persisted representation and recovery.

## Hierarchy and namespaces

The catalog has one synthetic root directory. Its internal empty name cannot be
constructed as a user name. The root contains schemas and has no child
directories. Within each schema, graphs, graph types, binding tables,
procedures, indexes, and constraints share one canonical-name dictionary. The
same canonical spelling conflicts across object kinds in one schema; different
schemas may reuse it.

The initial `DatabaseState` contains only the catalog descriptor and synthetic
root. It contains no schema, graph, graph type, runtime graph, or hidden working
graph. Kind-local high-water marks start at zero, so the first published user
schema, graph, and graph type each receive ID 1. Published IDs are never reused.

Each descriptor kind has a separate nonzero ID type. Catalog `GraphId` and
`GraphTypeId` are catalog identities, not lower storage IDs. `CatalogName`
retains display spelling for diagnostics and NFC canonical spelling for
dictionary identity. Names are case-sensitive and receive no compatibility or
case folding.

`CatalogPayload` is storage-neutral. A graph payload may refer to a catalog
`GraphTypeId`; a graph-type payload is only a kind marker. Validated runtime
graph-type definitions remain private facade state keyed by catalog
`GraphTypeId`.

## Logical paths

Facade paths are typed absolute logical paths, not filesystem paths:

```text
CatalogPath  /selene
SchemaPath   /selene/memory
ObjectPath   /selene/memory/episodes
```

Each segment is constructed through regular or delimited `CatalogName`
validation. Resolution compares canonical NFC names case-sensitively. Listings
sort by canonical name and use stable ID as the tie-break key.

## One outer publication

`DatabaseInner` has one `ArcSwap<DatabaseState>` and one mutation coordinator
that encapsulates the former lifecycle-writer mutex.
`DatabaseState` contains everything that must agree at a catalog generation:

- the immutable catalog snapshot;
- runtime graph instances keyed by catalog `GraphId`;
- validated graph-type definitions keyed by catalog `GraphTypeId`; and
- kind-local ID high-water marks.

A lifetime-free `DatabaseDraft` stores only detached catalog, graph-type,
high-water, graph-snapshot/delta, and pinned identity/generation metadata. It never owns
an outer `DatabaseState`, `GraphInstance`, `SharedGraph`, transaction, guard,
committer, or provider. The reservation is instead a borrowed, non-`Send`
capability whose invariant lifetime is tied to the stack-local writer guard;
the higher-ranked closure API prevents retaining or returning it. Both draft
construction and publication require that capability. An explicit transaction
retains the completed detached draft, never the capability; commit reacquires a
fresh reservation and validates the exact pinned outer allocation. Direct
lifecycle commands and selected-session writes therefore use the same
coordinator and outer store.

Catalog staging and graph `PreparedGraphCommit` staging do not publish. The
authority reloads and revalidates the current outer state immediately before
publication, composes the next catalog/runtime maps from that authoritative
state, and constructs any replacement CORE-only `SharedGraph` at the last
possible point after every pre-store failpoint. No fallible or cancelable phase
follows successful replacement construction before the one outer store. That
store is the sole facade visibility cut-line, so descriptor state, graph
snapshots, runtime maps, and high-water marks are never published separately.

Pre-store validation failure or cancellation retains the exact prior outer
allocation, generation, runtime maps, procedure state, graph IDs, and
high-water marks. Cancellation is publicly reported as `MutationCanceled` /
`5GQL2`. A post-store acknowledgement failure is publicly reported as
`MutationIndeterminate` / `40003`: the complete new state is already visible
even though acknowledgement was uncertain. Callers must inspect current state
and must not blindly retry an indeterminate mutation. These outcomes are
in-memory and durability-independent until M09. A same-path recreation or
replacement still receives a fresh stable ID.

`CatalogReadSnapshot` loads the outer state in O(1). Its facade summaries do not
expose lower graph instances, schema definitions, row positions, mutators,
providers, or persistence types. Retaining a read snapshot retains the complete
publication it observed, but an old runtime instance is not reachable by new
requests after drop or replacement.

## Rust lifecycle API

`Database::catalog()` returns a shared `Catalog` handle. Callers can create and
drop schemas, graphs, and graph types, and resolve or list descriptors from a
read snapshot.

`CreatePolicy::Strict` reports duplicates. `IfNotExists` is a no-op only when
the requested kind already exists at the same canonical path; another kind in
the shared namespace remains a wrong-kind error. `OrReplace` is supported for
graphs and graph types. It applies the same RESTRICT admission as drop and
publishes the removed and created identities together. Schema replacement is
not an ISO form and is rejected.

`DropPolicy::Strict` reports a missing object. `IfExists` returns a no-op only
for a missing leaf; a missing parent remains an invalid reference. All drops are
RESTRICT. The service rejects a graph with live nodes or edges, a nonempty
schema, and a graph type referenced by a graph.

Graph-type binding is same-schema. The public definition slice supports
property-free named node types with one implied singleton key label. Properties,
edges and endpoints, explicit key labels, aliases, and COPY OF, LIKE, inline, or
external graph-type sources remain unsupported.

## Selected facade sessions

`Database::session(&ObjectPath) -> Result<Session>` resolves a graph in the
current catalog publication with anonymous local defaults.
`Database::session_with_options` additionally accepts an authorization ID,
principal provider, and authorization policy. A facade `Session` stores the
database plus a `SessionContext`; it does not store or expose a lower graph
handle. The type is owned, lifetime-free, `Send`, and intentionally not `Sync`.
Concurrent use of one session is outside the current contract.

`Session::context()` exposes immutable dependency data plus controlled session
state. It copies:

- optional authorization ID and resolved principal;
- optional home schema and graph descriptors;
- required current schema and graph descriptors;
- the creation catalog generation and stable reference dependencies;
- generated profile identity and UTC displacement; and
- initially empty parameters, a vacant request/transaction slot, and active
  termination state.

The parameter count and request-slot inspection are dynamic. While a request is
active, `request_slot()` returns `RequestSlotState::Active` and
`current_request()` returns the associated `Arc<RequestContext>`. The context
holds only the merged typed parameter snapshot and request timestamp. Neither
the session nor request context retains a `CatalogReadSnapshot`, graph instance,
lifecycle lease, lower transaction context, binding registry, or execution
  stack. `transaction()` returns a cloned lower-type-free descriptor with the
  monotonic nonzero ID, access mode, precise lifecycle state, pinned publication
  and generation summaries, statement count, and staged graph-change count.
  `transaction_slot()` reports vacant, active, failed, committing, rolled back,
  committed, or indeterminate. Selected SET/RESET controls persist in this
  facade state, and SESSION CLOSE terminates it after releasing transaction state.

An explicit authorization ID is resolved by `PrincipalProvider`. Provider
`None` is an error for that explicit ID. Optional principal home paths resolve
in the same immutable catalog snapshot as the current graph and must identify a
coherent schema/graph pair. `AuthorizationPolicy` then receives only copied
facade descriptors. Provider resolution and policy evaluation run outside the
catalog lifecycle writer and graph request leases. The local defaults perform
no principal lookup for an anonymous session and allow session creation; there
is no user store, role catalog, credential handling, network call, or privilege
language.

Before implicit execution or transaction start, one current catalog snapshot checks every copied home/current
reference by stable ID and descriptor creation metadata. A dropped or replaced
dependency returns `StaleSessionReference`; a same-path recreation cannot
rebind the context. Execution then uses the current graph ID rather than path
rebinding:

1. load the outer state and find the runtime instance by stable ID;
2. acquire that instance's lifecycle read lease;
3. reload the outer state and confirm the same registered instance; and
4. execute before releasing the lease.

Drop and replacement use catalog writer, target lifecycle write lease, then
graph state lock order. They recheck registration after acquiring the lifecycle
lease. A request that already holds a read lease completes before drop; drop
then observes its writes when applying RESTRICT. A request that loses the race
fails as `StaleSessionReference`. Dropping and recreating the same path never
makes an old session refer to the replacement.

An idle session does not pin a lifecycle lease. A successful graph drop or
replacement removes the old descriptor and runtime entry in the same outer
publication, then clears procedure-derived graph state. Failed publication does
not clear that state.

## GQL catalog dispatch

A selected session associates one immutable request context before validating
copied catalog references or parsing. After parse and analysis, request
preflight checks every source parameter use and every supplied graph-backed
value before planning or execution. Implicit read-only plans execute against
the live pinned graph under its request lease without the global mutation
reservation. Explicit read-only plans execute against the transaction's
detached selected-graph snapshot. Database-catalog commands return to the
facade before execution; private lifecycle functions stage them into the same
`DatabaseDraft` used by Rust `Catalog` calls. The old selected-GQL adapter that
invoked public auto-committing `Catalog` methods no longer exists.

Data-modifying and engine catalog-modifying plans retain their one owned plan
and request input. Execution uses a request-local, CORE-only scratch
`SharedGraph` cloned from the transaction's current detached snapshot.
`WriteTxn::prepare_unpublished` validates and freezes its next graph without a
seal sequence, committer submission, graph-local store, WAL/provider work, live
schema-epoch bump, or fanout. The facade builds a CORE-only replacement
`GraphInstance`, composes the complete `DatabaseState`, and publishes once.
This CORE-only assumption is a Part 1 boundary; non-CORE/durable provider
preservation belongs to M09.

Parameter names omit `$`, preserve exact Unicode spelling, and are
case-sensitive. `GeneralParameter` carries both its declared `GqlType` and
`Value`. Request bindings shadow the session snapshot without modifying it.
Preflight rejects unbound uses, source/request declaration mismatches, stale or
foreign graph, node, edge, and path references, and nested invalid references.
Each request owns one binding-table registry and ID allocator. A `TableRef`
resolves only through that authority, including when nested in a list or record;
tombstone, unknown, stale, and cross-request IDs fail preflight as invalid
references rather than aliasing another request's table.

Bare `START TRANSACTION`, `COMMIT`, and `ROLLBACK` are classified from the same
prepared plan and handled by the facade state machine; they are never delegated
to a lower session with a live graph transaction. `START TRANSACTION` defaults
to read-write. Rust callers may also start `TransactionAccessMode::ReadOnly`.
Read-only transactions pin catalog and selected-graph snapshots, never publish,
and may commit after a concurrent writer. Read-write commit reacquires the
reservation; an outer-state conflict discards staged work and returns `40000`.
Successful graph/catalog predecessors are visible to successor requests in the
same transaction and remain invisible to other sessions until the one store.

The initial profile does not support GP18 mixing. Reads establish no mutation
mode; the first data- or catalog-modifying statement fixes the mode, and the
opposite class fails with `25G02`. A write in read-only mode fails with `25G03`.
Statement failure discards detached work and leaves an explicit transaction
failed: later non-controls and `COMMIT` return `25N02` (commit also completes
rollback), while `ROLLBACK` succeeds. GT03 multi-graph transactions remain
unsupported.

F03-PR01 adds bounded lexical read scopes: `AT /schema` at a query-procedure
head and `USE graph` in focused queries, including same-graph linear nesting.
Query references resolve against the innermost working schema, not necessarily
the persistent session schema. An implicit request executes on the graph chosen
by that resolution; an unused ambient default is not treated as a data access.
An explicit transaction retains its pinned selected-graph snapshot. A second
graph identity fails with `25G04` through ordinary statement-abort handling.
Lexical scopes never execute `SESSION SET` or rewrite persistent selection.
The [F03-PR01 boundary](roadmap/Milestone-F03-PR-01.md) lists unsupported forms;
this is not complete GQ01/GP16 or GT03 support.

Selected maintenance procedures remain rejected before lower live-maintenance
execution with `42N01`. This is an explicit deferred detached-maintenance
boundary, not a Part 2 bridge: maintenance does not auto-start, and an attempt
inside an active transaction fails that transaction. Direct lower-engine
maintenance behavior is unchanged. M03-PR05 routes selected session controls
through persistent facade state while preserving the independent lower-engine
session path. A
bare lower executor session rejects every database-catalog command
with the implementation-defined status `5GQL0`; it does not reinterpret `DROP
GRAPH` as a storage reset.

ISO absolute references do not spell the facade catalog:

- schema `/x` maps to `SchemaPath /selene/x`;
- graph or graph type `/x/g` maps to `ObjectPath /selene/x/g`;
- relative graph or graph type `g` maps to the selected session graph's schema;
- one-segment absolute object references and deeper directory shapes are
  invalid (`42002`).

The parser carries source forms and decoded spellings. `PathSegment`
construction remains the validation choke point for both GQL and Rust paths.
Lifecycle resolution occurs under the writer mutex from typed paths, not
pre-resolved IDs.

Dropping the selected graph through GQL is valid when it is empty. An implicit
command publishes the drop immediately; an explicit command stages it until
commit. After publication the session becomes stale. A nonempty selected graph
is still rejected by RESTRICT.

## Outcomes and statuses

Successful database-catalog statements return
`ExecutionOutcome::OmittedResult`. `Session::execute_request` retains that
summary in `RequestOutcome::Succeeded`; validation, compilation, dispatch, and
runtime errors use `RequestOutcome::Failed`. The legacy `Session::execute`
adapter preserves its existing `Result<ExecutionOutcome>` signature. Rust and
GQL lifecycle calls share the same structured facade errors and GQLSTATUS
mapping.

Regular results retain immutable rows plus an analyzer-declared typed
descriptor; omitted results have no fabricated descriptor. Every success and
failure carries one deterministic diagnostic bundle: a primary status, all
additional statuses in request order, and every ordered nested cause without
truncation. Internally, the same request owns the root/child execution stack,
status collector, and table registry. The facade exposes none of those runtime
contexts or physical table types.

| Case | Outcome or error | GQLSTATUS |
|---|---|---|
| successful create or drop | omitted result | `00001` |
| successful create-or-replace | omitted result, one publication | `00001` |
| duplicate or missing conditional no-op | omitted result, no publication | `00001` |
| `DROP GRAPH IF EXISTS` missing leaf | omitted result | `01G03` |
| strict duplicate | `CatalogObjectAlreadyExists` | `42N10` |
| missing object or parent, wrong kind, invalid path shape | structured reference error | `42002` |
| invalid catalog name | `InvalidCatalogName` | `42001` |
| RESTRICT dependency | `CatalogRestrictViolation` | `G1000` |
| canceled before the outer store | `MutationCanceled` | `5GQL2` |
| published but acknowledgement uncertain | `MutationIndeterminate`; complete state is visible and blind retry is unsafe | `40003` |
| duplicate `START TRANSACTION` | active transaction | `25G01` |
| mixed catalog/data modification | failed transaction, no publication | `25G02` |
| write in read-only transaction | failed transaction, no publication | `25G03` |
| request or commit after statement failure | failed transaction; commit rolls back | `25N02` |
| `COMMIT`/`ROLLBACK` without active work | invalid termination | `2D000` |
| optimistic pinned-base conflict | rolled back, winner preserved | `40000` |
| transaction ID exhaustion | program limit | `5GQL1` |
| stale home or current session identity | `StaleSessionReference` | none |
| invalid authorization/principal ID or home declaration | structured session error | none |
| missing/failing provider or denying/failing policy | structured authorization error | none |
| unsupported catalog source or selected maintenance | `FeatureNotSupported` | `42N01` |

No-op outcomes retain the same outer allocation and generation. Unsupported
clauses are rejected before a command exists and cannot mutate catalog state.

## Durability and later owners

Catalog lifecycle changes currently have no WAL, snapshot encoding, recovery,
or crash contract. Existing lower `Mutator::factory_reset` remains available
for engine and recovery use, but it is not a GQL database-catalog route.

- M03-PR03 owns execution context/stack and binding-table parameter support.
- M03-PR04 merged both delivery parts: the sole in-memory staging/publication
  authority plus explicit transaction demarcation and multi-request visibility.
- M03-PR05 owns persistent facade session controls and its temporary private
  generation-safe plan dependency stamp.
- M04-PR01 finalizes public identity/generation contracts; M05-PR02 owns full
  `AT SCHEMA` / `USE GRAPH` lexical scope and semantic-site resolution.
- M05 owns replacing the temporary facade re-export of lower `Value` and
  `GqlType` semantic types.
- Later milestones may broaden catalog object families without changing
  selected graph identity semantics.
- M09 owns persisted descriptor encoding, WAL and snapshot records, and
  recovery.
