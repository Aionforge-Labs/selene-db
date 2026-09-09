# Mixed-edge orientation contract

This is an original paraphrase of ISO/IEC 39075:2024 §16.7 SR14,
§22.3 GR3f2C–D, and §4.11.2, not a reproduction of the standard. It is the
F01-PR04 fixture contract for the current executor and the later batch/path
engines. It does not establish a formal conformance claim.

For a traversal from the left node to the right node, **R** means a directed
edge whose source is left; **L** means a directed edge whose destination is
left; **U** means an intrinsically undirected edge connecting the two nodes.

| Full pattern | Abbreviated | Eligible orientations |
|---|---|---|
| `-[]->` | `->` | R |
| `<-[]-` | `<-` | L |
| `~[]~` | `~` | U |
| `<~[]~` | `<~` | L or U |
| `~[]~>` | `~>` | U or R |
| `<-[]->` | `<->` | L or R |
| `-[]-` | `-` | L or U or R |

A directed self-loop satisfies both L and R but yields one binding for that
input row and edge identity. An undirected self-loop satisfies only U and has
one incidence at its node. Parallel identities remain separate. Deduplication
never crosses input bindings. Reversing the construction order of an undirected
edge changes neither its eligibility nor its semantic endpoints: canonical
storage order is **not** a source/destination assignment.

`selene-testing/src/mixed_orientation.rs` contains hand-worked expected edge
offsets for all seven rows, independent of the production matching helper.
Fixtures include forward/backward directed edges, two reverse-created parallel
undirected edges, both loop kinds, a parallel directed edge, and an isolated
node. Runtime tests compare adjacency and selective-index results by stable
identity, not physical row index.

## Predicates, insertion, and path values

Under §19.8, `e IS DIRECTED` yields unknown for null, true for a directed edge,
and false for an undirected edge. Under §19.10, either null operand makes
`n IS SOURCE/DESTINATION OF e` unknown; a non-null undirected edge makes it false
regardless of canonical endpoint order. `IS NOT` preserves unknown. Declared
operand errors retain analyzer status `42002`; forged runtime operands retain
the existing typed data exception, not a new clause-specific status.

§13.2 and §16.5 INSERT admit the three full forms `<-[]-`, `-[]->`, and `~[]~`.
Left-directed insertion swaps the semantic endpoints. Undirected insertion uses
the ordinary mixed-edge mutation funnel and accepts either closed endpoint type
order. Union and abbreviated INSERT forms are rejected. There is no new
grammar dialect, mutation side channel, or durable byte format.

An actual `PATH[...]` step carries `Undirected` only for an intrinsic undirected
edge. Submitted path parameters must agree with intrinsic directionality and
connectivity; `Undirected` is not a wildcard for a directed edge. Directed loops
accept either actual directed traversal orientation. Default match mode and
repeat/path-mode limits remain unchanged.

## Profile provenance

§16.7 CR11–13 distinguish G043 (complete full forms beyond left/right/any),
G044 (basic abbreviations), and G045 (complete abbreviations). AST syntax-form
provenance preserves this distinction through formatting and flagging. The
basic full Any form does not itself stamp GH02 as though it were pure U.
Explicit undirected forms and their L/U or U/R unions stamp GH02. The corrected
entries are `implemented_unclaimed`; unrelated feature selection and formal
conformance closure are not promoted.

## Algorithm interpretation

Projections expose intrinsic undirected edges in outgoing and incoming views at
both endpoints with the **same EdgeId**, retaining parallel identities, weights,
filters, stable neighbor order, scope, and generation invalidation. `edge_count`
counts logical edges once, including loops. Directed algorithms interpret an
undirected connection as reciprocal traversal arcs: pathfinding can traverse it
both ways, SCC joins its endpoints, PageRank sees reciprocal links, and
topological sorting can report the induced cycle. No canonical endpoint is a
privileged source.

Algorithms that ignore direction use logical incidence: community votes and
weights deduplicate only the same EdgeId across the two views; distinct parallel
edges still contribute. Self-loops contribute once per node. Louvain normalizes
by half the incidence-weight sum so its degree-sum invariant remains consistent.
Simple-neighbor algorithms (such as triangle counting) retain their existing
neighbor deduplication. Algorithms operate only on immutable projections.

## Executable evidence

- `selene-gql/tests/exec_mixed_orientation.rs`: spelling/provenance table,
  indexed/adjacency stable-ID oracle, loops, duplicate rows, questioned/repeat,
  PATH construction, closed mixed INSERT, and native projection GQL consumer.
- `selene-gql/tests/exec_mixed_predicates.rs`: independent null/NOT/endpoint
  truth table and forged-runtime operand status checks.
- `selene-db/tests/mixed_orientation.rs`: actual public facade INSERT, all
  orientations and predicates, property filters, rollback, and path preflight.
- `selene-algorithms/tests/mixed_projection.rs`: intrinsic incidence, logical
  counts, filtering/weights, generation change, pathfinding, WCC, and SCC.
- `BENCHMARKS.md`: controlled one-hop runtime measurement, separately from the
  F01-PR03 storage-incidence benchmark.

The legacy multi-step executor remains owned for deletion by F05-PR04. This
change introduces no compatibility alias or temporary directed-only bridge.
