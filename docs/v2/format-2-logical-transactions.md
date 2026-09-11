# Format-2 logical transactions (F02-PR03)

This document specifies the pure codec and isolated replay model, **not facade
open/reopen**. [F02-PR04](durable-commit.md) connects private append/sync/publication/acknowledgment; PR05 owns
checkpoint, runtime reconstruction and durable facade open; PR06 owns lifecycle;
PR07 owns the long recovery campaign. GT03 remains unsupported. Codec tests with
multiple graph payloads do not enable multigraph GQL transactions.

## Ownership and authoritative inventory

| Owner/input | Format-2 representation |
|---|---|
| Core `StoredValue` | Explicit semantic tags below; no `Value` enum serde/rkyv |
| `CatalogSnapshot::logical_changes_from`, `CatalogLogicalRecords` | Dependency-ordered Created/Replaced/Dropped descriptors and all nine allocation domains |
| Facade `DatabaseDraft` | Pure `logical_transaction` preparation retaining every statement's changes; invoked before private durable append with bounded semantic replay preflight |
| Named `graph_types` | Stable catalog type ID plus complete named logical definition, or explicit removal |
| Every touched graph | Graph ID, before/after generation, next node/edge IDs, resulting logical schema, actual backing index identities, ordered data operations |
| Graph schema/defaults | Named node/edge declarations, named endpoints, recursive property types/defaults, nullability, validation mode, immutability and uniqueness |
| Property/composite/edge/vector/text indexes | Catalog target/configuration/name/state and explicit retained backing IDs; **no index contents** |
| Constraints | Exact declaring type, element/property target, kind, dependencies and optional backing ID |
| Native registrations | Symbolic procedure signatures/defaults/effects; inactive candidate-state and projection configuration; **no callable code or membership cache** |
| Node/edge changes | Creation/update/deletion, property/label removal, type truncation, reset, intrinsic edge direction and stable canonical endpoints |

Graph-local schema events are deliberately normalized into the **complete final
logical definition and catalog registration metadata**, not replayed as legacy
implicit graph-type IDs. This includes add/alter/drop of current node/edge types
and all current index kinds. The graph producer refuses unbound index registrations
rather than silently omitting them. Catalog-only changes and graph changes share
one envelope; a descriptor marker cannot substitute for a named type body or graph
payload. Generation changes may cover several statements; the frame sequence is
the separate, gap-free atomic-unit sequence.

The legacy `SchemaChange` GraphCreated/GraphDropped/GraphTypeCreated/GraphTypeDropped
arms are **not** format-2 lifecycle records: current facade lifecycle uses catalog
descriptors and named type/graph payloads. Legacy v1 node/edge definitions,
`RecordTypeAdded`, bare untyped LIST adapters and positional `RecordTypeId` values
are not admitted by this producer. Unsupported union/reference schema adapters
fail, including any lossy conversion detected by re-encoding the normalized
logical definition. Current named open records retain names, not record intern IDs.
Reserved expression indexes and unsupported constraint/native activation remain
unsupported; their absence is not a heuristic skip of an authoritative tag.

Binding-table **descriptor markers** can be encoded as catalog metadata; this does
not add binding-table execution/storage or permit query table handles as values.
Query references, paths, candidates, DatabaseId, runtime contexts, intern IDs and
opaque Extended values cannot cross `StoredValue`, including recursively. UUID
values remain ordinary data, not a durable-reference or StoreId value protocol.
Legacy HLC/origin/principal WAL headers and the separate audit format are not the
catalog/graph authority represented here; catalog creation-principal metadata is
retained. No replication/audit-format cutover is implied.

## Fixed frame, version 2.0

All integers below use fixed-width **little-endian** bytes. No native struct layout
or Rust enum discriminant is serialized. The only compression IDs are RAW = 0 and
Zstd = 1. Every other version, codec, flag, reserved bit or authoritative tag fails.
There are no skippable extension records.

| Offset | Length | Field |
|---:|---:|---|
| 0 | 8 | `SLTXN2\0\0` |
| 8 | 2 | major = 2 |
| 10 | 2 | minor = 0 |
| 12 | 1 | codec ID |
| 13 | 3 | flags/reserved, all zero |
| 16 | 8 | encoded body length |
| 24 | 8 | expanded body length |
| 32 | 8 | exact expected nonzero transaction sequence |
| 40 | 16 | StoreId UUID-v4 bytes |
| 56 | 8 | nonzero store epoch |
| 64 | 32 | trusted segment/selected-manifest lineage anchor |
| 96 | 32 | prior complete record digest or trusted initial anchor |
| 128 | 32 | BLAKE3 of bytes `[0,128)` |
| 160 | encoded length | encoded body |
| following body | 8 | `SLTXEND2` |
| following end marker | 32 | BLAKE3 of **all preceding frame bytes**, including the header digest, encoded body and end marker |

Fixed overhead is **200 bytes**. Header validation, checked lengths, exact context
comparison and complete-frame availability precede payload allocation. Complete
encoded integrity precedes decompression. RAW requires equal encoded/expanded
lengths. The final digest is the next record's expected `previous` value.

`Context` is a **caller-supplied trusted expectation**, never derived from the
frame being checked. The future stream owner must first select/validate the
manifest's compatibility identity and authoritative segment boundary, provide
its lineage anchor, and advance sequence/digest only after successful validation.
Constructing StoreId/StoreEpoch or a Context grants no filesystem authority.

BLAKE3 here is an **unkeyed corruption/integrity check**, not authentication, a MAC,
encryption or proof against intentional rollback. A rewritten internally consistent
history cannot be disproved by these hashes alone.

### Compression decision

Automatic mode attempts the existing Zstd **level 1 at 4096 raw encoded semantic
bytes or more**, but selects RAW when compression is not strictly smaller. Explicit
RAW remains available. No new algorithm, dictionary service or dependency is used.
The encoder sets a maximum window log of 23. The reader independently calls
`window_log_max(23)`, stops at `single_frame()`, and checks `finish()` has no
unconsumed bytes. Skippable frames and dictionary-bearing frame headers are refused;
concatenated frames, trailing data and wrong expanded lengths fail.

The independent history limit is **8 MiB**, separate from the output limit. This
uses the pinned zstd 0.13.3 APIs; its default concatenating decoder and the legacy
output-only decompression helper are not used. Bytes and CPU tradeoffs are measured
under the `logical_wal` section of the repository's `BENCHMARKS.md`.

### Tail classification

The caller supplies `Boundary::UnsealedEnd`, `SealedEnd` or `Interior`. Missing
fixed-header/body/trailer bytes return `Incomplete { needed }` **only** at an
explicitly unsealed final boundary. The same missing bytes in a sealed/interior
location are corruption. A complete checksum failure is always corruption, even
at EOF. Incomplete is a classification, not proof of a torn write or permission
to truncate. There is no file repair, salvage or newest-file selection policy here.

`logical_frame::decode` returns a consumed length for a stream owner.
`ReplayState::apply_frame` instead requires exactly one frame and rejects trailing
bytes. An incomplete suffix returns no candidate; bad context, corruption and
semantic errors cannot mutate the previous state.

## Semantic body version 1

Notation: `text`/`blob` is u32 byte length followed by exact bytes; text is UTF-8.
`count` is u32; `bool` is exactly 0 or 1. Optional fields use a bool followed by the
field when present. IDs and generations are u64. These lengths never authorize an
allocation before the cumulative budget checks described below.

Body order:

1. u32 body version = 1.
2. Catalog before generation, after generation, nine u64 high waters, change count
   and catalog changes.
3. Named type count; each entry is type ID, definition presence, optional definition.
4. Graph count and graph payloads.
5. End of input: trailing bytes are invalid.

Named type IDs, graph IDs and backing IDs are strictly increasing without duplicates.
Catalog changes are dependency ordered, with no repeated changed descriptor identity.
Data operations retain mutation order: repeated updates are valid, reused creations
are not. Node creation must precede an edge that references it. Catalog creation
must follow its owner/dependencies; drops cannot leave a dependency behind.

### Catalog

Domain order and ID/payload tags: 1 Catalog, 2 Directory, 3 Schema, 4 Graph,
5 GraphType, 6 BindingTable, 7 Procedure/native, 8 Index, 9 Constraint.
The high-water array uses this order and records the **last allocated** identity,
including deleted identities; zero means that domain has allocated none.

Catalog operation tags: 1 Created(descriptor), 2 Replaced(previous revision,
descriptor), 3 Dropped(typed ID, last revision). A typed ID is its domain tag plus
u64 value. Descriptor fields are: typed ID; name form (0 synthetic, 1 regular,
2 delimited) and display text; parent; revision; creation revision; optional
creation principal text; payload tag and payload. Parent 0 has no ID; parents
1..5 have a u64 Catalog/Directory/Schema/Graph/GraphType ID. Kind/ID/payload and
parent agreement are checked by the catalog owner. NFC comparison names are
reconstructed with its pinned naming rules, not serialized intern identities.

Catalog/root/schema/graph-type/binding-table payloads are markers. Graph payload
adds an optional graph-type ID. Named type definitions and graph state appear in
the other body sections. Replacements preserve creation metadata and parent,
require the exact old revision and advance the descriptor revision. Missing
identities, duplicate names/IDs, wrong-kind dependencies, stale generations,
cycles and backwards/insufficient high water fail catalog validation.

Declaration metadata fields: state (0 Inactive, 1 Building, 2 Ready, 3 Failed),
profile ID text, profile hash text, semantics u32, dependency count and sorted
(typed ID, exact revision) dependencies. Current profile compatibility is checked.
Dependency order on the wire is canonical even if a native input Vec was not.

An index stores metadata, target (element 1 node/2 edge, label text, property texts
in declaration order) and configuration: 1 property kinds, 2 vector, 3 text.
Property kinds are 1 Bool, 2 I64, 3 U64, 4 I128, 5 U128, 6 Decimal, 7 F32, 8 F64,
9 String, 10 Date, 11 LocalDateTime, 12 ZonedDateTime, 13 LocalTime, 14 ZonedTime,
15 Duration, 16 UUID. Vector configuration is kind, u32 dimension, optional HNSW
(u32 neighbors, u32 construction effort), optional IVF (u32 centroids). Vector
kinds are 1 Flat; 2..4 HNSW squared-Euclidean/cosine/negative-inner-product; 5..7
the same IVF metrics; 8 TurboQuant cosine. Owner limits and kind/config agreement
are reapplied. A constraint stores metadata, target, declaring-type text, kind
(1 Unique, 2 CompositeUnique, 3 Key), optional backing index ID.

Native binding tags: 1 procedure, 2 candidate state, 3 projection. Procedure
fields are binding texts, description, since-version, parameters, outputs, effect
(1 graph read, 2 schema write, 3 maintenance). A field is name, type, nullable,
description. A parameter then adds default (0 absent, 1 NULL, 2 bool, 3 i64,
4 text) and optional default-documentation text. Native types: 0 Any, 1
AnyProperty, 2 Boolean, 3 Integer, 4 Int64, 5 Uint64, 6 Float, 7 Float64, 8 String,
9 Vector, 10 JSON, 11 NodeRef, 12 EdgeRef, 13 GraphRef, 14 OpenRecord, 15 List
followed by its element type. These reference **signature descriptions** do not
permit stored reference values. Candidate-state fields are optional required
label, required outgoing/incoming labels, excluded outgoing/incoming labels.
Projection fields are node labels, edge labels, optional weight property.

### Graphs and definitions

A graph payload stores ID, optional previous generation (absent only for new
graphs), resulting generation, **next** node ID, **next** edge ID, optional complete
resulting definition, backing-index ID count/list, and ordered data operations.
The graph owner validates before returning a candidate. Published/deleted ID
water cannot regress; each creation advances its domain floor with checked
arithmetic. No external ID is reconstructed from a physical row.

A catalog graph binding to a named type is an independent constraint, not merely
a reference-existence check. Its stable type ID must select a same-schema catalog
type whose body name matches the descriptor's display spelling. The instance must
retain a definition with that name; empty or foreign names do not bypass binding.
The graph owner's entity-state and change validators check the resulting graph
against the **named definition as well as its instance definition**, including
required labels/properties, endpoints, UNIQUE and immutable-property operations.
Instance definitions need not be byte-identical: graph-local declarations may
differ while the graph's data still conforms to both. The existing owner behavior
for create/update/delete in one atomic unit remains: final deleted entities do not
have to remain live merely to validate their earlier operations.

Every changed graph-type catalog descriptor requires an explicit matching type
payload, even for a revision with an unchanged body. A cached old body is not a
substitute. Changed type bodies revalidate **all** referencing candidate graphs,
including retained graphs absent from the transaction's graph-delta list. Changed
graph bindings and touched graphs also revalidate; only an unchanged immutable
graph/type/binding triple may reuse its prior proof. This adds no public named-type
evolution, GT03 or activation behavior: facade replacement remains fresh-identity
and RESTRICT under its existing lifecycle owner.

Data operation tags and fields:

| Tag | Operation | Following fields |
|---:|---|---|
| 1 | NodeCreated | ID, labels, properties |
| 2 | NodeUpdated | ID, added labels, removed labels, property diff |
| 3 | NodeDeleted | ID |
| 4 | EdgeCreated | ID, direction (1 directed/2 undirected), label, first/source ID, second/target ID, properties |
| 5 | EdgeUpdated | ID, property diff |
| 6 | EdgeDeleted | ID |
| 7 | NodePropertyRemoved | ID, property |
| 8 | EdgePropertyRemoved | ID, property |
| 9 | NodeLabelRemoved | ID, label |
| 10 | NodesOfTypeTruncated | label (including incident edges) |
| 11 | EdgesOfTypeTruncated | label |
| 12 | GraphReset | no fields; clears data and resulting schema is explicit |

Labels and property keys are strictly increasing exact database strings. A property
bag is count plus (name, value) pairs, not Standard/Compact storage layout. A diff
is set pairs then removed names; sets are canonical and disjoint. Undirected
endpoints are in canonical stable-ID order; this is not orientation.

A definition stores its name, named node definitions in declaration order, then
named edge definitions in declaration order. Nodes store labels, properties,
validation mode (0 Strict/1 Warn). Edges store label, two named endpoints,
properties, mode. Endpoint tags: 0 Any, 1 node-type name, 2 sorted distinct names
with count at least two. Positional runtime endpoint indexes are reconstructed
only inside the graph owner.

Property fields: name; value-type descriptor; nullable; optional stored default;
immutable; unique; optional inline record fields. A value-type descriptor stores
optional predefined type; optional decimal bounds (u32 precision/scale); optional
character and byte bounds (u64 minimum/maximum each); optional list element
descriptor; not-null; cardinality (0 exactly-one/1 zero-or-one). Unsupported/lossy
adapter combinations fail semantic admission. Predefined tags 1..30 are explicitly
assigned in `logical/tags.rs`, independently of the Rust declaration order.

Inline record fields use 0 Open or 1 Closed plus named field count/list. Each field
stores name, field-type descriptor and required bool. Field-type tags are 0 scalar
property kind, 1 bounded character string, 2 bounded decimal, 3 bounded bytes,
4 list plus element descriptor, 5 record plus inline fields, 6 not-null wrapper.
The explicit scalar assignments are in `logical/tags.rs`; query-only families
are rejected at this boundary. The owning graph's narrower list/record limits,
default checks and structural/UNIQUE validators still apply.

### Stored values

| Tag | Representation |
|---:|---|
| 0 | NULL, no payload |
| 1 | bool |
| 2 / 3 | i64 two's-complement / u64, eight bytes |
| 4 / 5 | i128 two's-complement / u128, sixteen bytes |
| 6 / 7 | binary64 / binary32 IEEE bits, including signed zero and NaN payloads |
| 8 | decimal: 16-byte maintained `Decimal::serialize` primitive, LE flags/lo/mid/hi; reserved flag bits and scale above 28 fail |
| 9 / 10 | database UTF-8 string / bytes |
| 11 | list count and recursively tagged values |
| 12 | named record count and (field text, value) pairs; field order retained, duplicates forbidden |
| 13 | sixteen UUID bytes |
| 14 | u32 component count, finite binary32 bits; nonempty, at most 65535 components |
| 15 | canonical compact JSON text, sorted object keys; validated by the native JSON owner |
| 16 / 17 / 18 | canonical Date / LocalTime / LocalDateTime text |
| 19 / 20 | canonical ZonedTime / ZonedDateTime text with offset/zone, including the date of the current ZonedTime carrier |
| 21 | ten i64 duration components: years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds |

Temporal text must round-trip to the same canonical spelling and is capped at
512 bytes before parsing. Duration components retain their unit representation
instead of normalizing months/days or subsecond fields; mixed signs and out-of-range
components fail Jiff validation. Defaults use this same stored semantic encoding.
Replay preserves the written property bag; it does not rematerialize omitted defaults
and accidentally undo an explicit later removal.

## Resource and publication boundaries

| Resource | Default ceiling |
|---|---:|
| Encoded body / expanded body (independently) | 256 MiB each |
| Cumulative decoded allocation charge | 256 MiB |
| Cumulative entries/value/component work | 1,048,576 |
| Metadata entries/descriptor work | 4096 |
| Stored-value/wire-descriptor depth | 256 including root |
| Zstd history | 8 MiB |

`Limits` can tighten, not raise, the ceilings. Collection counts charge at least
256 bytes per entry; large resident types charge at least **four times their
actual `size_of`** before reserve, covering materialization/validation copies.
This matters for the large legacy **in-memory** Change carrier even though its
serde layout is never encoded. Strings charge eight times bytes, values charge
128 bytes plus their containers, vector components charge 16 bytes each, JSON
charges 136 times canonical text bytes and text-byte work. JSON encoding uses a
bounded writer before building its intermediate text. Stored-value decoding uses
an explicit container stack, not 256 large recursive parser frames.
Recursive schema descriptors also charge at least 128 bytes or four resident
descriptor sizes before allocating a box, independently of their depth/work count.

The isolated apply preflights retained catalog fields through the same codec in
counting mode, charges retained maps, physical scan work including dead slots,
and recursive value clones before copying a touched graph. Named type and untouched
graph bodies are shared. A large retained catalog/work set can therefore return
`Limit` even when the new record is small; this is not an unbounded large-database
recovery claim. The metadata ceiling also bounds quadratic owner-validation work.
Accounting units are conservative enforced charges, **not allocator callbacks**.
Encoded/expanded buffers and decoder history have their own bounds; the 256 MiB
allocation charge is not a claim that entire process RSS is below 256 MiB.
Primary-column materialization additionally charges actual row widths rounded to
the backend's 2048-entry chunk allocation, with a four-copy allowance and fixed
graph overhead. Optional property/vector/text accelerators are not even constructed
by isolated apply; the graph owner checks logical backing identity/target agreement
and the same constraint-binding rules without granting runtime eligibility.
Named-type materializations are cached per validation pass and charged before
conversion, including retained definitions not decoded in the current frame.
Additional referencing-graph validation charges row/type lookup work, label/property
traversal and canonical constraint-key storage through the shared budget before
calling the owner validators. Limits remain cumulative across retained graphs.

All body decoding finishes before apply. Catalog dependency/revision/high-water
checks, named type coverage, graph referential/order/high-water checks, structural
assignment/default/UNIQUE validation and declaration/backing agreement finish
before a **new isolated** ReplayState returns. Failure drops the candidate and
leaves the original catalog and every graph unchanged. No global/session state,
StoreDirectory, WAL writer, provider callback or publication lock is touched.

ReplayState intentionally exposes only primary-value/count inspection. Required
constraint semantics are checked, but optional index contents and native callable
bindings are not activated from Ready metadata. PR05 must rebuild/admit runtime
state before serving queries. The temporary `RecoveryState` bridge uses only
isolated data replay/materialization and existing semantic adapters; it calls no
legacy byte decoder. Its extraction/replacement is deletion-owned by F02-PR08.

Legacy SLDB WAL, SLSN snapshots, SLMF manifests and SLAU audit formats retain their
existing versions and paths. New entry points cannot dispatch to them. Empty
StoreControl still rejects unmanaged WAL artifacts. No format-1 reader, migration,
file truncation, arbitrary salvage or filesystem publication is added.

## Evidence

Independent fixtures document offsets/tags for a 209-byte raw frame, a 104-byte
empty body, a 272-byte catalog+graph transaction, catalog descriptors and selected
scalar/temporal/record/vector values. Tests also cover whole-frame bit flips,
every partial cut, invalid final fields/operations across two graphs, schema and
UNIQUE failures, stale/wrong-kind/missing references, registration replacement/drop,
deleted-ID water, and cumulative allocation/depth rejection. Bounded proptests
exercise nested values and all partial prefixes (128 cases).

A deliberate local removal of the Zstd history guard made the independently built
16 MiB-history frame test fail by accepting `x`; restoring the guard made it pass.
An initial recursive value decoder overflowed the test stack at hostile depth;
the explicit-stack replacement passes rejection and the valid 256-level boundary.
Short native fuzz runs target both the new frame/body and stored-value decoders;
raw and repaired-integrity passes reach payload parsing. These are smoke evidence,
not the PR07 long corruption/crash campaign or power-loss qualification.

Final local smoke on 2026-09-10: `cargo +nightly fuzz build` compiled all seven
registered targets. `decode_logical_value` ran 4,284,055 executions with
`-max_total_time=60 -max_len=8192`; `decode_logical` ran 1,262,433 with
`-max_total_time=60 -max_len=16384`. Both used `-verbosity=0 -print_final_stats=1`,
completed without a failure, and reported sanitizer-process peak RSS of 459/339 MiB.
Those RSS values include the fuzz engine/corpus/quarantine, not a per-record
production allocation measurement. Earlier 60-second smoke passes also completed;
the final passes followed the allocation-accounting changes.

The pre-delivery named-type followup reproduced six failing rejection cases out
of an initial eight-test selection, then passed the expanded 50-test focused codec
selection. It adds explicit registered-type facade production, required-property/
label and immutable-operation checks, retained-graph type-revision validation,
name/ID/body pairing, and cumulative retained-validation budget regressions.
The enhanced `decode_logical` target also mutates a named-type revision over two
retained graphs with repaired frame integrity: 985,791 executions in 61 seconds
(`-max_total_time=60 -max_len=16384 -print_final_stats=1`), no failure, 355 MiB
sanitizer-process peak RSS. All seven fuzz targets compiled. No wire schema or
encode/decode-only benchmark path changed in that followup.
