# selene-db benchmarks

_Last measured: 2026-06-01 on Apple M5 (10-core / 16 GiB / macOS 26.5 build 25F71 / rustc 1.95.0 / commit `3a864ac`)._

This file is the **north-star performance baseline** for selene-db: the medians
below are the reference point for performance-uplift work and the tripwire for
regressions. Every benchmark bin registered in `scripts/run-benches.sh` is
documented here — that parity is CI-enforced (fast-gate
`.github/scripts/check-benchmarks-doc.sh` + release-gate
`crates/selene-testing/tests/benchmarks_md_pin.rs`), so a bench can never ship
undocumented (the historical `expression_eval` orphan).

The suite is **criterion-only** (wall-clock medians on the dev box). There is no
iai-callgrind instruction-count layer — it needs valgrind, which never runs on
the macOS dev machine, so it was dropped rather than left as a perpetually-TBD
placeholder.

## Running benchmarks

`scripts/run-benches.sh` is the sanctioned entry point. Direct `cargo bench
--workspace` is **forbidden** (Cargo may dispatch bench binaries concurrently,
which corrupts wall-clock medians); the runner executes strictly one binary at a
time, guarded by a `pgrep` check and a serial run loop.

The runner is **flexibly scoped** so you never have to fire up a whole run to
check one thing:

```bash
run-benches.sh --list                          # enumerate registered benches + smoke subset
run-benches.sh --smoke                          # curated <~60s tripwire subset (profile quick)
run-benches.sh                                  # FULL run, every bench (the north-star sweep)
run-benches.sh --bench wal                      # one bench bin (scoped compile + run)
run-benches.sh --bench vector_graph_retrieval --compile-only  # compile tripwire, no Criterion run
run-benches.sh --crate selene-db-graph          # every bench in one package
run-benches.sh --bench wal --filter body_size   # one criterion group within a bin
run-benches.sh --bench graph_hub_delete --sample-size 50 --measurement-time 5   # A/B fidelity knobs
run-benches.sh --bench single_graph --filter graph_exact_vector_scan --vector-scales million
run-benches.sh --bench vector_index_rebuild --vector-scales 10000,50000
run-benches.sh --bench vector_index_rebuild --filter graph_vector_index_rebuild/ivf --vector-scales 100000
run-benches.sh --profile quick --bench vector_wgpu --filter core_vector_wgpu_prototype
SELENE_WGPU_STRESS_CASES=1 run-benches.sh --profile quick --bench vector_wgpu --filter q8x100000x1024
scripts/criterion-summary.sh core_vector_wgpu_prototype/cpu_rayon_score_topk/q8x100000x1024
SELENE_VECTOR_IVF_INSERT_DRIFT_BPS=100,500,1000 run-benches.sh --bench vector_ivf_insert_drift --vector-scales 10000
run-benches.sh --bench vector_index_rebuild --allocator system   # allocator A/B without mimalloc
run-benches.sh --crate selene-db-algorithms --dry-run   # preview resolved invocations, run nothing
```

### Profiles

The workload envelope is selected by `SELENE_BENCH_PROFILE` (set via `--profile`;
see `crates/selene-testing/src/bench_profiles.rs`):

| Profile | Scales | Sample / measurement | Use |
|---|---|---|---|
| `quick` | one 1k scale | 10 / 500 ms | fast spot-check / smoke |
| `full` (default) | 10k / 50k / 100k | 30 / 1500 ms | **publish-quality / this doc** |
| `stress` | adds 250k | 30 / 1500 ms | opt-in larger envelope |

Vector benches also accept an independent runner override with
`--vector-scales`. The flag exports both `SELENE_VECTOR_BENCH_SCALES` and
`SELENE_VECTOR_REBUILD_BENCH_SCALES`, so it covers exact/ANN query sweeps and
index rebuild sweeps without changing non-vector benches.

`vector_ivf_insert_drift` also accepts
`SELENE_VECTOR_IVF_INSERT_DRIFT_BPS` as a comma-separated basis-point sweep over
post-training novel inserts. The default remains `1000` (10%) so routine runs
keep the historical row count; use `100,500,1000` for the maintenance-policy
threshold sweep.

For `vector_index_rebuild`, Criterion's positional `--filter` is mirrored into
`SELENE_VECTOR_REBUILD_GROUP_FILTER` and
`SELENE_VECTOR_REBUILD_VARIANT_FILTER=ivf|hnsw` when the filter names a rebuild
group or ANN family. Those prefilters skip irrelevant fixture construction, so
focused large-scale IVF or HNSW runs do not pay preview costs for unrelated
groups or the other family.

| Vector scale selector | Scales | Use |
|---|---|---|
| `quick` | 1k | fast vector smoke |
| `full` | 10k / 50k / 100k | publish-quality vector sweep |
| `stress` | 1k / 10k / 50k / 100k / 250k | opt-in stress sweep |
| `large` | 250k / 1M | local large-scale validation |
| `million` | 1M | focused million-vector run |
| comma list | sorted positive integers | custom A/B scale envelope |

A few benches sweep an independent axis instead of the node-scale envelope (hub
degree, WAL entry-body packing, WAL sync policy, writer fan-in, correlated-row
count); those are profile-trimmed too so `quick` stays fast.

## Tracking regressions

The full sweep saves a named criterion baseline so later runs report a
percentage delta instead of a bare median:

```bash
run-benches.sh --profile full --save-baseline northstar     # record this doc's baseline
# … later, after an optimization or to check for a regression …
run-benches.sh --bench graph_hub_delete --baseline northstar # %-change vs northstar
```

`--save-baseline` and `--baseline` are mutually exclusive (one records, one
compares). criterion stores baselines under `target/criterion/` (gitignored, so
local to your machine); the committed baseline of record is the number tables in
this file. Refresh both together (see [Update protocol](#update-protocol)) on a
quiet machine — background load pollutes the medians.

## Hardware footprint

| Field | Value | Source |
|---|---|---|
| CPU | Apple M5 | `sysctl -n machdep.cpu.brand_string` |
| Cores | 10 physical / 10 logical | `sysctl -n hw.physicalcpu hw.logicalcpu` |
| Memory | 16.0 GiB | `sysctl -n hw.memsize` |
| OS | macOS 26.5 (build 25F71) | `sw_vers` |
| rustc | 1.95.0 (59807616e 2026-04-14) | `rustc --version` |
| Commit | `3a864ac` | `git rev-parse --short HEAD` |

All committed benchmark rows use `mimalloc` as the global allocator; the library
crates are allocator-agnostic. For allocator A/B work, run the same scoped bench
twice with `--allocator mimalloc` (the default) and `--allocator system`.
Criterion writes per-run artifacts under `target/criterion`. After a sanctioned
runner invocation, `scripts/criterion-summary.sh <criterion-id>` prints
tab-separated sample count, median, mean, standard deviation, and sample p95 in
milliseconds for quick variance checks.

## §0 selene-catalog

Bench bin: `catalog_descriptors`. It measures canonical-name lookup, typed-ID
lookup, and `Arc` snapshot clone at 100 and 1,000 schema-owned objects in the
quick profile. The full profile adds 10,000 objects.

M02-PR02 quick evidence was recorded on 2026-08-23 with Apple M5 (10 cores,
16 GiB), macOS 26.7 build 25G220, rustc 1.97.1, and the worktree based on
`7bd53af3f112c4a912e894075ccc9c1ab80494fd`:

```bash
scripts/run-benches.sh --profile quick --bench catalog_descriptors
```

Two consecutive quick runs produced the following median ranges. The profile's
10 samples are a smoke envelope, so these rows are recorded evidence rather
than regression thresholds.

| Bench | 100 objects | 1,000 objects | Notes |
|---|---:|---:|---|
| `catalog_descriptor/canonical_name_lookup` | 30.341–30.623 ns | 90.714–109.02 ns | `BTreeMap<CatalogName, CatalogObjectId>` lookup with a pre-canonicalized query. |
| `catalog_descriptor/snapshot/clone_arc` | 3.6660–4.1816 ns | 3.6604–3.8273 ns | Clone one immutable snapshot handle; descriptor count does not change the operation. |
| `catalog_descriptor/snapshot/read_by_id` | 4.7855–5.7107 ns | 9.7319–11.730 ns | `BTreeMap` point lookup keyed by typed descriptor ID. |

The bench also prints `CatalogSnapshot::memory_accounting`. This is a
reproducible structural lower bound, not process resident memory: it sums inline
descriptor/dictionary key-value sizes and owned string capacities, and excludes
declaration payload heap allocations, allocator metadata, `BTreeMap` node slack,
and `Arc` control blocks. The historical table below predates typed declarations;
it is not current total-memory accounting.

| Objects | Descriptors | Accounted bytes / descriptor | Dictionary entries | Accounted bytes / entry |
|---:|---:|---:|---:|---:|
| 100 | 103 | 197.40 B | 101 | 95.80 B |
| 1,000 | 1,003 | 197.94 B | 1,001 | 95.98 B |

### Catalog lifecycle facade

F02-PR02 adds `catalog_declaration/{lookup,clone_arc,draft_build}` to
`catalog_descriptors` and `catalog_declaration/outer_publication` to
`catalog_lifecycle`. These are additional groups in the existing registered
targets, not new benchmark binaries.

The following first table is the **pre-correction implementation**, before exact
physical binding and end-to-end eligibility were corrected. Current-worktree
measurements follow it. BTreeMap descriptor lookup is not runtime query overhead.

Measured 2026-09-10 on Apple M5 (10 cores, 16 GiB), native arm64 macOS 27.0
build 26A5425a, rustc 1.97.1, bench profile (opt-level 3, thin LTO, one codegen
unit), mimalloc, worktree based on `099a7129436abbe9b35fb9df209b544641bcd03f`:

```bash
scripts/run-benches.sh --bench catalog_descriptors --compile-only
scripts/run-benches.sh --bench catalog_lifecycle --compile-only
scripts/run-benches.sh --profile quick --bench catalog_descriptors --filter catalog_declaration
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_declaration/outer_publication
```

Runs were serialized, with 10 samples, 100 ms warmup, and a requested 500 ms
measurement window (Criterion extended the slowest rows). Intervals below are
Criterion's reported time confidence intervals, not min/max observed samples.
These are absolute characterization measurements, not a matched-baseline
speedup claim or a release regression threshold.

| Graph owners × declarations per owner | Owner/name lookup | Snapshot clone/drop | Detached catalog clone/validate/build/drop | Facade declaration create/publication |
|---|---:|---:|---:|---:|
| 100 × 4 | 17.031–17.221 ns | 3.2895–3.2967 ns | 131.37–131.82 µs | 303.05–317.31 µs |
| 100 × 16 | 23.990–24.234 ns | 3.2946–3.3034 ns | 545.59–550.37 µs | Not measured |
| 1,000 × 4 | 19.505–19.598 ns | 3.2945–3.3434 ns | 1.7608–1.7764 ms | Not measured |
| 1,000 × 16 | 34.496–34.777 ns | 3.3005–3.3699 ns | 7.1263–7.2542 ms | 14.392–14.866 ms |

Lower fixtures mix scalar, 384-dimensional flat-vector, text, and inactive
arity-one constraint declarations. Only logical configurations are present, not
accelerators. The draft row includes metadata cloning, complete validation,
replacement construction and destruction; it is not an outer database store.
Facade fixtures mix scalar/vector/text declarations across real empty graphs
and include the frozen native inventory. The facade row times creation of one
inactive projection declaration, including reservation, metadata validation,
derived owner binding, outer publication and acknowledgement. Input cloning,
initial fixtures, and the paired cleanup drop are outside the clock. Neither
row measures filesystem durability. Snapshot clones share one `Arc`; draft and
publication costs remain scale-dependent and are not claimed O(1).

The facade rows were rebuilt and remeasured after the final descriptor-revision
publication guard. An earlier intermediate-worktree run measured
277.21–283.89 µs and 11.781–12.256 ms respectively. Criterion reported increases
of approximately 10.7% and 21.8% against those saved intermediate samples. This
is disclosed validation overhead, not a comparison against the supplied base
revision; the correctness guard is retained. Larger catalog publication remains
a measured follow-up, not grounds to weaken admission or claim a speedup.

#### F02-PR02 pre-delivery correction: current measurements

The same native host, toolchain, allocator, codegen, sample count, and clock
boundaries above were used. Rebuilt, serialized reruns of the two existing
catalog groups produced these current confidence intervals:

| Graphs × declarations | Descriptor lookup | Snapshot clone/drop | Detached catalog build/drop | Facade declaration publication |
|---|---:|---:|---:|---:|
| 100 × 4 | 16.817–17.379 ns | 3.3506–3.5540 ns | 132.25–140.24 µs | 284.87–289.02 µs |
| 100 × 16 | 25.210–26.189 ns | 3.3201–3.5222 ns | 550.90–588.50 µs | Not measured |
| 1,000 × 4 | 19.633–20.873 ns | 3.3348–3.5450 ns | 1.7740–1.8942 ms | Not measured |
| 1,000 × 16 | 34.286–36.352 ns | 3.3590–3.5607 ns | 6.9060–7.3854 ms | 11.946–12.247 ms |

The added `catalog_runtime_binding` group in `catalog_lifecycle` measures the
actual property/vector accessor and typed candidate-probe paths. Each fixture
has **1,024 nodes and one physical index**; vector fixtures use 16-dimensional
flat indexes. Bound/unbound variants share identical physical data and index
allocations. Owner declaration counts are 1, 16, and 256: one ready registration
named `zz_query`, sorted after the remaining inactive, non-backed declarations.
All setup and key/value construction is outside the clock. Accessor rows include
returned Arc clone/drop; candidate rows include the one-hit candidate result's
construction/drop. These are access/probe costs, not ANN scoring or whole-query
latency. No concurrent build/benchmark was launched during measurement.

```bash
scripts/run-benches.sh --bench catalog_lifecycle --compile-only
# Before correcting the runtime implementation:
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_runtime_binding --save-baseline f02-pr02-pre-correction
# After correction, against that saved same-fixture baseline:
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_runtime_binding --baseline f02-pr02-pre-correction
scripts/run-benches.sh --bench catalog_descriptors --compile-only
scripts/run-benches.sh --profile quick --bench catalog_descriptors --filter catalog_declaration
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_declaration/outer_publication
```

All intervals below are nanoseconds. Exact IDs are
`catalog_runtime_binding/<operation>_<bound|unbound>/<declarations>`.

| Operation | Declarations | Before, unbound | Before, bound | Corrected, unbound | Corrected, bound |
|---|---:|---:|---:|---:|---:|
| property_access | 1 | 9.1176–9.3897 | 39.276–40.577 | 9.7657–9.8719 | 28.007–29.453 |
| property_access | 16 | 9.0791–9.3374 | 41.461–43.899 | 9.9713–10.339 | 27.877–29.257 |
| property_access | 256 | 8.7615–9.1785 | 141.58–148.02 | 9.6340–10.084 | 27.700–28.608 |
| vector_access | 1 | 9.1856–9.3963 | 40.690–41.476 | 9.7089–10.206 | 28.411–29.512 |
| vector_access | 16 | 9.2243–9.4754 | 42.269–44.097 | 9.9783–10.621 | 28.291–29.796 |
| vector_access | 256 | 9.0882–9.4875 | 143.05–151.40 | 10.004–10.511 | 28.350–28.946 |
| candidate_probe | 1 | 44.986–46.638 | 44.475–45.877 | 44.512–45.899 | 62.287–65.714 |
| candidate_probe | 16 | 42.009–42.707 | 42.872–45.213 | 44.986–46.993 | 61.152–64.196 |
| candidate_probe | 256 | 42.764–44.055 | 42.027–43.989 | 43.285–44.514 | 62.143–64.046 |

The original bound accessor cost grew with owner declarations. Admission now
precompiles keyed metadata, with no whole-owner scan or String-to-DbString/Vec
construction on scalar/vector probes. Current native names/configuration and
drift remain checked. Bound access at 256 declarations improved by about 81% in
this matched fixture. Unbound accessor controls rose roughly 0.6–1.2 ns, and are
reported rather than hidden. The **candidate-probe increase is correctness
overhead**: its original bound path bypassed eligibility entirely; it now pays
the required check. This is not a claim that candidate queries became faster.
Ten-sample results remain characterization, not general throughput guarantees.

Bench bin: `catalog_lifecycle`. The quick profile measures absolute schema
resolve, deterministic schema listing, outer snapshot clone, schema
create/drop publication at 16 and 256 user schemas, and default/authenticated
session-context creation at 4 and 32 registered graphs. The full profile adds
1,024 schemas and 128 graphs. M03-PR04 Part 1 adds focused direct-reservation,
selected graph staging/publication, and publish-then-read rows at one graph.
The create and drop rows time only the named operation; the inverse operation
restores the fixture outside the returned Criterion duration.

M02-PR03 quick evidence was recorded on 2026-08-23 with Apple M5 (10 cores,
16 GiB), macOS 26.7 build 25G220, rustc 1.97.1, and the worktree based on
`69c95581f30a83c67502a61135ac17bc83862643`:

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle
```

These are 10-sample Criterion confidence intervals from one quick run. They are
recorded baselines, not regression thresholds.

| Bench | Small scale | Large scale |
|---|---:|---:|
| `catalog_lifecycle/read/resolve_schema` | 74.176–78.665 ns (16 schemas) | 143.43–151.37 ns (256 schemas) |
| `catalog_lifecycle/read/list_schemas` | 820.21–908.02 ns (16 schemas) | 16.063–17.124 µs (256 schemas) |
| `catalog_lifecycle/read/clone_outer_snapshot` | 3.8280–4.1036 ns (16 schemas) | 3.8296–3.8889 ns (256 schemas) |
| `catalog_lifecycle/outer_snapshot_publication/create_schema` | 3.0397–3.1255 µs (16 schemas) | 68.633–71.906 µs (256 schemas) |
| `catalog_lifecycle/outer_snapshot_publication/drop_schema` | 3.0956–3.3848 µs (16 schemas) | 67.853–70.335 µs (256 schemas) |
| prior named graph-open row (removed) | 116.55–135.69 ns (4 graphs) | 141.25–151.78 ns (32 graphs) |

M02-PR05 selected-session evidence was recorded on 2026-08-24 on the same
Apple M5 host with the worktree based on
`cc740b8ba3d3f4c4a70957c3b83e4fe4701f3532`:

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle
```

The renamed row resolves the graph and constructs its stable-ID facade session.
This is one 10-sample quick run, not a regression threshold.

| Bench | 4 graphs | 32 graphs |
|---|---:|---:|
| `catalog_lifecycle/session_creation/resolve_graph` | 133.50–135.44 ns | 158.60–160.89 ns |

M03-PR01 session-context evidence was recorded on 2026-08-24 with Apple M5 (10
cores, 16 GiB), macOS 26.7 build 25G220, rustc 1.97.1, and the worktree based on
`002174a5ff2c5134467d4aa5bbbe8906f8c94a74`:

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter default_context
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter authenticated_allow_context
```

The default row resolves and copies the anonymous `SessionContext`. The
authenticated row also invokes a local principal provider and the explicit
allow-all policy. These are 10-sample Criterion confidence intervals from one
quick run, not regression thresholds.

| Bench | 4 graphs | 32 graphs |
|---|---:|---:|
| `catalog_lifecycle/session_creation/default_context` | 243.66–246.63 ns | 272.00–281.33 ns |
| `catalog_lifecycle/session_creation/authenticated_allow_context` | 282.04–312.81 ns | 317.94–348.64 ns |

#### M03-PR02 request setup evidence

The request rows measure facade parse/analyze/preflight/plan/execute setup on a
fresh empty graph. `minimal_execute` retains the compatibility
`Session::execute("RETURN 1")` call. The parameter rows construct an explicit
`Request` from a cloned `RequestParams`, merge it with the session dictionary,
validate all supplied declarations, and execute `RETURN 1`; unused bindings are
allowed by the request contract.

Recorded on 2026-08-24 with Apple M5 (4 performance and 6 efficiency cores, 16
GiB), macOS 26.7 build 25G220, rustc 1.97.1, and mimalloc. The baseline row was
run before request lifecycle changes at
`0071db54fc0360b6d14b65db4eae2eec21b76efa`. Current rows use the M03-PR02
worktree based on that SHA:

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_lifecycle/request/minimal_execute
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_lifecycle/request_setup
```

| Bench | 10-sample quick interval |
|---|---:|
| `catalog_lifecycle/request/minimal_execute` (base) | 8.1383–8.3502 µs |
| `catalog_lifecycle/request/minimal_execute` (current, five repeats) | 17.695–18.865 µs; 38.052–39.029 µs; 37.151–37.882 µs; 29.840–30.434 µs; 37.182–38.677 µs |
| `catalog_lifecycle/request_setup/0` (three repeats) | 29.854–30.338 µs; 36.303–39.293 µs; 28.723–29.008 µs |
| `catalog_lifecycle/request_setup/10` (three repeats) | 30.931–34.076 µs; 37.855–39.264 µs; 18.247–19.338 µs |
| `catalog_lifecycle/request_setup/100` (three repeats) | 43.875–44.334 µs; 50.982–51.597 µs; 31.325–31.812 µs |
| `catalog_lifecycle/request_setup/1000` (three repeats) | 197.69–201.31 µs; 202.95–209.45 µs; 184.76–191.45 µs |

The current rows varied substantially across repeated quick runs. These
measurements do not support an improvement claim.

#### M03-PR05 persistent session-control rows

The `catalog_lifecycle/session_control` group measures a validated prepared-plan
hit, reprepare after a session-characteristic dependency miss, SET/RESET graph
resolution, and ten repeated short requests. The facade cache retains only the
request-independent compiled plan and always binds fresh request values.

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_lifecycle/session_control
```

Recorded on 2026-08-29 on arm64 macOS 26.7 (25G220), rustc 1.97.1, with
mimalloc. This is one 10-sample quick run; the rows are characterization, not
regression thresholds or performance claims.

| Bench | 10-sample quick interval |
|---|---:|
| `catalog_lifecycle/session_control/prepared_cache_hit` | 882.27–920.48 ns |
| `catalog_lifecycle/session_control/characteristic_miss_reprepare` | 916.43–935.03 ns |
| `catalog_lifecycle/session_control/set_reset_graph_resolve` | 15.000–15.854 µs |
| `catalog_lifecycle/session_control/repeated_short_query_10` | 8.9000–9.0822 µs |

#### M04-PR01 facade reference validation

The `catalog_lifecycle/reference_validation` group measures successful runtime
validation for one database-scoped graph, node, or edge handle. The fixture has
one graph, two live nodes, and one live edge. Graph validation also resolves and
copies its facade descriptor; node/edge rows cover the stable-ID liveness lookup
under the selected graph lifecycle lease. These are characterization rows, not
regression thresholds or an improvement claim.

```bash
scripts/run-benches.sh --bench catalog_lifecycle --compile-only
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_lifecycle/reference_validation
```

Recorded on 2026-08-31 on arm64 macOS 26.7 (25G220), rustc 1.97.1, with
mimalloc and the M04-PR01 worktree based on
`6de963c991c476a9853df13280fc46ae0e1192d8`. This is one 10-sample quick run.

| Bench | 10-sample quick interval |
|---|---:|
| `catalog_lifecycle/reference_validation/graph` | 64.686–67.491 ns |
| `catalog_lifecycle/reference_validation/node` | 50.063–53.285 ns |
| `catalog_lifecycle/reference_validation/edge` | 50.252–55.794 ns |

#### M03-PR03 execution-context microbenchmarks

Bench bin: `execution_context`. The focused rows cover root construction,
child push/drop cleanup, immutable record amendment, a two-row/two-column
binding-table construction, and statement-result conversion including declared
descriptor allocation. They are characterization rows, not regression claims.

```bash
scripts/run-benches.sh --profile quick --bench execution_context
```

#### M03-PR04 Part 1 publication-authority evidence

The `catalog_lifecycle/transaction_authority` group characterizes the new
in-memory authority without claiming an improvement. The direct row includes
one common reservation and outer catalog publication. The selected insert row
includes single-pass planning, CORE-only scratch construction, unpublished
graph preparation, replacement construction, and one outer publication. The
publish-then-read row adds a read-only request that must observe the complete
new graph. Cleanup runs outside each returned Criterion duration.

Recorded on 2026-08-26 with Apple M5 (10 cores, 16 GiB), macOS 26.7 build
25G220, rustc 1.97.1, mimalloc, and the M03-PR04 Part 1 worktree based on
`1b6d801e43d6d2ded02ba74edf28824f3b36db26`:

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_lifecycle/transaction_authority
```

| Bench | 10-sample quick interval |
|---|---:|
| `catalog_lifecycle/transaction_authority/direct_schema_reserve_publish` | 590.55–593.91 ns |
| `catalog_lifecycle/transaction_authority/selected_insert_stage_publish` | 123.06–146.13 µs |
| `catalog_lifecycle/transaction_authority/selected_publish_then_read` | 256.49–271.13 µs |

#### M03-PR04 Part 2 transaction-demarcation characterization

The existing `catalog_lifecycle/transaction_authority` group now also measures
empty read-write start/commit and start/rollback, read-only snapshot acquisition
plus rollback, implicit one-write staging/publication, and four explicit staged
writes plus one commit. Cleanup is outside each returned Criterion duration.
These are quick characterization numbers, not an improvement claim.

Recorded on 2026-08-26 with Apple M5 (10 cores, 16 GiB), macOS 26.7 build
25G220, rustc 1.97.1, and mimalloc on the M03-PR04 Part 2 worktree based on
`4750e2efd799bfb95766b9687d8cdee14fb054fe`:

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter catalog_lifecycle/transaction_authority
```

| Bench | 10-sample quick interval |
|---|---:|
| `catalog_lifecycle/transaction_authority/empty_start_commit` | 310.53–312.20 ns |
| `catalog_lifecycle/transaction_authority/empty_start_rollback` | 307.90–327.35 ns |
| `catalog_lifecycle/transaction_authority/read_snapshot_start_rollback` | 307.25–320.63 ns |
| `catalog_lifecycle/transaction_authority/selected_insert_stage_publish` | 179.61–226.71 µs |
| `catalog_lifecycle/transaction_authority/explicit_four_write_stage_commit` | 1.2539–1.4208 ms |

#### M02-PR04 part 1 quick evidence

The `catalog_lifecycle/gql_ddl` group issues the same schema and graph
lifecycle commands as GQL database-catalog statements through a selected
facade `Session`, at the same schema scales as the Rust-API publication
rows. Timed rows cover only the GQL statement; the inverse operation restores
the fixture through the Rust API outside the returned duration. The
`create_or_replace_graph_gql` row replaces the fixture graph on every
iteration (drop admission plus create in one publication) and needs no
cleanup. The `create_graph_if_not_exists_noop_gql` row publishes nothing and
isolates the per-request parse/analyze/plan cost of the fresh lower session
(there is no plan cache on this path), which is the constant difference
between a GQL row and its Rust-API counterpart.

The group also includes `create_graph_type_gql` and `create_bound_graph_gql`.
The first times the bounded property-free node definition and removes it through
the Rust API outside the timed interval. The second resolves that named type and
constructs a closed graph; graph cleanup is outside the timed interval.

Recorded on 2026-08-23 with Apple M5 (10 cores, 16 GiB), macOS 26.7 build
25G220, rustc 1.97.1, and the worktree at `ac93178f` plus the
`CREATE OR REPLACE GRAPH` repair:

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle
```

10-sample Criterion confidence intervals from one quick run; recorded
baselines, not regression thresholds. The Rust-API rows from the same run are
listed for direct comparison. The `create_or_replace_graph_gql` 16-schema
interval was wide in this run; the immediately preceding run of the same
command measured it at 59.478–60.154 µs.

| Bench | 16 schemas | 256 schemas |
|---|---:|---:|
| `catalog_lifecycle/outer_snapshot_publication/create_schema` (Rust API) | 2.8742–2.9183 µs | 61.384–62.220 µs |
| `catalog_lifecycle/outer_snapshot_publication/drop_schema` (Rust API) | 2.7734–2.8207 µs | 63.339–69.788 µs |
| `catalog_lifecycle/gql_ddl/create_schema_gql` | 32.367–32.958 µs | 89.828–91.754 µs |
| `catalog_lifecycle/gql_ddl/create_graph_gql` | 43.711–44.740 µs | 101.50–103.25 µs |
| `catalog_lifecycle/gql_ddl/drop_graph_gql` | 50.772–51.826 µs | 104.27–104.74 µs |
| `catalog_lifecycle/gql_ddl/create_or_replace_graph_gql` | 62.227–84.952 µs | 113.69–115.11 µs |
| `catalog_lifecycle/gql_ddl/create_graph_if_not_exists_noop_gql` | 30.446–31.413 µs | 29.980–30.527 µs |

The graph-type rows were recorded on 2026-08-24 on the same host, from the
M02-PR04 part 2 worktree based on `1fab61fadfa5dd14f8f14df0f5b0952c2dc2ecce`:

```bash
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter create_graph_type_gql
scripts/run-benches.sh --profile quick --bench catalog_lifecycle --filter create_bound_graph_gql
```

| Bench | 16 schemas | 256 schemas |
|---|---:|---:|
| `catalog_lifecycle/gql_ddl/create_graph_type_gql` | 41.415–70.516 µs | 111.64–147.30 µs |
| `catalog_lifecycle/gql_ddl/create_bound_graph_gql` | 40.340–41.438 µs | 95.606–98.579 µs |

## §1 selene-core

Bench bins: `value_clone`, `vector_wgpu`. `value_clone` measures `Value` /
`PropertyMap` clone cost: every clone memcpys the whole enum regardless of the
active variant, then pays the active variant's owned/shared payload cost. A
compile-time `size_of::<Value>() <= 32` ceiling in `value.rs` is the zero-cost
re-bloat tripwire; the bench prints the live size to stderr. **Measured
`size_of::<Value>() = 32 bytes`** at this commit — CORE-06 boxed the four
oversized variants (`Path` 120 B — the real former ceiling, not the time
variants — plus `Duration`/`ZonedDateTime`/`ZonedTime`), down from 128 B.
`DbString` now stores `Arc<str>`, so string values, labels, and property keys
clone by sharing storage rather than copying string bytes.
The same bin also covers open and compact property-map construction paths so
`from_pairs` stays linearithmic rather than repeated-insert quadratic for
schema- or record-shaped maps with many properties, plus common one-property
compact-map construction, compact-map encoding, and mutation diff constructors.
Already-canonical `from_pairs` inputs are tracked separately from reverse-sorted
inputs so the constructor can skip redundant sort/dedup work without regressing
the non-canonical path. Compact-map 256-key rows mirror that guard for closed
schema-shaped maps. Standard/compact property-map postcard encode rows pin the
canonical WAL/snapshot serialization path separately from construction cost.
The `core_change_diff/*` rows cover canonical WAL diff constructors so mutation
payload construction and postcard serialization stay cheap when callers already
have sorted property keys.
The `core_label_set/*` rows cover high-cardinality label-set construction; most
runtime rows stay at one to three labels, but schema and test fixtures can build
larger sets and should not pay repeated insertion shifts.
The `core_vector_value/*` rows are the first native vector
baselines: validation/construction, `Arc<[f32]>` clone cost, and postcard
round-trip cost at common embedding dimensions. The `core_vector_distance/*`
and `core_vector_exact_top_k/*` rows are exact-search oracle baselines for the
future ANN layer; current kernels use safe `wide::f64x4` accumulation over the
existing `f64` score semantics, so these rows are the SIMD/Rayon improvement
tripwire. The `cosine_omlx_*` exact-top-k rows pin product-shaped local embedding
dimensions and candidate widths without depending on the localhost oMLX service.
The `core_vector_gpu_baseline/*` rows are CPU/SIMD and host-pack envelopes for
future optional GPU acceleration: a Metal/CUDA backend must beat these rerank
times after any setup and host/device transfer costs, not just win a kernel
microbenchmark. The split host-pack rows model warm GPU-resident candidates
versus cold candidate upload/resync, and the resident-slab CPU rows show the
layout-only speedup from prepacked contiguous candidates plus cached norms.
`vector_wgpu` is the first benchmark-only wgpu compute prototype. It keeps
candidate vectors resident in GPU buffers, optionally rewrites the query batch,
scores every query/candidate pair with a WGSL cosine kernel, reads all scores
back to the host, and validates a score prefix against the CPU oracle during
setup. The default case list includes q8/q16 over 4,096 candidates, a
2560-dimension larger-embedding row, and q8/q16 over 10,000 candidates. A
100,000-candidate q8/d1024 row is opt-in with `SELENE_WGPU_STRESS_CASES=1`.
The newest rows compare a Rayon CPU score+top-k path against fused WGPU
score+block-top-k kernels that avoid full score-buffer readback, including a
parallel in-workgroup top-k reducer probe and q16 hot-shard x8 reuse rows. It
also registers the Rayon rows when WGPU adapter discovery fails, so local Metal
availability issues do not hide the CPU-parallel baseline. It is not a
production accelerator API.

| Bench | Median | Notes |
|---|---:|---|
| `core_value_clone/vec_mixed_1024` | 4.41 µs | Clone a 1024-element mixed-variant `Vec<Value>`. Quick local A/B after `DbString` moved to shared storage: 4.63 µs → 4.41 µs. |
| `core_value_clone/property_map_5` | 45.5 ns | Clone a 5-key `PropertyMap` (Int/Float/String/Duration/ZonedDateTime). Quick local A/B after `DbString` moved to shared storage: 55.2 ns → 45.5 ns. |
| `core_value_clone/json_canonical_string_metadata` | 184.98 ns (quick) | Render a nested agent-memory JSON metadata document to canonical compact JSON. Latest whole-value serde_json render A/B: 336.64 ns → 184.98 ns by delegating compact rendering to serde_json over the already-sorted object map. Earlier object-order guard: 419.17 ns → 386.24 ns. |
| `core_value_clone/json_canonical_string_object64` | 3.0605 µs (quick) | Render a 64-field JSON object with nested scalar metadata values. Latest whole-value serde_json render A/B: 5.0618 µs → 3.0605 µs with the same canonical map-order invariant. Earlier object-order guard: 6.2762 µs → 5.8432 µs. |
| `core_value_clone/json_parse_metadata` | 616.50 ns (quick) | Parse and validate the nested agent-memory JSON metadata document, including duplicate-key detection. Latest PR-local parse-validation A/B: 633.88 ns → 616.50 ns by enforcing JSON caps during strict deserialization and skipping the post-parse validation walk, while using a single object-map entry lookup for duplicate keys. Earlier map-backed duplicate check: 722.23 ns → 572.00 ns. |
| `core_value_clone/json_parse_object64` | 8.7265 µs (quick) | Parse and validate a 64-field JSON object with nested scalar metadata values. Latest PR-local parse-validation A/B: 10.651 µs → 8.7265 µs with the same fused validation and single-lookup duplicate-key insertion. Earlier map-backed duplicate check: 13.320 µs → 10.944 µs. |
| `core_value_clone/property_map_from_pairs_1` | 8.509 ns (quick) | Build a one-property standard `PropertyMap`. PR-local singleton fast path A/B: 20.617 ns → 8.509 ns by returning len 0/1 maps before sort/dedup work. |
| `core_value_clone/property_map_compact_1` | 22.327 ns (quick) | Build a one-key compact `PropertyMap`. PR-local singleton fast path A/B: 50.580 ns → 22.327 ns by collecting keys/values inline and returning len 0/1 maps before sort/dedup work. |
| `core_value_clone/property_map_compact_postcard_encode_1` | 22.413 ns (quick) | Encode a canonical one-key compact `PropertyMap` with postcard. Latest PR-local borrowed-serde A/B: 35.921 ns → 22.413 ns by serializing canonical key/value storage by reference instead of cloning it before encode. Earlier canonical fast path: 60.158 ns → 35.470 ns by reusing length-aligned sorted compact storage. |
| `core_value_clone/property_map_from_pairs_256_reverse` | 2.6387 µs (quick) | Build a 256-property map from reverse-sorted pairs. Quick local A/B after `DbString` moved to shared storage: 3.45 µs → 2.68 µs. Canonical-scan guard after the sorted-input fast path: 2.6488 µs → 2.6387 µs. |
| `core_value_clone/property_map_from_pairs_256_sorted` | 1.6249 µs (quick) | Build a 256-property map from already-canonical pairs. PR-local canonical fast path A/B: 2.5746 µs → 1.6249 µs by reusing the collected sorted entries directly. |
| `core_value_clone/property_map_standard_postcard_encode_256` | 2.2996 µs (quick) | Encode a canonical 256-entry standard `PropertyMap` with postcard. PR-local borrowed-serde A/B: 3.5841 µs → 2.2996 µs by avoiding clone/sort on already-canonical entries while preserving the non-canonical public-construction fallback. |
| `core_value_clone/property_map_compact_256_reverse` | 4.7462 µs (quick) | Build a 256-key compact map from reverse-sorted schema keys. PR-local canonical-key guard: 4.7589 µs → 4.7462 µs, preserving the existing sort/dedup path for non-canonical input. |
| `core_value_clone/property_map_compact_256_sorted` | 1.7334 µs (quick) | Build a 256-key compact map from already-canonical schema keys. PR-local canonical-key fast path A/B: 4.6719 µs → 1.7334 µs by reusing aligned keys/values directly. |
| `core_value_clone/property_map_compact_postcard_encode_256` | 2.5201 µs (quick) | Encode a canonical 256-key compact `PropertyMap` with postcard. PR-local borrowed-serde A/B: 3.5046 µs → 2.5201 µs by borrowing aligned compact key/value slices instead of cloning them before encode. |
| `core_change_diff/property_diff_set_1` | 9.0227 ns (quick) | Build a `PropertyDiff` with one set property and no removals. PR-local A/B: 23.291 ns → 9.0227 ns (-61.3%) by collecting directly into inline `SmallVec` storage and skipping sort/dedup for len 0/1 set inputs. |
| `core_change_diff/property_diff_set_256_reverse` | 2.6632 µs (quick) | Build a 256-property `PropertyDiff` from reverse-sorted set entries. PR-local canonical-set guard: 2.6779 µs → 2.6632 µs, preserving the existing stable sort/dedup path for non-canonical input. |
| `core_change_diff/property_diff_set_256_sorted` | 1.6691 µs (quick) | Build a 256-property `PropertyDiff` from already-canonical set entries. PR-local canonical-set fast path A/B: 2.5985 µs → 1.6691 µs by skipping redundant sort/dedup work. |
| `core_change_diff/property_diff_removed_256_sorted` | 719.80 ns (quick) | Build a removal-only `PropertyDiff` with 256 already-canonical removed keys; grounds removed-side constructor and overlap-check cost separately from set-side rows. |
| `core_change_diff/property_diff_set_removed_128_each_sorted` | 1.3453 µs (quick) | Build a disjoint `PropertyDiff` with 128 canonical set entries and 128 canonical removed keys. PR-local merge-scan overlap A/B: 2.7893 µs → 1.3453 µs (-51.9%) by walking the two sorted key lists once instead of binary-searching every set key. |
| `core_change_diff/property_diff_postcard_encode_256_sorted` | 2.0383 µs (quick) | Encode a canonical 256-property `PropertyDiff` with postcard. PR-local borrowed-serde A/B: 3.4432 µs → 2.0383 µs by serializing canonical set/removal slices by reference while preserving the public-field sort fallback. |
| `core_change_diff/label_diff_added_100_reverse` | 636.81 ns (quick) | Build a 100-label `LabelDiff` from reverse-sorted added labels. PR-local canonical-label guard: 636.46 ns → 636.81 ns, preserving the existing sort/dedup path for non-canonical input. |
| `core_change_diff/label_diff_added_100_sorted` | 411.80 ns (quick) | Build a 100-label `LabelDiff` from already-canonical added labels. PR-local canonical-label fast path A/B: 625.95 ns → 411.80 ns by skipping redundant sort/dedup work. |
| `core_change_diff/label_diff_removed_100_sorted` | 281.86 ns (quick) | Build a removal-only `LabelDiff` with 100 already-canonical removed labels; complements the existing add-only rows. |
| `core_change_diff/label_diff_added_removed_50_each_sorted` | 351.75 ns (quick) | Build a disjoint `LabelDiff` with 50 canonical added labels and 50 canonical removed labels. PR-local merge-scan overlap A/B: 815.27 ns → 351.75 ns (-56.9%) by walking the two sorted label lists once instead of binary-searching every added label. |
| `core_change_diff/label_diff_postcard_encode_100_sorted` | 716.51 ns (quick) | Encode a canonical 100-label `LabelDiff` with postcard. PR-local borrowed-serde A/B: 877.40 ns → 716.51 ns by borrowing canonical added/removed slices instead of cloning before encode. |
| `core_label_set/from_iter_100_reverse` | 595.93 ns (quick) | Build a 100-label `LabelSet` from reverse-sorted labels. PR-local collect/sort A/B: 3.8124 µs → 605.67 ns by avoiding repeated front insertion shifts. PR-local unstable-sort A/B: 623.36 ns → 595.93 ns (-4.2099%, p=0.00). |
| `core_label_set/from_iter_100_sorted` | 395.12 ns (quick) | Build a 100-label `LabelSet` from already-canonical labels. PR-local canonical fast path A/B: 2.6632 µs → 395.12 ns by reusing the collected sorted labels directly. |
| `core_vector_value/construct_validate/128/768/1536` | 55.4 ns / 276 ns / 528 ns (quick) | Validate finite, non-empty `f32` vectors while constructing `VectorValue`; roughly linear in dimension. |
| `core_vector_value/clone_arc/128/768/1536` | 3.12 ns / 3.12 ns / 3.13 ns (quick) | Clone `VectorValue` shared component storage; intentionally dimension-independent. |
| `core_vector_value/postcard_roundtrip/128/768/1536` | 240 ns / 1.04 µs / 2.07 µs (quick) | Serialize and deserialize `Value::Vector`, including deserialize-time invariant checks. |
| `core_vector_distance/squared_euclidean/128/768/1536` | 19.0 ns / 116.3 ns / 224.2 ns (full B9) | Exact lower-is-better L2-squared metric, safe `f64x4` accumulation; B9 keeps the 128-dim single-chain path and improves the widest row. |
| `core_vector_distance/cosine/128/768/1536` | 31.0 ns / 179.7 ns / 358.6 ns (full B9) | Exact cosine distance with zero-norm checks and clamped similarity; B9 keeps one-off cosine mostly noise-flat while accelerating bound-query ANN paths. |
| `core_vector_distance/negative_inner_product/128/768/1536` | 15.6 ns / 90.0 ns / 179.7 ns (full B9) | Max-inner-product adapter (`-dot`) with lower-is-better ordering; B9 uses four independent dot accumulators for wider vectors. |
| `core_vector_exact_top_k/squared_euclidean_2048x128_k10` | 39.7 µs (full B9) | Exhaustive exact-search oracle over 2,048 candidates using a bounded max-heap (`O(n log k)`); B9 is noise-flat at this 128-dim width. |
| `core_vector_exact_top_k/cosine_2048x128_k10` | 51.958 µs (quick) | Bound-query cosine exact top-k over 2,048 candidates; retained-heap replacement now uses `BinaryHeap::peek_mut`, avoiding a pop+push pair when a candidate beats the current worst retained hit. |
| `core_vector_exact_top_k/squared_euclidean_2048x128_k1` | 38.858 µs (full) | Top-1 companion row for the L2 exact-search oracle over the same 2,048 candidates; keeps single-hit rerank behavior visible separately from the k10 heap envelope. |
| `core_vector_exact_top_k/cosine_2048x128_k1` | 50.750 µs (full) | Top-1 companion row for bound-query cosine exact search; grounds future single-hit rerank changes without inferring from the k10 row. |
| `core_vector_exact_top_k/cosine_omlx_{64/256/1024/4096}x1024_k10` | 12.2 µs / 47.6 µs / 188.3 µs / 750.7 µs (quick) | Product-shaped cosine rerank envelope for the 1024-dim local embedding model. |
| `core_vector_exact_top_k/cosine_omlx_{64/256/1024/4096}x2560_k10` | 29.6 µs / 116.6 µs / 465.1 µs / 1.856 ms (quick) | Product-shaped cosine rerank envelope for the 2560-dim local embedding model. |
| `core_vector_exact_top_k/cosine_omlx_{64/256/1024/4096}x4096_k10` | 47.0 µs / 185.5 µs / 739.3 µs / 2.959 ms (quick) | Product-shaped cosine rerank envelope for the 4096-dim local embedding model. |

PR-local JSON whole-render A/B:

`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench value_clone --filter json_canonical --save-baseline json-whole-render-before`,
then the same command with `--baseline json-whole-render-before` after
delegating `JsonValue::to_canonical_string` to serde_json's compact whole-value
renderer.

| Bench | Before | After | Delta |
|---|---:|---:|---:|
| `core_value_clone/json_canonical_string_metadata` | 336.64 ns | 184.98 ns | -45.382% (`p=0.00`) |
| `core_value_clone/json_canonical_string_object64` | 5.0618 µs | 3.0605 µs | -39.717% (`p=0.00`) |

PR-local JSON parse duplicate-check A/B:

`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench value_clone --filter json_parse --save-baseline json-parse-dup-before`,
then the same command with `--baseline json-parse-dup-before` after replacing
the side `BTreeSet` duplicate-key tracker with a serde_json object-map lookup.

| Bench | Before | After | Delta |
|---|---:|---:|---:|
| `core_value_clone/json_parse_metadata` | 722.23 ns | 572.00 ns | -20.146% (`p=0.00`) |
| `core_value_clone/json_parse_object64` | 13.320 µs | 10.944 µs | -17.938% (`p=0.00`) |

PR-local JSON parse-validation A/B:

`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench value_clone --filter json_parse --save-baseline json-parse-validate-fuse-pre`,
then the same command with `--baseline json-parse-validate-fuse-pre` after
moving JSON string/container cap checks into strict deserialization, skipping
the post-parse validation traversal for parsed values, and using the serde_json
object-map entry API so unique keys require one map lookup.

| Bench | Before | After | Delta |
|---|---:|---:|---:|
| `core_value_clone/json_parse_metadata` | 633.88 ns | 616.50 ns | -3.8136% (`p=0.01`) |
| `core_value_clone/json_parse_object64` | 10.651 µs | 8.7265 µs | -17.859% (`p=0.00`) |

| `core_vector_gpu_baseline/cpu_cosine_rerank_q1x4096x1024_k10` | 750.67 µs (quick) | CPU/SIMD exact rerank over one 1024-dim query and 4,096 candidates; first GPU break-even row. |
| `core_vector_gpu_baseline/host_pack_f32_q1x4096x1024_k10` | 336.43 µs (quick) | Lower-bound host packing copy for the same q1/c4096/d1024 input window, before real GPU transfer/setup. |
| `core_vector_gpu_baseline/host_pack_queries_f32_q1x4096x1024_k10` | 39.70 ns (quick) | Query-only host packing lower bound when candidates are already resident in the accelerator. |
| `core_vector_gpu_baseline/host_pack_candidates_f32_q1x4096x1024_k10` | 295.74 µs (quick) | Candidate-only host packing lower bound for cold upload or hot-shard resync. |
| `core_vector_gpu_baseline/cpu_cosine_resident_slab_q1x4096x1024_k10` | 672.48 µs (quick) | CPU rerank over prepacked contiguous candidates with cached norms; layout-only resident-state comparator. |
| `core_vector_gpu_baseline/cpu_cosine_rerank_q8x4096x1024_k10` | 6.010 ms (quick) | Batched CPU/SIMD exact rerank over eight 1024-dim queries and a shared 4,096-candidate window. |
| `core_vector_gpu_baseline/host_pack_f32_q8x4096x1024_k10` | 335.52 µs (quick) | Host-pack lower bound for q8/c4096/d1024; candidate payload dominates query payload. |
| `core_vector_gpu_baseline/host_pack_queries_f32_q8x4096x1024_k10` | 396.54 ns (quick) | Query-only host packing lower bound for the q8 warm-resident path. |
| `core_vector_gpu_baseline/host_pack_candidates_f32_q8x4096x1024_k10` | 291.09 µs (quick) | Candidate-only host packing lower bound for cold upload or shard resync. |
| `core_vector_gpu_baseline/cpu_cosine_resident_slab_q8x4096x1024_k10` | 5.407 ms (quick) | Resident-slab CPU comparator; trims the q8/d1024 rerank envelope by about 10% before any GPU work. |
| `core_vector_gpu_baseline/cpu_cosine_rerank_q8x4096x2560_k10` | 14.901 ms (quick) | Batched CPU/SIMD exact rerank over eight 2560-dim queries and 4,096 candidates; representative local larger embedding row. |
| `core_vector_gpu_baseline/host_pack_f32_q8x4096x2560_k10` | 857.04 µs (quick) | Host-pack lower bound for q8/c4096/d2560. |
| `core_vector_gpu_baseline/host_pack_queries_f32_q8x4096x2560_k10` | 994.53 ns (quick) | Query-only host packing lower bound for the q8/d2560 warm-resident path. |
| `core_vector_gpu_baseline/host_pack_candidates_f32_q8x4096x2560_k10` | 727.24 µs (quick) | Candidate-only host packing lower bound for cold upload or shard resync at larger local embedding dimensions. |
| `core_vector_gpu_baseline/cpu_cosine_resident_slab_q8x4096x2560_k10` | 13.867 ms (quick) | Resident-slab CPU comparator for the larger local embedding row. |
| `core_vector_gpu_baseline/cpu_cosine_rerank_q16x4096x1024_k10` | 12.007 ms (quick) | Larger query-batch CPU/SIMD rerank envelope for GPU-batch break-even tests. |
| `core_vector_gpu_baseline/host_pack_f32_q16x4096x1024_k10` | 337.17 µs (quick) | Host-pack lower bound for q16/c4096/d1024. |
| `core_vector_gpu_baseline/host_pack_queries_f32_q16x4096x1024_k10` | 794.50 ns (quick) | Query-only host packing lower bound for q16 warm-resident accelerator paths. |
| `core_vector_gpu_baseline/host_pack_candidates_f32_q16x4096x1024_k10` | 289.42 µs (quick) | Candidate-only host packing lower bound for cold upload or shard resync. |
| `core_vector_gpu_baseline/cpu_cosine_resident_slab_q16x4096x1024_k10` | 10.808 ms (quick) | Resident-slab CPU comparator; trims the q16/d1024 rerank envelope by about 10% before any GPU work. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback/q8x4096x1024` | 1.6822 ms (quick) | First wgpu/Metal end-to-end prototype: candidates resident, query batch copied each iteration, WGSL cosine scores all 32,768 pairs, and all scores are read back. Beats the q8 CPU resident-slab comparator (5.407 ms) by ~3.2x in this local quick run. |
| `core_vector_wgpu_prototype/resident_preloaded_score_readback/q8x4096x1024` | 1.5456 ms (quick) | Same q8 scoring/readback path with queries preloaded; close to query-copy because scoring/readback dominates over the small query payload. |
| `core_vector_wgpu_prototype/cold_candidate_upload_score_readback/q8x4096x1024` | 4.1782 ms (quick) | Cold-shard path: uploads the 4,096-candidate slab, rewrites queries, scores on GPU, and reads all scores back. Candidate upload dominates but still beats the q8 CPU resident-slab comparator. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback_cpu_topk/q8x4096x1024` | 1.5496 ms (quick) | Query-copy GPU score+readback followed by CPU `VectorTopK` over returned scores. CPU top-k adds little overhead at this candidate width. |
| `core_vector_wgpu_prototype/cpu_rayon_score_topk/q8x4096x1024` | 2.8047 ms (quick) | Rayon CPU comparator that scores candidates and maintains top-k directly without producing a full score slab. Useful baseline for GPU fused top-k rows. |
| `core_vector_wgpu_prototype/resident_query_copy_score_gpu_block_topk_cpu_merge/q8x4096x1024` | 3.1297 ms (quick) | Query-copy GPU score, GPU block-local top-k over 256-candidate blocks, partial-score/index readback, and CPU merge. Slower than full score readback at q8 because the simple block reducer adds more overhead than it saves. |
| `core_vector_wgpu_prototype/resident_query_copy_score_fused_block_topk_cpu_merge/q8x4096x1024` | 1.2930 ms (quick) | Fused WGPU score plus block-local top-k over resident candidates. This avoids full score-buffer readback and beats the same-run Rayon comparator and older two-pass block reducer. |
| `core_vector_wgpu_prototype/resident_query_copy_score_parallel_block_topk_cpu_merge/q8x4096x1024` | 1.2986 ms (quick) | Parallel in-workgroup top-k reducer probe. It validates cleanly and is effectively tied with the simpler fused row at q8/4096. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback/q16x4096x1024` | 2.6590 ms (quick) | Batched 65,536-pair wgpu scoring/readback with query copy. Beats the q16 CPU resident-slab comparator (10.808 ms) by ~4.1x locally. |
| `core_vector_wgpu_prototype/resident_preloaded_score_readback/q16x4096x1024` | 2.6808 ms (quick) | Same q16 path with queries preloaded; effectively tied with query-copy because scoring/readback dominates at this batch size. |
| `core_vector_wgpu_prototype/cold_candidate_upload_score_readback/q16x4096x1024` | 5.9342 ms (quick) | Cold-shard q16 path with candidate upload, query write, GPU scoring, and score readback. Still beats the q16 CPU resident-slab comparator, but the upload tax is visible. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback_cpu_topk/q16x4096x1024` | 2.7005 ms (quick) | Query-copy GPU score+readback followed by CPU `VectorTopK` for all 16 queries. The extra ranking step is small compared with scoring/readback. |
| `core_vector_wgpu_prototype/cpu_rayon_score_topk/q16x4096x1024` | 5.2245 ms (quick) | Rayon CPU score+top-k comparator for the q16 batch. Parallel CPU work improves the baseline but remains behind WGPU resident paths. |
| `core_vector_wgpu_prototype/resident_query_copy_score_gpu_block_topk_cpu_merge/q16x4096x1024` | 3.0623 ms (quick) | Same block-local top-k path at q16. In the same quick run it was effectively tied with full score readback (`3.0798 ms`) and ahead of full readback plus CPU top-k (`3.2836 ms`), but this reducer is still a benchmark probe rather than production shape. |
| `core_vector_wgpu_prototype/resident_query_copy_score_fused_block_topk_cpu_merge/q16x4096x1024` | 2.6872 ms (quick) | Fused WGPU score plus block-local top-k. Effectively tied with full score readback at this size, while cutting output volume and beating the Rayon comparator. |
| `core_vector_wgpu_prototype/resident_query_copy_score_parallel_block_topk_cpu_merge/q16x4096x1024` | 1.2981 ms (quick) | Parallel in-workgroup top-k reducer. This is the one default row where the parallel reducer clearly beats the lane-0 fused reducer (`2.6773 ms` in the same quick run). |
| `core_vector_wgpu_prototype/cpu_rayon_score_topk_hot_shard_x8/q16x4096x1024` | 41.906 ms (quick) | Eight repeated Rayon CPU score+top-k batches against the same resident-style candidate window. This is effectively linear from the same-run single-batch Rayon row (`5.2226 ms`). |
| `core_vector_wgpu_prototype/resident_hot_shard_x8_fused_block_topk_cpu_merge/q16x4096x1024` | 20.671 ms (quick) | Eight repeated query-copy fused WGPU score+top-k cycles against resident candidates. Throughput holds near the same-run single fused row (`2.6204 ms`), and the row is ~2.0x faster than the x8 Rayon comparator. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback/q8x4096x2560` | 3.8233 ms (quick) | Larger local embedding dimension with warm resident candidates and full score readback. Still well below the q8/d2560 CPU resident-slab comparator (`13.867 ms`). |
| `core_vector_wgpu_prototype/resident_preloaded_score_readback/q8x4096x2560` | 3.8640 ms (quick) | Same q8/d2560 scoring/readback path with queries already resident; query-copy remains negligible relative to scoring/readback. |
| `core_vector_wgpu_prototype/cold_candidate_upload_score_readback/q8x4096x2560` | 8.7333 ms (quick) | Cold upload for the 2560-dim candidate slab. Upload tax is visible but still below the CPU resident-slab comparator. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback_cpu_topk/q8x4096x2560` | 3.9350 ms (quick) | Query-copy score/readback plus CPU `VectorTopK`; ranking overhead remains small at 4,096 candidates. |
| `core_vector_wgpu_prototype/cpu_rayon_score_topk/q8x4096x2560` | 7.7630 ms (quick) | Rayon CPU score+top-k comparator for the larger local embedding dimension. |
| `core_vector_wgpu_prototype/resident_query_copy_score_gpu_block_topk_cpu_merge/q8x4096x2560` | 4.6799 ms (quick) | Block-local top-k still trails full score readback at this candidate width, even with larger vector dimensions. |
| `core_vector_wgpu_prototype/resident_query_copy_score_fused_block_topk_cpu_merge/q8x4096x2560` | 2.6873 ms (quick) | Fused WGPU row for the 2560-dim embedding case; the first row where fused output reduction clearly beats full score readback and the Rayon comparator. |
| `core_vector_wgpu_prototype/resident_query_copy_score_parallel_block_topk_cpu_merge/q8x4096x2560` | 2.6147 ms (quick) | Parallel in-workgroup top-k probe for the 2560-dim row. It is effectively tied with the simpler fused row (`2.5894 ms` in the same quick run). |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback/q8x10000x1024` | 4.4663 ms (quick) | Warm resident 10,000-candidate q8 row. Larger candidate windows improve throughput but full score readback still beats the simple block reducer. |
| `core_vector_wgpu_prototype/resident_preloaded_score_readback/q8x10000x1024` | 4.1366 ms (quick) | Preloaded query q8/10k row; still close to query-copy because query payload is tiny. |
| `core_vector_wgpu_prototype/cold_candidate_upload_score_readback/q8x10000x1024` | 8.7483 ms (quick) | Cold upload for 10,000 1024-dim candidates. Candidate upload roughly doubles the warm resident path. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback_cpu_topk/q8x10000x1024` | 4.1849 ms (quick) | CPU top-k over 80,000 returned scores remains a small part of the total row. |
| `core_vector_wgpu_prototype/cpu_rayon_score_topk/q8x10000x1024` | 6.8316 ms (quick) | Rayon CPU score+top-k comparator over the 10,000-candidate q8 window. |
| `core_vector_wgpu_prototype/resident_query_copy_score_gpu_block_topk_cpu_merge/q8x10000x1024` | 5.0904 ms (quick) | Simple block-local top-k remains slower than full score readback at q8/10k. A fused or parallel reducer is the next meaningful shader experiment. |
| `core_vector_wgpu_prototype/resident_query_copy_score_fused_block_topk_cpu_merge/q8x10000x1024` | 2.5989 ms (quick) | Fused WGPU score plus block-local top-k over 10,000 candidates. This is a clear win over full readback, the older block reducer, and Rayon CPU score+top-k. |
| `core_vector_wgpu_prototype/resident_query_copy_score_parallel_block_topk_cpu_merge/q8x10000x1024` | 2.6617 ms (quick) | Parallel in-workgroup top-k probe over 10,000 candidates. It is slightly behind the simpler fused row (`2.6157 ms` in the same quick run), so parallel reduction is not yet a clear replacement. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback/q16x10000x1024` | 7.6021 ms (quick) | Warm resident 160,000-pair q16/10k row. This is the first default row where block top-k nears full readback plus CPU ranking. |
| `core_vector_wgpu_prototype/resident_preloaded_score_readback/q16x10000x1024` | 7.6760 ms (quick) | Same q16/10k path with preloaded queries; effectively tied with query-copy at this scale. |
| `core_vector_wgpu_prototype/cold_candidate_upload_score_readback/q16x10000x1024` | 11.219 ms (quick) | Cold upload for the q16/10k row; upload still matters but scales better as query batch size rises. |
| `core_vector_wgpu_prototype/resident_query_copy_score_readback_cpu_topk/q16x10000x1024` | 7.9875 ms (quick) | Full score readback plus CPU ranking over 160,000 scores. |
| `core_vector_wgpu_prototype/cpu_rayon_score_topk/q16x10000x1024` | 13.241 ms (quick) | Rayon CPU score+top-k comparator at q16/10k. This reinforces that CPU parallelism helps benchmark honesty but does not erase the WGPU signal for large resident windows. |
| `core_vector_wgpu_prototype/resident_query_copy_score_gpu_block_topk_cpu_merge/q16x10000x1024` | 8.0790 ms (quick) | Block-local top-k is now close to full readback plus CPU ranking, but still not better enough to justify productionizing the current serial reducer. |
| `core_vector_wgpu_prototype/resident_query_copy_score_fused_block_topk_cpu_merge/q16x10000x1024` | 3.8601 ms (quick) | Fused WGPU score plus block-local top-k at q16/10k. This is the strongest WGPU row so far: about 2.1x faster than full readback plus CPU top-k and 3.4x faster than Rayon CPU score+top-k in the same quick run. |
| `core_vector_wgpu_prototype/resident_query_copy_score_parallel_block_topk_cpu_merge/q16x10000x1024` | 3.9287 ms (quick) | Parallel in-workgroup top-k probe at q16/10k. It remains far ahead of full readback plus CPU top-k and Rayon CPU, but is slightly behind the simpler fused row (`3.8622 ms` in the same quick run). |
| `core_vector_wgpu_prototype/cpu_rayon_score_topk_hot_shard_x8/q16x10000x1024` | 101.09 ms (quick) | Eight repeated Rayon CPU score+top-k batches for the wider resident candidate window. This is effectively linear from the same-run single-batch Rayon row (`12.684 ms`). |
| `core_vector_wgpu_prototype/resident_hot_shard_x8_fused_block_topk_cpu_merge/q16x10000x1024` | 30.884 ms (quick) | Eight repeated query-copy fused WGPU score+top-k cycles against the resident 10k candidate window. Throughput holds at ~41.4 Melem/s, matching the same-run single fused row (`3.8604 ms`) while staying ~3.3x faster than the x8 Rayon comparator. |
| `core_vector_wgpu_prototype/cpu_rayon_score_topk/q8x100000x1024` | 61.066 ms (quick, `SELENE_WGPU_STRESS_CASES=1`) | Opt-in 100k stress CPU-parallel baseline: ~13.1 Melem/s for 800,000 query/candidate cosine top-k scores. Same run skipped WGPU rows because wgpu compiled only `METAL` for this target and enumerated `available_adapters=none`; the harness now reports compiled backends, enumerated adapters, and attempted power preferences before falling back to the Rayon row. |

## §2 selene-graph — read hot paths

### F01-PR03 mixed-edge storage control (2026-09-09)

Command: `scripts/run-benches.sh --profile full --bench mixed_edge_storage --sample-size 30 --measurement-time 2`.
Native Apple M5, Darwin arm64, rustc 1.97.1, optimized bench profile, mimalloc;
serialized execution. Each fixture has 1,024 nodes and 1k or 10k empty-property
edges on the same ring, including parallel identities. The directed control has
no undirected edges; the mixed fixture alternates directed/undirected identities.
Both have exactly N edge identities and 2N incidences. Node setup is outside the
mutation timing; creation includes one in-memory commit, not WAL durability.

| Operation | Edges | Directed time estimate | Mixed time estimate |
|---|---:|---:|---:|
| Enumerate all endpoint incidences | 1,000 | 9.0995 µs | 12.349 µs |
| Enumerate IDs and inspect directionality | 1,000 | 20.296 µs | 22.213 µs |
| Create + commit batch | 1,000 | 253.89 µs | 256.58 µs |
| Enumerate all endpoint incidences | 10,000 | 15.358 µs | 17.540 µs |
| Enumerate IDs and inspect directionality | 10,000 | 147.21 µs | 144.82 µs |
| Create + commit batch | 10,000 | 2.8538 ms | 2.8211 ms |

The incidence comparison adds about 36% at 1k and 14% at 10k for this topology.
The 10k creation confidence intervals overlap (directed 2.8039–2.9183 ms;
mixed 2.8124–2.8341 ms); this is not evidence of a mutation speedup. This is a
same-revision topology control, **not** a pre/post revision regression claim.

Fresh-process native `ps` RSS deltas after edge creation/commit were 3,964,928 B
for both 1k fixtures (3,964.93 B/edge), and 45,465,600 B directed versus
45,383,680 B mixed at 10k (4,546.56 versus 4,538.37 B/edge). These single-sample
RSS deltas include allocator retention and transient commit allocations; they
are not exact retained graph ownership sizes or evidence of a memory saving.
The intrinsic directionality column adds one byte of payload per allocated row;
undirected edges retain one row and one identity rather than two directed rows.

Bench bins: `single_graph`, `mixed_edge_storage`, `vector_index_rebuild`, `vector_pq`,
`vector_ivf_pq`, `vector_turbo_projection`, `vector_turbo_churn`, `vector_ivf_pressure`, `vector_mixed_workload`,
`bulk_mutation`, `concurrent_read`, `bfs`, `text_search_bm25`. Most medians below
predate CORE-06 (measured at the 128 B `Value` layout). `graph_node_fetch`
returns a column ref (no `Value` clone) and is unaffected.

_Re-measured 2026-08-16 (same M5 box, rustc 1.97.1): the `bulk_mutation` rows via
`--bench bulk_mutation`, and `graph_node_fetch` / `graph_label_index_lookup` /
`graph_typed_index_point` via
`--bench single_graph --filter 'graph_(node_fetch|typed_index_point|label_index_lookup)'`.
Those rows are current; every other row in §2 still carries the file-level
2026-06-01 stamp._

**⚠ #1118 regressed every read hot path here — bisected, `p = 0.00`. See #1137.**

Read the three rows above against 2026-06-01 and they look mixed:
`graph_label_index_lookup` +43.6% at 10k / +36.7% at 100k, but
`graph_node_fetch` −17.8% and `graph_typed_index_point` −21.9%. Those net
figures are correct, and they are also **misleading** — which is the lesson
worth keeping from this row.

An A/B across the #1118 boundary — `cee7ef6c` (#1117, already rustc 1.97.1) vs
`33e03cc2` (#1118), toolchain held constant, and #1118 touched no bench or
fixture file — shows all nine comparisons regressing:

| Bench | pre-#1118 | post-#1118 | change |
|---|---:|---:|---:|
| `graph_node_fetch` 10k/50k/100k | 4.37 / 4.87 / 5.10 ns | 6.62 / 7.54 / 7.94 ns | **+51.5 / +54.9 / +55.8%** |
| `graph_label_index_lookup` | 6.28 / 8.48 / 6.68 ns | 11.25 / 11.42 / 12.10 ns | **+79.0 / +34.6 / +81.2%** |
| `graph_typed_index_point` | 11.80 / 11.88 / 11.91 ns | 13.22 / 13.18 / 12.84 ns | **+12.1 / +10.9 / +7.8%** |

Commands: `scripts/run-benches.sh --profile full --bench single_graph --filter 'graph_(node_fetch|typed_index_point|label_index_lookup)' --save-baseline pre1118`
at `cee7ef6c`, then the matching `--baseline pre1118` at `33e03cc2`.

**A stale baseline hid a 55% regression behind a −17.8% net.**
`graph_node_fetch` went 8.22 → ~4.4 ns between 2026-06-01 and #1117, and #1118
then gave back half that gain. Measured only against the old baseline the row
reads as an improvement. This is the concrete argument for section-level
re-stamping over one ageing file-level date: a net delta spanning many commits
cannot distinguish "never regressed" from "regressed after improving more."

Severity grades with how each map changed, which is partial mechanism:
string-keyed `MapM` (`idx_label` — `DbString` comparisons down a **sorted**
chunked tree, replacing a one-hash `imbl::HashMap` probe) +35–81%;
integer-keyed `MapM` (`node_id_to_row`) +52–56%; `FxHashMap`, which #1118 never
migrated (`property_index`, behind `graph_typed_index_point`) +8–12%. **That
last row is unexplained** — it regressed without its map changing, so some of
the cost is a secondary layout/cache effect rather than the probe itself.

Not release-blocking at single-digit nanoseconds, and #1118's write wins are
real and large (`delete_only/n100000/1000` −86%). The point is that the trade
was never priced: #1118's recorded A/B measured write rows only, so a read cost
on a ~60%-read engine landed unweighed.

**The prediction this section made was half right, and the miss is the
informative half.** It said the `PropertyMap`-clone-heavy rows
(`graph_edge_create_cascade`, `graph_mutation_commit_batch`) would both tighten
once `Value` shrank 128 B → 32 B. `graph_mutation_commit_batch` did, hard
(−57% to −71%). `graph_edge_create_cascade` did **not** — it moved −0.1% /
+1.5% / −4.3% across 10k/50k/100k, which is no change of the predicted kind.
So `Value` width was not that row's dominant term; adjacency insertion is, and
it is unaffected by payload width. (These are point-estimate deltas against the
2026-06-01 medians, not a Criterion `--baseline` comparison, so they carry no
p-value.) Treat the
remaining "will tighten at the next re-baseline" expectations in this file as
hypotheses, not scheduled wins. `graph_exact_vector_scan/*` is the native graph-level
exact-vector oracle: label-filtered row scan plus the core vector metric
kernels, returning stable node ids. Large exact scans use threshold-gated Rayon
for both unindexed label rows and flat-index row sets; cancellation/deadline
aware calls check once per chunk and keep the same parallel path. Large
`graph_vector_candidate_set/*` reranks use the same chunked cancellation-aware
Rayon primitive once a single set reaches the 4,096-candidate threshold.
Exact batch cosine scans reuse one candidate squared norm across the query
batch; candidate-set batch reranking intentionally keeps the fused per-query
cosine kernel because a separate candidate-norm pass regressed q64/d1024 rows.
Canonical candidate-set batch scoring uses query-level Rayon once the batch has
at least 4,096 total candidates spread across multiple sets, keeping each
per-query scan serial inside that branch so many-query batches do not nest
chunked reductions. Explicit node batch scoring normalizes to canonical
candidate sets once, then delegates to the same batch scorer. Graph-expanded
candidate-set batches parallelize root-set expansion only when the batch has at
least 16 root sets and a sampled estimate projects at least 8,192 expanded
candidates; smaller expanded batches stay serial to avoid Rayon overhead. The
neighbor batch scorer reuses one derived candidate set when every query uses the
same graph anchor, then falls back to sampled thresholded derivation for broad
distinct-anchor batches before entering canonical candidate-set batch scoring.
The candidate-set group also measures the Rust graph/vector boundary for deriving
canonical candidate sets from graph adjacency and reranking canonical candidate
sets by vector score.
`graph_vector_index_rebuild/*` times the
maintenance rebuild that reclaims stale ANN entries after vector update/delete
churn; `graph_vector_index_recommended_rebuild/*` compares recommended-only
maintenance against full rebuild on a multi-index IVF fixture where only one
index is above the rebuild threshold. `graph_text_bm25_exact/*` is the
dependency-light full-text correctness oracle: it scans string properties,
computes query-local BM25 statistics, and returns deterministic top-k text hits.
`graph_text_bm25_indexed/*` compares a reusable in-memory postings index against
the oracle: `prebuilt_*` is the repeated-query path, while `transient_*` includes
index construction so build cost stays visible. Fixture setup is excluded from
the reported Criterion duration. `graph_json_contains_scan/*`,
`graph_json_path_exists_scan/*`, `graph_json_path_contains_scan/*`, and
`graph_json_path_value_scan/*` are exact JSON metadata oracles over JSON-valued
node properties before maintained JSON/path indexes exist. Global JSON scans use
threshold-gated Rayon when the label row set has at least 16,384 rows, including
deadline-bearing checked calls; candidate-scoped JSON scans remain sequential
because they sort/dedup and can stop once `k` matches are found.
`graph_edge_property_scan/*`, `graph_edge_property_index_lookup/*`, and
`graph_point_connected_traversal/*` are edge-index sprint rows over an
open-control-shaped `CdlBlock`/`Point` fixture: the first scans `CONNECTED_TO`
edge-label rows and filters edge properties, the second uses the built-in
edge-property typed index, and the third walks `Point` nodes through labeled
adjacency before checking target metadata.
`graph_snapshot_read_loops/*` amortizes thread setup over many
`SharedGraph::read()` calls so the ArcSwap snapshot hot path is visible; the
older `graph_concurrent_reads` row remains a legacy spawn/join smoke row.
The focused `graph_vector_index_ivf_target_centroid_rebuild/*` group sweeps
explicit IVF list-count targets on the same rebuild fixture so read-side
candidate pressure can be compared against write-side retrain/reassignment cost.
`vector_pq` is a benchmark-only quantized candidate generator for
compression/recall research: PQ, dequantized scalar u8, scalar u8 code-space
distance, packed binary sign codes, and a portable TurboQuant-style scorer
produce short candidate sets, then full-fidelity vectors are exact reranked.
The TurboQuant-style row is intentionally benchmark-only: it uses safe scalar
bit-packed codes and deterministic orthogonal mixing to ground storage/recall
trade-offs before any production TurboVec-derived index or storage policy.
`vector_ivf_pq` adds a coarse synthetic IVF-style partition ahead of PQ,
scalar code-space, and binary scorers so future work can compare standalone
full-code scans against candidate-producer plus compression layering.
`vector_turbo_projection` sweeps the benchmark-only TurboQuant scorer across
128/768/1536 dimensions at a fixed 10k-row scale so storage ratio and safe
block-Hadamard rotation behavior are visible before production codec work. Its
production `TurboQuantCosine` rows track the current omitted search-width
default (`512`) across single, batch, filtered, filtered-batch, and
shared-filtered-batch search.
`vector_turbo_churn` applies the standard 10% vector update / 5% delete churn
shape to a 10k-row production `TurboQuantCosine` index and times approximate
search over the churned derived state at the current omitted search-width
default (`512`). It also times `TurboQuantCosine` index creation over the same
10k-row, 128-dimensional graph shape plus a 2k-row, 1536-dimensional graph
shape so online indexing cost stays visible for embedding-sized vectors.
`vector_ivf_pressure` uses the
production graph IVF index and records list-skew plus candidate-pressure
suffixes so future IVF/PQ layering work is grounded against real index fanout
under the expected 60% read / 40% write workload. It also includes the
`graph_ivf_target_centroids` sweep for explicit IVF list-count tuning.
`vector_mixed_workload` includes capped-maintenance cadence rows that compare
rebuilding one recommended IVF index per maintenance pass against rebuilding
every recommended IVF index after repeated 60/40 cycles. Vector benchmark IDs
include a memory/cardinality suffix:
`m{index KiB}-{reachable KiB}_n{indexed rows}_{flat|he...|ve...}`. The
`he...` form carries HNSW entries/live/deleted entries plus link counters; the
`ve...` form carries IVF entries/live/deleted entries plus centroid/list
counters. ANN recall IDs encode exact-ID recall as `idbp{basis points}` and
tie-tolerant nearest-distance quality as `dqbp{basis points}` before that
memory suffix.
unindexed rows use `noidx`. Rebuild IDs add
`upd{updates}_del{deletes}_b{entries-live-deleted}_a{entries-live-deleted}_rk{reclaimed reachable KiB}`.
Recommended-rebuild IDs add
`idx{registered indexes}_rb{rebuilt indexes}_pend{pending retrain entries}_bp{pending basis points}`.
Stale-query IDs use
`{stale|rebuilt}_n{rows}_{h|v}e{entries}l{live}d{deleted}_m{index KiB}-{reachable KiB}`,
where `h` is HNSW and `v` is IVF.
IVF pressure IDs use
`lists{centroids}ne{non_empty}max{max_list_len}avg{avg_list_len}avgq{avg_candidates_per_query}maxq{worst_case_candidates_per_query}_m{index KiB}-{reachable KiB}`.

PR-local B9 vector-kernel unroll A/B:

Commands:
`scripts/run-benches.sh --profile full --bench value_clone --filter core_vector_distance --save-baseline b9_pre`;
`scripts/run-benches.sh --profile full --bench value_clone --filter core_vector_exact_top_k --save-baseline b9_pre`;
`scripts/run-benches.sh --profile full --bench single_graph --filter graph_ann_recall_validation/cluster_cos_hnsw --vector-scales 10000 --save-baseline b9_pre`;
rerun each with `--baseline b9_pre` after the implementation. The required
`graph_exact_vector_scan` guard was also run, but concurrent desktop load made
the small 128-dim exact-scan rows noisy enough that they are not rebaselined
here.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `core_vector_distance/squared_euclidean/1536` | 249.26 ns | 224.21 ns | Four independent `f64x4` accumulators cut the wide L2-squared row by 10.2%; 128 dims remains on the single-chain path and was noise-flat. |
| `core_vector_distance/negative_inner_product/768` | 114.01 ns | 89.96 ns | Four-accumulator dot product improves the 768-dim MIPS adapter by 21.1%. |
| `core_vector_distance/negative_inner_product/1536` | 241.10 ns | 179.71 ns | Same dot-product path improves the 1536-dim row by 25.4%. |
| `core_vector_exact_top_k/cosine_2048x128_k10` | 54.46 µs | 52.97 µs | Bound-query cosine top-k improves 2.6%; most local-embedding rerank rows stayed within noise, with one 4096x2560 row showing a small local regression. |
| `graph_ann_recall_validation/cluster_cos_hnsw_d128_k10_ef10...` | 63.20 µs | 57.89 µs | HNSW cosine search improves 8.3% on the 10k clustered fixture. |
| `graph_ann_recall_validation/cluster_cos_hnsw_d128_k10_ef64...` | 159.03 µs | 149.27 µs | Larger default-HNSW `ef_search` improves 5.1%. |
| `graph_ann_recall_validation/cluster_cos_hnsw_m24ef64_d128_k10_ef64...` | 193.12 µs | 178.96 µs | Tuned-HNSW high-ef guard improves 4.3%, so ANN traversal benefits despite the conservative one-off cosine thresholds. |

PR-local B11 HNSW visited-buffer A/B:

Commands:
`scripts/run-benches.sh --profile full --bench single_graph --filter graph_ann_recall_validation/cluster_cos_hnsw --vector-scales 10000 --save-baseline b11_pre`;
`scripts/run-benches.sh --profile full --bench vector_index_rebuild --filter graph_vector_index_rebuild/hnsw --vector-scales 10000 --save-baseline b11_pre`;
rerun each with `--baseline b11_pre` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_ann_recall_validation/cluster_cos_hnsw_d128_k10_ef10...` | 58.13 µs | 54.94 µs | Replacing the per-layer `FxHashSet` with an epoch-marked dense buffer improves default-HNSW ef10 search by 4.6% with the same 9875 bp recall suffix. |
| `graph_ann_recall_validation/cluster_cos_hnsw_d128_k10_ef64...` | 148.95 µs | 118.12 µs | Higher-width default-HNSW search improves 23.0%, showing the visited buffer matters most when layer walks admit more candidates. |
| `graph_ann_recall_validation/cluster_cos_hnsw_m24ef64_d128_k10_ef10...` | 70.41 µs | 68.30 µs | Tuned-HNSW ef10 search improves 8.0% with the same 10000 bp recall suffix. |
| `graph_ann_recall_validation/cluster_cos_hnsw_m24ef64_d128_k10_ef64...` | 176.97 µs | 147.82 µs | Tuned-HNSW ef64 search improves 19.1%; this is the broadest query-side traversal row in the B11 guard set. |
| `graph_vector_index_rebuild/hnsw_l2_dim128_default` | 1.8897 s | 1.8745 s | Construction-side rebuild is effectively noise-flat; the layer-walk scratch change does not regress the 10k L2 default row. |
| `graph_vector_index_rebuild/hnsw_l2_dim128_m24ef64` | 3.2868 s | 3.2481 s | Tuned L2 rebuild remains within Criterion's noise threshold. |
| `graph_vector_index_rebuild/hnsw_cos_dim128_default` | 1.5623 s | 1.5330 s | Default cosine rebuild improves 1.9%, a small but statistically significant construction-side gain. |
| `graph_vector_index_rebuild/hnsw_cos_dim128_m24ef64` | 2.7218 s | 2.7141 s | Tuned cosine rebuild is noise-flat. |

PR-local quick vector exact-scan Rayon A/B:

Command: `scripts/run-benches.sh --profile quick --bench single_graph --filter graph_exact_vector_scan --vector-scales 50000`

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_exact_vector_scan/unindexed_squared_euclidean_dim128_k10_noidx/50000` | 1.2285 ms | 558.91 µs | Unindexed label-row exact scan now uses the existing row-chunked Rayon path above the 16,384-row threshold. |
| `graph_exact_vector_scan/unindexed_cosine_dim128_k10_noidx/50000` | 1.7016 ms | 592.95 µs | Same threshold-gated path for cosine; B10 later moved checked calls onto the same chunked cancellation-aware path. |
| `graph_exact_vector_scan/flat_index_squared_euclidean_dim128_k10_m64-64_n50k_flat/50000` | 581.30 µs | 568.49 µs | Existing flat-index parallel path remains stable. |
| `graph_exact_vector_scan/flat_index_cosine_dim128_k10_m64-64_n50k_flat/50000` | 616.20 µs | 591.44 µs | Existing flat-index parallel path remains stable. |

PR-local B10 cancellation-aware deadline quick A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_exact_vector_scan`,
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_json`,
and
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_vector_candidate_set`.

| Bench | Disabled checker | Deadline checker | Notes |
|---|---:|---:|---|
| `graph_exact_vector_scan/unindexed_squared_euclidean_dim128_k10_noidx/1000` | 23.240 µs | 23.309 µs | Checked deadline row now stays on the exact-scan parallel gate; delta is noise-scale at the quick 1k fixture. |
| `graph_exact_vector_scan/flat_index_squared_euclidean_dim128_k10_m4-4_n1k_flat/1000` | 23.155 µs | 23.131 µs | Flat-index row-set scan keeps the same chunked path under a deadline checker. |
| `graph_exact_vector_scan/unindexed_cosine_dim128_k10_noidx/1000` | 33.500 µs | 33.620 µs | Cosine checked row remains effectively tied to the disabled-checker row. |
| `graph_exact_vector_scan/flat_index_cosine_dim128_k10_m4-4_n1k_flat/1000` | 33.723 µs | 33.667 µs | Flat cosine deadline row is also within noise. |
| `graph_json_contains_scan/nested_metadata_k10/1000` | 22.097 µs | 22.275 µs | JSON containment checked-with-deadline row exercises the shared chunked scan helper. |
| `graph_json_path_exists_scan/nested_score_path_k10/1000` | 17.732 µs | 17.931 µs | Path-exists checked row stays parallel instead of falling back to serial deadline behavior. |
| `graph_json_path_contains_scan/nested_memory_path_k10/1000` | 19.796 µs | 19.632 µs | Path-containment checked row is within same-run quick noise. |
| `graph_json_path_value_scan/nested_score_path_k10/1000` | 23.718 µs | 23.653 µs | Path-value checked row keeps the same parallel gate; this pre-B21 row still included residual JSON-value clone cost. |
| `graph_vector_candidate_set/score_candidate_set_cosine_c64_d1024/64` | 12.870 µs | 12.878 µs | Below the 4,096-candidate threshold; both rows are sequential but checked overhead is noise-scale. |
| `graph_vector_candidate_set/score_candidate_set_cosine_c256_d1024/256` | 50.759 µs | 50.828 µs | Below threshold. |
| `graph_vector_candidate_set/score_candidate_set_cosine_c1024_d1024/1024` | 203.18 µs | 203.45 µs | Below threshold. |
| `graph_vector_candidate_set/score_candidate_set_cosine_c4096_d1024/4096` | 277.33 µs | 281.40 µs | At the parallel threshold, deadline checking now stays on the chunked Rayon scorer. |

PR-local B14/B12 full BM25 exact-scan A/B:

Commands:
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_exact --save-baseline b14_pre`
on the pre-change serial branch, then
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_exact`
on the B14/B12 branch. The comparison run with `--baseline b14_pre` was
stopped only because the branch adds the new `topic_query_checked_with_deadline`
row, which has no saved pre-change baseline.

| Bench | Before | After | Deadline checker | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_exact/topic_query/n10000_k10` | 3.0836 ms | 2.4305 ms | 2.4387 ms | Borrowed tokenizer removes per-token document allocations and the 10k fixture remains under the parallel threshold. |
| `graph_text_bm25_exact/topic_query/n50000_k10` | 15.844 ms | 3.5758 ms | 3.3681 ms | Exact BM25 scan now uses the shared cancellation-aware Rayon chunk reducer; corpus document-frequency merge remains element-wise. |
| `graph_text_bm25_exact/topic_query/n100000_k10` | 33.959 ms | 6.6143 ms | 6.5797 ms | Large exact BM25 scans keep the parallel path under a deadline checker instead of falling back to serial session behavior. |

PR-local quick TextIndex term-interning A/B:

Command:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_rebuild/create_registered_index`
before and after interning BM25 terms as shared `Arc<str>` storage across
postings keys and per-document maintenance term lists.

| Bench | Before | After | Signal |
|---|---:|---:|---|
| `graph_text_bm25_rebuild/create_registered_index/n1000` | 440.68 µs | 419.93 µs | Criterion reported −4.7623% (`p=0.00`); repeated term bytes are no longer duplicated into every document-term list. |

Guard commands after the change:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed`
and
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_mixed`.

| Guard row | Post-change median |
|---|---:|
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 27.815 µs |
| `graph_text_bm25_indexed/registered_topic_query/n1000_k10` | 27.785 µs |
| `graph_text_bm25_indexed/transient_build_query/n1000_k10` | 447.44 µs |
| `graph_text_bm25_mixed/registered_query_update_r60w40/n1000_k10` | 3.4034 ms |
| `graph_text_bm25_mixed/write_registered_update_w40/n1000` | 1.7031 ms |

PR-local BM25 document-term count accumulator A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_rebuild/create_registered_index`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_mixed/write_registered_update_w40`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/transient_build_query`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query`.

| Bench | Before | After | Signal |
|---|---:|---:|---|
| `graph_text_bm25_rebuild/create_registered_index/n1000` | 422.11 µs | 348.86 µs | Criterion reported -17.423% (`p=0.00`); short BM25 documents now count terms in inline storage before spilling to a hash map for high-cardinality documents. |
| `graph_text_bm25_mixed/write_registered_update_w40/n1000` | 1.7121 ms | 1.6922 ms | Neutral (`p=0.26`); update maintenance keeps the same durable postings representation. |

Post-change guard medians:

| Guard row | Post-change median |
|---|---:|
| `graph_text_bm25_indexed/transient_build_query/n1000_k10` | 387.61 µs |
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 27.896 µs |

PR-local BM25 differential document replacement A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench text_search_bm25 --filter graph_text_bm25_mixed/write_registered_update_w40 --save-baseline text_index_replace_s30_pre`
and
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench text_search_bm25 --filter graph_text_bm25_mixed/write_registered_update_w40 --baseline text_index_replace_s30_pre`.

| Bench | Before | After | Signal |
|---|---:|---:|---|
| `graph_text_bm25_mixed/write_registered_update_w40/n1000` | 1.6767 ms | 1.6251 ms | Maintained `TextIndex` document replacement now keeps postings for terms that survive an update instead of removing and reinserting the full document term set. Criterion reported -2.9066% (`p=0.00`). |

PR-local quick BM25 full-cover candidate A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query_candidates`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10`.

| Bench | Before | After | Signal |
|---|---:|---:|---|
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_sorted/n1000_k10` | 34.770 µs | 30.221 µs | Full-cover sorted candidate lists now delegate to the regular indexed scorer after verifying that the candidates cover every indexed document; Criterion reported -13.185% (`p=0.00`). |
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_reverse/n1000_k10` | 38.948 µs | 31.249 µs | Full-cover unsorted lists dedupe to the indexed corpus and then use the same regular indexed scorer; Criterion reported -21.787% (`p=0.00`). |
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 28.044 µs | 28.399 µs | Guard row for the delegated scorer; Criterion kept the change within the noise threshold. |

PR-local quick vector candidate-set scoring Rayon A/B:

Command: `scripts/run-benches.sh --profile quick --bench single_graph --filter graph_vector_candidate_set`

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_vector_candidate_set/score_candidate_set_cosine_c64_d1024/64` | 13.717 µs | 13.700 µs | Below the 4,096-candidate threshold; stays sequential and statistically unchanged. |
| `graph_vector_candidate_set/score_candidate_set_cosine_c256_d1024/256` | 54.786 µs | 54.653 µs | Below the threshold; stays sequential and statistically unchanged. |
| `graph_vector_candidate_set/score_candidate_set_cosine_c1024_d1024/1024` | 223.55 µs | 222.59 µs | Below the threshold; stays sequential and change remains noise-scale. |
| `graph_vector_candidate_set/score_candidate_set_cosine_c4096_d1024/4096` | 891.37 µs | 308.32 µs | Candidate-set rerank now uses chunked Rayon scoring once the set has at least 4,096 nodes; B10 later made that path cancellation/deadline aware. |

PR-local quick vector candidate-set batch Rayon A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench single_graph --filter graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64 --save-baseline candidate_batch_serial_q64_pre`
on the serial batch branch, then the same command with `--baseline
candidate_batch_serial_q64_pre` after enabling query-level batch parallelism.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c64_d1024/64` | 836.72 µs | 177.92 µs | 64 queries over 64-candidate canonical sets now cross the 4,096 total-candidate batch threshold and fan out by query. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c256_d1024/256` | 3.2221 ms | 611.32 µs | Query-level Rayon removes the serial per-query loop for medium graph-derived candidate sets. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c1024_d1024/1024` | 13.001 ms | 2.2670 ms | Per-query scans stay serial inside the batch branch, avoiding nested Rayon reductions while still using all worker threads across queries. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c4096_d1024/4096` | 18.015 ms | 9.5569 ms | Broad many-query batches now prefer query-level parallelism instead of repeatedly entering the single-set chunked scorer. |

PR-local quick explicit-node batch delegation A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench single_graph --filter graph_vector_candidate_set/score_nodes_batch_cosine_q64 --save-baseline nodes_batch_serial_q64_pre`
with the new benchmark row on the pre-change generic batch scorer, then the
same command with `--baseline nodes_batch_serial_q64_pre` after delegating
explicit-node batches through canonical candidate-set batch scoring.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_vector_candidate_set/score_nodes_batch_cosine_q64_c64_d1024/64` | 840.41 µs | 185.26 µs | Explicit node slices normalize once to canonical sets, then use the query-level batch scorer. |
| `graph_vector_candidate_set/score_nodes_batch_cosine_q64_c256_d1024/256` | 3.2679 ms | 631.07 µs | Normalization overhead is small relative to removing the serial 64-query scoring loop. |
| `graph_vector_candidate_set/score_nodes_batch_cosine_q64_c1024_d1024/1024` | 13.027 ms | 2.3506 ms | The generic API now carries the same many-query parallel shape as canonical candidate-set scoring. |
| `graph_vector_candidate_set/score_nodes_batch_cosine_q64_c4096_d1024/4096` | 18.304 ms | 9.5613 ms | Broad explicit-node batches avoid repeated single-set chunked reductions after normalization. |

PR-local quick graph-expanded batch expansion A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench single_graph --filter graph_vector_candidate_set/score_expanded_batch_cosine_q64 --save-baseline expanded_batch_serial_q64_pre`
with the new expanded-batch row on the pre-change serial expansion path, then
the same command with `--baseline expanded_batch_serial_q64_pre` after enabling
thresholded query-level root-set expansion.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_vector_candidate_set/score_expanded_batch_cosine_q64_c64_d1024/64` | 191.63 µs | 189.05 µs | The sampled expanded-work gate keeps this small expanded batch on the serial expansion path; change is noise-scale. |
| `graph_vector_candidate_set/score_expanded_batch_cosine_q64_c256_d1024/256` | 717.83 µs | 624.63 µs | Projected expanded work crosses the threshold, so root-set expansion fans out before shared candidate-set batch scoring. |
| `graph_vector_candidate_set/score_expanded_batch_cosine_q64_c1024_d1024/1024` | 2.4806 ms | 2.3041 ms | Wider graph-expanded batches keep the parallel expansion branch. |
| `graph_vector_candidate_set/score_expanded_batch_cosine_q64_c4096_d1024/4096` | 10.260 ms | 9.3476 ms | Broad expanded batches avoid serially deriving 64 large expanded sets before scoring. |

PR-local quick neighbor batch candidate derivation A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench single_graph --filter graph_vector_candidate_set/score_neighbors_batch_cosine_q64 --save-baseline neighbor_batch_serial_q64_pre`
with the new benchmark row on the pre-change serial neighbor derivation path,
then the same command with `--baseline neighbor_batch_serial_q64_pre` after
adding repeated-anchor candidate reuse and thresholded distinct-anchor
derivation. A threshold-only parallel derivation trial was neutral for
c64/c256/c1024 and only noise-scale faster for c4096, so the measured win comes
from avoiding duplicate adjacency walks when every query uses the same anchor.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_vector_candidate_set/score_neighbors_batch_cosine_q64_c64_d1024/64` | 190.73 µs | 181.84 µs | One 64-neighbor candidate set is derived once and reused across the 64-query batch. |
| `graph_vector_candidate_set/score_neighbors_batch_cosine_q64_c256_d1024/256` | 636.30 µs | 609.77 µs | Repeated-anchor reuse removes duplicate adjacency candidate construction before canonical batch scoring. |
| `graph_vector_candidate_set/score_neighbors_batch_cosine_q64_c1024_d1024/1024` | 2.3400 ms | 2.3147 ms | Broad reranking dominates; the candidate-derivation win is noise-scale at this width. |
| `graph_vector_candidate_set/score_neighbors_batch_cosine_q64_c4096_d1024/4096` | 9.5891 ms | 9.4297 ms | The 64-query rerank dominates and Criterion kept the change within the noise threshold. |

PR-local B5 id-map hasher A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_node_fetch`
and
`scripts/run-benches.sh --profile full --bench bulk_mutation --filter commit_batch`.

| Bench | Development | B5 | Notes |
|---|---:|---:|---|
| `graph_node_fetch/1000` | 9.177 ns | 7.125 ns | Engine-assigned id lookup now uses `FxBuildHasher` in the immutable id map; Criterion reported −22.7% median time. |
| `graph_mutation_commit_batch/n10000/10` | 87.211 µs | 85.462 µs | Existing-node update path, mostly lookup-bound; noise-scale/slight win. |
| `graph_mutation_commit_batch/n50000/10` | 114.94 µs | 107.65 µs | Same update path; warmed branch row was modestly faster. |
| `graph_mutation_commit_batch/n10000/1000` | 521.91 µs | 492.71 µs | Larger update batch stayed modestly faster in the warmed branch matrix. |
| `graph_mutation_commit_batch/n50000/100` | 171.50 µs | 199.07 µs | Noisy guard row: the first branch pass was 195.72 µs, an isolated rerun was 164.37 µs, and the warmed matrix returned 199.07 µs. Do not claim a write-side win from B5. |

| Bench | 10k | 50k | 100k | Notes |
|---|---:|---:|---:|---|
| `graph_node_fetch` | 6.18 ns | 7.17 ns | 7.42 ns | Near-flat O(1) columnar fetch. Re-measured 2026-08-16; 100k **−17.8%**. |
| `graph_label_index_lookup` | 11.24 ns | 10.90 ns | 11.06 ns | Flat; `DbString`-keyed hash lookup. Re-measured 2026-08-16; **10k +43.6%, 100k +36.7% — a regression, see below**. |
| `graph_typed_index_point` | 11.89 ns | 11.95 ns | 11.92 ns | Flat tri-state `lookup_eq`. Re-measured 2026-08-16; 100k **−21.9%**. |
| `graph_typed_index_range` | 7.05 µs | 37.89 µs | 55.74 µs | Sub-linear range scan. |
| `graph_composite_index_proxy` | 82.8 ns | 177.1 ns | 313.9 ns | Linear. |
| `graph_edge_create_cascade` | 362.5 µs | 758.9 µs | 1.417 ms | Mutation + commit body; teardown excluded. Re-measured 2026-08-16; 100k −4.3% — see the CORE-06 note above. |
| `graph_mutation_commit_batch` (10) | 84.01 µs | 104.5 µs | 127.8 µs | Batched commit, 10 ops. Re-measured 2026-08-16; **100k −71.4%**. |
| `graph_mutation_commit_batch` (100) | 118.8 µs | 156.5 µs | 187.2 µs | Batched commit, 100 ops. Re-measured 2026-08-16; **100k −66.1%**. |
| `graph_mutation_commit_batch` (1000) | 437.3 µs | 479.4 µs | 521.9 µs | Batched commit, 1000 ops. Re-measured 2026-08-16; **100k −57.4%**. |
| `graph_concurrent_reads` | 74.6 µs | 71.7 µs | 71.8 µs | Legacy row: 10 scoped threads with one snapshot read each; dominated by spawn/join. |
| `graph_snapshot_read_loops/single_thread` | 334.14 µs | 336.52 µs | 337.36 µs | 100k snapshot reads per sample, about 3.34-3.37 ns/read; scale-flat. |
| `graph_snapshot_read_loops/parallel_threads8` | 15.508 ms | 11.209 ms | 10.955 ms | 8 threads x 20k reads per sample, about 69-97 ns/read including scoped thread setup and contention. |
| `graph_bfs` (depth=1) | 106.3 ns | 109.0 ns | 109.6 ns | Depth-1 independent of N. |
| `graph_bfs` (depth=10) | 11.34 µs | 12.09 µs | 12.18 µs | Mostly traversal cost. |
| `graph_bfs` (depth=50) | 101.1 µs | 111.1 µs | 113.1 µs | Saturates ~110 µs. |

PR-local edge-index sprint baseline:

Commands:
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_edge_property_scan`
and
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_edge_property_index_lookup`
and
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_point_connected_traversal`.

| Bench | 1k quick | Notes |
|---|---:|---|
| `graph_edge_property_scan/1000` | 22.410 µs | Exact scan path: `CONNECTED_TO` edge-label bitmap scan, row-to-`EdgeId` mapping, and `from_port = 'out_0'` property check over 1,000 connected edges. |
| `graph_edge_property_index_lookup/1000` | 11.887 ns | Built-in edge-property typed index lookup for the same `(CONNECTED_TO, from_port = 'out_0')` predicate. Fixture build/index creation is excluded from the timed body. |
| `graph_point_connected_traversal/1000` | 26.569 µs | Open-control-shaped Point-node path: label scan over 1,000 `Point` nodes, filter output points, traverse outgoing `CONNECTED_TO`, and validate input-point metadata. |

PR-local quick text baseline:

| Bench | 1k | Notes |
|---|---:|---|
| `graph_text_bm25_exact/topic_query/n1000_k10` | 327.59 µs (quick) | Exact BM25 scan over 1,000 string-valued document nodes with Unicode-aware tokenization, query-local document frequencies, and deterministic score/node-id ordering. This is the oracle for postings-index and hybrid BM25/vector rows. |
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 34.665 µs (quick) | Repeated query over a prebuilt `TextIndex` postings structure. Same BM25 tokenizer/scorer/order as the exact oracle; about 9.5x faster than the exact scan on this fixture. |
| `graph_text_bm25_indexed/transient_build_query/n1000_k10` | 456.56 µs (quick) | Build a transient postings index from the graph snapshot, then query it once. Slower than exact for one-off 1k queries; useful as the build-cost envelope and as the bridge toward durable maintained registrations. |

PR-local text-index mixed maintenance rows:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_mixed`;
`scripts/run-benches.sh --profile quick --bench graph_mixed_workload --filter point_read`;
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_mixed/registered_query_update_r60w40/n10000_k10`;
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_mixed/write_registered_update_w40/n10000`;
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_mixed/registered_query_update_r60w40/n100000_k10`.

| Bench | 1k | 10k | 100k | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_mixed/registered_query_update_r60w40` | 7.1944 ms (quick) | 72.334 ms | 764.43 ms | One cycle interleaves 60 maintained BM25 index reads with 40 registered text-property updates. The 1k row had two high severe outliers; the 100k row had one high mild outlier. |
| `graph_text_bm25_mixed/write_registered_update_w40` | 4.9800 ms (quick) | 46.147 ms | 495.04 ms | Write-only companion over the same fixture. The roughly linear scale curve shows text-body churn, not repeated BM25 reads, is the dominant high-scale cost. |

Same-run 1k `graph_mixed_workload` guards measured scalar mixed at 2.1155 ms,
typed property-index mixed at 2.2297 ms, and scalar WAL mixed at 149.76 ms.
Maintained BM25 remains the preferred read path when lexical/current-state
quality is sufficient, but body-update-heavy workloads should treat text-index
maintenance as a first-class write-side bottleneck before adding richer text or
JSON indexing surfaces.

PR-local text-index snapshot-sharing A/B:

Commands: same text-index mixed maintenance rows above, plus
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query`
and
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/transient_build_query`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_mixed/registered_query_update_r60w40/n1000_k10` | 7.1944 ms | 4.0606 ms | -43.6% | Shared document-term and postings vectors avoid deep-cloning most text-index state on each body update commit. |
| `graph_text_bm25_mixed/write_registered_update_w40/n1000` | 4.9800 ms | 1.7686 ms | -64.5% | Isolated update companion shows the write-side maintenance win directly. |
| `graph_text_bm25_mixed/registered_query_update_r60w40/n10000_k10` | 72.334 ms | 31.054 ms | -57.1% | High-scale mixed row remains dominated by text update commits, but the snapshot-sharing change cuts the cycle by more than half. |
| `graph_text_bm25_mixed/write_registered_update_w40/n10000` | 46.147 ms | 6.7589 ms | -85.4% | 10k write-only row becomes close to the scalar/property-index mixed envelope rather than the old full-index clone envelope. |
| `graph_text_bm25_mixed/registered_query_update_r60w40/n100000_k10` | 764.43 ms | 335.88 ms | -56.1% | 100k row still identifies maintained text-body churn as a real write-side cost, but no longer by full postings-index clone per commit. |
| `graph_text_bm25_mixed/write_registered_update_w40/n100000` | 495.04 ms | 62.795 ms | -87.3% | Largest write-side win; one high mild outlier in the accepted row. |
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 34.665 µs | 36.696 µs | +5.9% | Small maintained-read tax from one extra shared-vector dereference. Keep watching this guard because BM25/current-state is a preferred read path. |
| `graph_text_bm25_indexed/transient_build_query/n1000_k10` | 456.56 µs | 533.05 µs | +16.8% | Build/recovery/regeneration pays to convert bulk-built vectors into shared snapshot state. This is accepted for the update win but keeps text-index rebuild/recovery cost as a follow-up benchmark area. |

PR-local BM25 term-count inline storage A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_exact/topic_query`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query`;
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_exact/topic_query/n1000_k10` | 248.85 µs | 248.66 µs | neutral | Exact scan stayed noise-flat because tokenization and row scanning dominate. |
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 36.016 µs | 33.988 µs | -5.55% | Candidate `DocumentStats` now stores the common four-term query counts inline rather than allocating a per-candidate `Vec<u32>`; p=0.00. Full-profile sanity medians after the change: 10k 342.47 µs, 50k 1.7921 ms, 100k 3.6531 ms. |

PR-local indexed BM25 candidate-map preallocation A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed`;
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 34.069 µs | 27.897 µs | -18.10% | Indexed search now gathers query postings once, keeps short-query posting metadata inline, and reserves the per-query candidate map from the capped postings upper bound; p=0.00. Registered-index sanity after the change is 28.054 µs, transient build/query is 476.87 µs, and full-profile indexed-read medians are 10k 282.10 µs, 50k 1.5770 ms, 100k 3.3224 ms. |

PR-local candidate-scoped BM25 query metadata A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_vector_hybrid/graph_topic_bm25_current_scoped`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_vector_hybrid/vector_bm25_current`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_vector_hybrid/ann_bm25_current`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_vector_hybrid/graph_topic_bm25_current_scoped/...q8_c64` | 42.487 µs | 36.467 µs | -13.91% | Candidate-scoped BM25 now uses the same inline query-posting metadata and reserves its indexed-candidate dedup set from the input width; p=0.00. |
| `graph_text_bm25_vector_hybrid/graph_topic_bm25_current_scoped_vector_rerank/...q8_c64` | 61.235 µs | 55.042 µs | -10.76% | The downstream vector rerank row keeps the BM25 candidate win while preserving the same selected-candidate shape; p=0.00. Sanity after the change: vector-BM25 current filter 210.98 µs, ANN-BM25 current filter 54.834 µs, and indexed prebuilt topic query 28.446 µs. |

PR-local candidate-scoped BM25 canonical-input A/B:

Command:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query_candidates`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_sorted/n1000_k10` | 41.831 µs | 34.770 µs | -16.9% | Candidate-scoped indexed BM25 over already-canonical node ids now skips the `FxHashSet` dedup allocation and scores the ascending slice directly; p=0.00. Full-profile post-change sanity: 10k 547.46 µs, 50k 3.5299 ms, 100k 7.3916 ms. |
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_reverse/n1000_k10` | 41.024 µs | 38.948 µs | -7.0% | Reverse candidate input stays on the existing hash-dedup path; the shared scoring loop trims a smaller but significant amount of overhead while preserving duplicate handling; p=0.00. Full-profile post-change sanity: 10k 638.60 µs, 50k 4.6077 ms, 100k 10.978 ms. |

Rejected variants: sharing postings while leaving per-document term lists as
plain `Vec<String>` kept transient build lower at 523.57 µs but lost the update
win (`write_registered_update_w40/n1000` returned to 5.0114 ms). Wrapping
document terms as `Arc<Vec<String>>` instead of `Arc<[String]>` worsened the
transient build row to 580.25 µs, so the accepted representation uses
`Arc<[String]>` document terms plus a bulk builder.

PR-local text-index rebuild and bulk-append builder A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_rebuild`;
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_rebuild/create_registered_index/n10000`;
`scripts/run-benches.sh --profile full --bench text_search_bm25 --filter graph_text_bm25_rebuild/compact_registered_after_delete/n10000`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/transient_build_query`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_rebuild/create_registered_index/n1000` | 491.64 µs | 450.07 µs | -8.5% | Registers and builds a maintained text index over a seeded graph. The bulk builder now appends postings and relies on the final sort instead of binary-searching each posting vector during construction. |
| `graph_text_bm25_rebuild/create_registered_index/n10000` | 5.1203 ms | 4.3962 ms | -14.1% | Same registration/build row at 10k; Criterion reports a significant improvement (`p=0.00`). |
| `graph_text_bm25_rebuild/create_registered_index/n100000` | 59.798 ms | 49.751 ms | -16.8% | 100k build/regeneration cost remains material but is no longer inflated by per-posting insertion search. |
| `graph_text_bm25_rebuild/compact_registered_after_delete/n1000_del100` | 571.97 µs | 533.23 µs | -6.8% | Compacts after deleting 10% of registered text documents, exercising the production rebuild chain from live primary rows. |
| `graph_text_bm25_rebuild/compact_registered_after_delete/n10000_del1000` | 6.7179 ms | 5.8964 ms | -12.2% | 10k compaction/rebuild improves while preserving the same reclaimed-node assertions. |
| `graph_text_bm25_rebuild/compact_registered_after_delete/n100000_del10000` | 78.225 ms | 69.793 ms | -10.8% | 100k compaction/rebuild remains a maintenance scheduling concern, but the builder change takes about 8.4 ms off this row. |
| `graph_text_bm25_indexed/transient_build_query/n1000_k10` | 533.05 µs | 487.80 µs | -8.5% | Recovers part of the snapshot-sharing build tax; still above the pre-sharing 456.56 µs row, so rebuild/recovery stays visible. |
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 36.696 µs | 36.528 µs | noise | Query path is unchanged by the bulk-builder append path. |

PR-local text-index builder document-map preallocation A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_rebuild`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_rebuild/create_registered_index/n1000` | 447.00 µs | 437.54 µs | -2.12% | Bulk text-index builds now reserve document-length and document-term maps from the label-row cardinality, then shrink at finish so sparse labels do not keep over-reserved document-map capacity. |
| `graph_text_bm25_rebuild/compact_registered_after_delete/n1000_del100` | 528.47 µs | 521.75 µs | -1.27% | The same builder path trims a small amount from compaction's text-index rebuild stage without changing postings order or scoring. Full-profile sanity after the change: create 10k 4.3152 ms, 50k 23.728 ms, 100k 48.806 ms; compact 10k 5.8408 ms, 50k 33.185 ms, 100k 69.183 ms. Indexed sanity: transient build/query 470.42 µs, prebuilt query 28.289 µs, registered query 28.183 µs. |

PR-local text-index builder finalization A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_rebuild`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/transient_build_query`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_rebuild/create_registered_index/n1000` | 361.14 µs | 348.10 µs | -3.7945% | Bulk builds now intern new terms against the postings table and store final `Arc<[TextTerm]>` document-term lists during insertion, removing the temporary intern table and the finish-time document-term map rebuild. Criterion reports p=0.00. |
| `graph_text_bm25_rebuild/compact_registered_after_delete/n1000_del100` | 440.78 µs | 427.36 µs | noise | Compaction's text-index rebuild consumer remains statistically flat (`p=0.31`) while sharing the same builder path. |
| `graph_text_bm25_indexed/transient_build_query/n1000_k10` | 387.59 µs | 378.70 µs | -2.1557% | The one-off transient build/query row also benefits from the builder-finalization shortcut; Criterion reports p=0.00. |

PR-local BM25 top-k replacement A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10 --save-baseline text_topk_peek_mut_pre`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10 --baseline text_topk_peek_mut_pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 28.424 µs | 27.752 µs | -2.0122% | `TextTopK` now replaces the retained worst hit through `BinaryHeap::peek_mut()` instead of `pop()` plus `push()` when a candidate beats the current worst; Criterion reports p=0.00. |

PR-local BM25 top-k heap preallocation A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter topic_query --save-baseline bm25_query_terms_pre`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter topic_query --baseline bm25_query_terms_pre`;
focused checked-row rerun:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter topic_query_checked_with_deadline --baseline bm25_query_terms_pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_exact/topic_query/n1000_k10` | 132.28 µs | 130.13 µs | -1.6820% | `TextTopK` now reserves the known `k` slots up front with `BinaryHeap::with_capacity(k)`; Criterion reports p=0.00. |
| `graph_text_bm25_exact/topic_query_checked_with_deadline/n1000_k10` | 131.48 µs | 130.35 µs | -2.7186% | Focused rerun after a noisy broad-run sample; same checked scan path, Criterion reports p=0.00. |
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 28.887 µs | 28.687 µs | neutral | Maintained read path stayed statistically flat. |
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_sorted/n1000_k10` | 30.646 µs | 30.465 µs | neutral | Full-cover sorted candidate guard stayed statistically flat. |
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_reverse/n1000_k10` | 31.805 µs | 30.648 µs | -5.9149% | Reverse candidate input benefits from avoiding heap growth in the delegated scorer; Criterion reports p=0.05. |
| `graph_text_bm25_indexed/registered_topic_query/n1000_k10` | 29.520 µs | 28.835 µs | neutral | Registered-index read path stayed statistically flat. |

PR-local partial BM25 candidate cursor A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query_candidates_partial --save-baseline bm25-partial-candidates-pre`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query_candidates_partial --baseline bm25-partial-candidates-pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_partial_sorted/n1000_k10` | 7.6817 µs | 5.7940 µs | -25.063% | Partial sorted candidates now advance monotonic cursors through each query-term postings list instead of binary-searching every postings list per candidate; Criterion reports p=0.00. |
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_partial_reverse/n1000_k10` | 9.7642 µs | 9.9943 µs | neutral | Reverse partial candidates stay on the hash-dedup path and were not the target; the clean rerun stayed within Criterion's noise threshold. |

PR-local text-index update-maintenance candidate-key A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_mixed --save-baseline text_index_update_maint_pre`;
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_mixed --baseline text_index_update_maint_pre`;
read-path sanity:
`scripts/run-benches.sh --profile quick --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_mixed/registered_query_update_r60w40/n1000_k10` | 3.5261 ms | 3.3854 ms | -3.7434% | Text-index update maintenance now collects touched labels/properties in inline borrowed storage and mutates the matched entry directly instead of materializing a `BTreeSet` of owned candidate keys and re-looking up each key. Criterion reports p=0.00. |
| `graph_text_bm25_mixed/write_registered_update_w40/n1000` | 1.6797 ms | 1.6825 ms | noise | The write-only companion stayed statistically neutral (`p=0.66`), so the accepted win is the mixed read/write cycle's maintenance overhead reduction. Indexed-read sanity after the change: `prebuilt_topic_query/n1000_k10` 29.815 µs. |

PR-local BM25 ASCII tokenizer A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench text_search_bm25 --filter graph_text_bm25_exact/topic_query --save-baseline bm25-tokenizer-ascii-pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query --save-baseline bm25-tokenizer-ascii-pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench text_search_bm25 --filter graph_text_bm25_exact/topic_query --baseline bm25-tokenizer-ascii-pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench text_search_bm25 --filter graph_text_bm25_indexed/prebuilt_topic_query --baseline bm25-tokenizer-ascii-pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_exact/topic_query/n1000_k10` | 252.62 µs | 177.61 µs | -29.433% | All-ASCII documents now take a byte-scanning tokenizer path while preserving the original Unicode lowercase path for mixed input. Criterion reports p=0.00. |
| `graph_text_bm25_exact/topic_query_checked_with_deadline/n1000_k10` | 252.06 µs | 178.23 µs | -29.765% | The cancellation-aware exact oracle keeps the same improvement because tokenization dominates the 1k ASCII fixture scan. Criterion reports p=0.00. |
| `graph_text_bm25_indexed/prebuilt_topic_query/n1000_k10` | 28.116 µs | 27.967 µs | noise | Guard row for the maintained read path; Criterion reported -0.5120%, inside the noise threshold. |
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_sorted/n1000_k10` | 31.103 µs | 29.155 µs | -5.1952% | Candidate-scoped indexed BM25 reuses the same query tokenizer path, and the sorted full-cover row improves without changing scoring order. Criterion reports p=0.00. |
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_reverse/n1000_k10` | 32.414 µs | 30.362 µs | -5.6244% | Reverse candidate input stays on the existing dedup path while benefiting from the cheaper ASCII query tokenization. Criterion reports p=0.00. |

PR-local exact BM25 query-term matching A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench text_search_bm25 --filter graph_text_bm25_exact/topic_query --save-baseline bm25-query-match-pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench text_search_bm25 --filter graph_text_bm25_exact/topic_query --baseline bm25-query-match-pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_text_bm25_exact/topic_query/n1000_k10` | 174.24 µs | 128.71 µs | -27.776% | Exact scan now matches each document token against the short deduplicated query-term list directly instead of binary-searching four terms per token, while longer query-term lists keep the binary-search path. Criterion reports p=0.00. |
| `graph_text_bm25_exact/topic_query_checked_with_deadline/n1000_k10` | 180.55 µs | 131.33 µs | -26.222% | The checked exact oracle sees the same per-token matching win while preserving cancellation checks around the scan. Criterion reports p=0.00. |

PR-local quick JSON baseline:

| Bench | 1k | Notes |
|---|---:|---|
| `graph_json_contains_scan/nested_metadata_k10/1000` | 21.731 µs (quick) | Exact scan over 1,000 JSON metadata payloads with one-quarter matching nested current episodic facts, skipping non-JSON properties. This is the oracle for future maintained JSON/path indexes and JSON/vector/text candidate composition. |
| `graph_json_path_exists_scan/nested_score_path_k10/1000` | 17.559 µs (quick) | Exact scan over 1,000 JSON metadata payloads for selector path `["memory","score"]`, skipping non-JSON properties. This is the oracle for path-existence candidate production before maintained JSON/path indexes. |
| `graph_json_path_exists_scan/nested_score_path_candidates_reverse_k10/1000` | 1.0303 µs (quick) | Candidate-scoped path-existence over 1,000 reverse-sorted node ids. Latest PR-local lazy hit-reserve A/B: 1.0426 µs → 1.0303 µs, within noise but not regressing the sort/dedup path. Earlier canonical-candidate guard: 1.1235 µs → 1.1287 µs. |
| `graph_json_path_exists_scan/nested_score_path_candidates_sorted_k10/1000` | 638.84 ns (quick) | Candidate-scoped path-existence over 1,000 already-canonical node ids. Latest PR-local lazy hit-reserve A/B: 649.16 ns → 638.84 ns by reserving result storage on the first actual hit. Earlier canonical-candidate fast path: 972.19 ns → 727.37 ns by skipping redundant sort/dedup work. |
| `graph_json_path_contains_scan/nested_memory_path_k10/1000` | 19.263 µs (quick) | Exact scan over 1,000 JSON metadata payloads for selector path `["memory"]`, applying recursive containment to the selected subvalue. This is the oracle for path-scoped JSON containment before maintained JSON/path indexes. |
| `graph_json_path_value_scan/nested_score_path_k10/1000` | 22.855 µs (quick) | Exact scan over 1,000 JSON metadata payloads for selector path `["memory","score"]`, returning node ids plus selected JSON values. This measures the candidate-plus-value path before maintained JSON/path indexes. |

PR-local full JSON Rayon A/B:

Command: `scripts/run-benches.sh --profile full --bench single_graph --filter graph_json`

| Bench | 10k sequential -> post | 50k sequential -> Rayon | 100k sequential -> Rayon | Notes |
|---|---:|---:|---:|---|
| `graph_json_contains_scan/nested_metadata_k10` | 210.13 µs -> 235.38 µs | 1.7982 ms -> 994.45 µs | 4.6986 ms -> 2.3977 ms | 10k stays below the 16,384-row Rayon threshold; 50k/100k improve about 1.8x/2.0x. |
| `graph_json_path_exists_scan/nested_score_path_k10` | 189.31 µs -> 203.56 µs | 2.9875 ms -> 949.97 µs | 9.4831 ms -> 2.0903 ms | Large path-existence scans are the strongest win, about 3.1x at 50k and 4.5x at 100k. |
| `graph_json_path_contains_scan/nested_memory_path_k10` | 185.37 µs -> 181.71 µs | 2.6000 ms -> 952.68 µs | 6.7178 ms -> 2.1109 ms | Path-scoped containment improves about 2.7x at 50k and 3.2x at 100k. |
| `graph_json_path_value_scan/nested_score_path_k10` | 234.60 µs -> 264.70 µs | 2.3864 ms -> 1.1162 ms | 5.9315 ms -> 2.4776 ms | Pre-B21 path-value scans still cloned selected JSON values before top-k admission; Rayon improved large rows about 2.1x/2.4x. |

PR-local B21 JSON path-value borrowed-selector A/B:

Command: `scripts/run-benches.sh --profile full --bench single_graph --filter graph_json_path_value_scan`

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_json_path_value_scan/nested_score_path_k10/10000` | 214.42 µs | 164.50 µs | -23.5% | Selected JSON values are now borrowed during the scan and cloned only when admitted to the top-k heap. |
| `graph_json_path_value_scan/nested_score_path_k10_checked_with_deadline/10000` | 211.19 µs | 165.07 µs | -22.2% | Deadline-bearing calls keep the B10 chunked path and avoid rejected-candidate JSON clones. |
| `graph_json_path_value_scan/nested_score_path_k10/50000` | 1.0821 ms | 821.05 µs | -32.3% | Larger row set shows the clone removal compounding with the existing Rayon scan. |
| `graph_json_path_value_scan/nested_score_path_k10_checked_with_deadline/50000` | 1.0580 ms | 824.72 µs | -20.1% | Deadline row remains parallel and avoids pre-admission value materialization. |
| `graph_json_path_value_scan/nested_score_path_k10/100000` | 2.2521 ms | 1.9638 ms | -12.5% | 100k unchecked row still improves, though less dramatically than 50k on this run. |
| `graph_json_path_value_scan/nested_score_path_k10_checked_with_deadline/100000` | 2.5734 ms | 1.9746 ms | -29.3% | 100k deadline row drops back near the unchecked row once selected-value clones move behind top-k admission. |

PR-local JSON top-k heap preallocation A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_json --save-baseline json_topk_prealloc_pre`;
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_json --baseline json_topk_prealloc_pre`;
focused path-exists rerun:
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_json_path_exists_scan/nested_score_path_k10/1000 --baseline json_topk_prealloc_pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_json_contains_scan/nested_metadata_k10/1000` | 21.434 µs | 21.136 µs | -3.2938% | JSON top-k helpers now reserve the known `k` slots up front with `BinaryHeap::with_capacity(k)`; Criterion reports p=0.00. |
| `graph_json_contains_scan/nested_metadata_k10_checked_with_deadline/1000` | 23.200 µs | 22.541 µs | -3.2981% | Deadline-bearing containment row sees the same allocation shape; Criterion reports p=0.01. |
| `graph_json_path_exists_scan/nested_score_path_k10/1000` | 18.090 µs | 17.557 µs | -4.7722% | Focused rerun after a noisy broad-run sample; Criterion reports p=0.00. |
| `graph_json_path_exists_scan/nested_score_path_k10_checked_with_deadline/1000` | 18.799 µs | 18.245 µs | neutral | Checked path-exists row stayed statistically flat in the broad run. |
| `graph_json_path_exists_scan/nested_score_path_candidates_sorted_k10/1000` | 755.19 ns | 779.27 ns | neutral | Candidate-scoped sorted guard stayed statistically flat. |
| `graph_json_path_exists_scan/nested_score_path_candidates_reverse_k10/1000` | 1.0706 µs | 1.1128 µs | neutral | Candidate-scoped reverse guard stayed statistically flat. |
| `graph_json_path_contains_scan/nested_memory_path_k10/1000` | 19.960 µs | 20.605 µs | neutral | Path-containment row stayed statistically flat. |
| `graph_json_path_value_scan/nested_score_path_k10/1000` | 17.859 µs | 18.801 µs | neutral | Path-value row stayed statistically flat. |

PR-local JSON candidate borrowed-slice A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_json_path_exists_scan/nested_score_path_candidates --save-baseline json-candidate-borrow-pre`;
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_json_path_exists_scan/nested_score_path_candidates --baseline json-candidate-borrow-pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_json_path_exists_scan/nested_score_path_candidates_sorted_k10/1000` | 726.11 ns | 650.17 ns | -10.271% | Candidate-scoped JSON search now borrows already-canonical candidate slices instead of cloning them into a temporary `Vec`; Criterion reports p=0.00. |
| `graph_json_path_exists_scan/nested_score_path_candidates_reverse_k10/1000` | 1.0641 µs | 1.0960 µs | neutral | Unsorted inputs still take the owned sort/dedup path and stayed within noise (`p=0.20`). |

PR-local JSON candidate hit-reserve A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench single_graph --filter graph_json_path_exists_scan/nested_score_path_candidates --save-baseline json-candidate-hit-prealloc-before`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench single_graph --filter graph_json_path_exists_scan/nested_score_path_candidates --baseline json-candidate-hit-prealloc-before`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_json_path_exists_scan/nested_score_path_candidates_sorted_k10/1000` | 649.16 ns | 638.84 ns | -1.7684% | Candidate-scoped JSON filtering now reserves bounded result storage on the first actual hit, avoiding eager allocation for zero-hit queries while removing repeated growth for common top-k hits. Criterion reports p=0.00. |
| `graph_json_path_exists_scan/nested_score_path_candidates_reverse_k10/1000` | 1.0426 µs | 1.0303 µs | neutral | Reverse candidates still take the owned sort/dedup path; Criterion reports the median lower but within the noise threshold. |

PR-local B24 batch exact-vector scan Rayon A/B:

Commands:
`scripts/run-benches.sh --profile full --bench single_graph --filter graph_exact_vector_batch_scan --save-baseline b24_serial_batch`
on the pre-change serial branch, then
`scripts/run-benches.sh --profile full --bench single_graph --filter graph_exact_vector_batch_scan --baseline b24_serial_batch`
on the B24-par branch.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_exact_vector_batch_scan/unindexed_squared_euclidean_q8_dim128_k10/10000` | 1.6124 ms | 1.5897 ms | Below the 16,384-row parallel threshold; remains effectively serial and unchanged. |
| `graph_exact_vector_batch_scan/unindexed_squared_euclidean_q8_dim128_k10_checked_with_deadline/10000` | 1.5764 ms | 1.5975 ms | Deadline checker overhead stays noise-scale below the threshold. |
| `graph_exact_vector_batch_scan/unindexed_squared_euclidean_q8_dim128_k10/50000` | 20.809 ms | 1.8056 ms | Batch exact scan now uses the shared chunked Rayon reducer above the row threshold. |
| `graph_exact_vector_batch_scan/unindexed_squared_euclidean_q8_dim128_k10_checked_with_deadline/50000` | 20.511 ms | 1.8283 ms | Deadline-bearing batch calls keep the parallel path instead of reverting to serial. |
| `graph_exact_vector_batch_scan/unindexed_squared_euclidean_q8_dim128_k10/100000` | 15.673 ms | 3.6915 ms | Large unindexed q8 scan improves about 4.2x on this branch run. |
| `graph_exact_vector_batch_scan/unindexed_squared_euclidean_q8_dim128_k10_checked_with_deadline/100000` | 15.811 ms | 3.7289 ms | Deadline row tracks the unchecked large-row path. |
| `graph_exact_vector_batch_scan/flat_index_squared_euclidean_q8_dim128_k10/50000` | 19.617 ms | 1.8456 ms | Flat-index row-set scans share the same batch chunk reducer once the index row bitmap is broad. |
| `graph_exact_vector_batch_scan/flat_index_squared_euclidean_q8_dim128_k10_checked_with_deadline/50000` | 8.6977 ms | 1.9225 ms | Baseline flat/deadline row was noisy but still improves materially after parallelization. |
| `graph_exact_vector_batch_scan/flat_index_squared_euclidean_q8_dim128_k10/100000` | 41.284 ms | 3.7879 ms | Broad flat-index batch scan improves about 10.9x on this branch run. |
| `graph_exact_vector_batch_scan/flat_index_squared_euclidean_q8_dim128_k10_checked_with_deadline/100000` | 38.925 ms | 3.7937 ms | Deadline flat-index row remains on the parallel path. |

PR-local quick exact batch cosine candidate-norm A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench single_graph --filter cosine_q8_dim128_k10 --vector-scales 10000,50000 --save-baseline vector-batch-cosine-norm-pre`,
then the same command with `--baseline vector-batch-cosine-norm-pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_exact_vector_batch_scan/unindexed_cosine_q8_dim128_k10/10000` | 2.1250 ms | 1.7721 ms | -16.36% | Reuses one candidate squared norm across the eight bound cosine queries. |
| `graph_exact_vector_batch_scan/flat_index_cosine_q8_dim128_k10/10000` | 2.1286 ms | 1.7774 ms | -18.03% | Flat-index row-set scan keeps the same metric shortcut. |
| `graph_exact_vector_batch_scan/unindexed_cosine_q8_dim128_k10/50000` | 2.3514 ms | 2.1747 ms | -6.72% | Above the parallel threshold, chunked row scans still benefit from the per-candidate norm reuse. |
| `graph_exact_vector_batch_scan/flat_index_cosine_q8_dim128_k10/50000` | 2.3971 ms | 2.2063 ms | -8.39% | Broad flat-index q8 cosine scan stays statistically faster (`p=0.00`). |

Rejected variant: applying the same separate candidate-norm pass to
`graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64` regressed
c64/c256/c1024 rows by +31.84%/+38.48%/+28.38% (`p=0.00`), with c4096 neutral,
so the candidate-set scorer keeps the existing fused per-query cosine pass.

PR-local vector candidate-set disjoint intersection A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_vector_candidate_set/set_intersection --save-baseline vector-disjoint-intersection-pre`;
`scripts/run-benches.sh --profile quick --bench single_graph --filter graph_vector_candidate_set/set_intersection --baseline vector-disjoint-intersection-pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_vector_candidate_set/set_intersection_l256_r256_o0/0` | 135.45 ns | 1.3378 ns | -99.015% | Equal-width sorted ranges that cannot overlap now return an empty candidate set before allocation or merge scanning. |
| `graph_vector_candidate_set/set_intersection_l256_r256_o128/128` | 160.05 ns | 145.66 ns | -9.2900% | Balanced overlap stayed faster in the accepted rerun, but the optimization targets the disjoint row. |
| `graph_vector_candidate_set/set_intersection_l8_r1024_o8/8` | 29.285 ns | 28.730 ns | neutral | The tiny-vs-large probe path stays on the existing binary-search branch; Criterion reported no change (`p=0.58`). |

PR-local quick vector baseline:

| Bench | 1k | Notes |
|---|---:|---|
| `graph_exact_vector_scan/squared_euclidean_dim128_k10` | 22.9 µs unindexed / 24.3 µs flat (quick) | Exhaustive label-filtered scan over 1,000 vector nodes; safe `f64x4` L2-squared accumulation; flat 20k row: ~244 µs. |
| `graph_exact_vector_scan/cosine_dim128_k10` | 33.5 µs unindexed / 33.6 µs flat (quick) | Exhaustive label-filtered scan over 1,000 vector nodes; safe `f64x4` cosine accumulation; flat 20k row: ~276 µs. |
| `graph_exact_vector_batch_scan/cosine_q8_dim128_k10` | 1.772 ms unindexed / 1.777 ms flat at 10k; 2.175 ms unindexed / 2.206 ms flat at 50k (quick) | Scores eight 128-dim cosine queries over the same exact row set and reuses each candidate squared norm across the batch. |
| `graph_vector_candidate_set/neighbor_candidates_depends_on_k64` | 233.8 ns (quick) | Derives a sorted/deduplicated 64-node candidate set from one anchor's outgoing `DEPENDS_ON` adjacency. This measures the reusable Rust candidate-set boundary, not vector scoring. |
| `graph_vector_candidate_set/adjacency_label_range_l8_k64` | 44.6 ns (quick) | Iterates the sorted label range for 64 matching edges mixed with 8x64 unrelated-label edges. |
| `graph_vector_candidate_set/adjacency_label_scan_l8_k64` | 374.8 ns (quick) | Benchmark-local old path: scans the same mixed-label adjacency entry and filters by label, showing the range lookup is ~8.4x faster for high-degree mixed-label candidates. |
| `graph_vector_candidate_set/score_candidate_set_cosine_c64/c256/c1024/c4096_d1024` | 13.7 µs / 54.7 µs / 222.6 µs / 308.3 µs (quick) | Scores canonical candidate sets against one 1024-dim cosine query. Widths below 4,096 stay sequential; the 4,096-row uses chunked Rayon and is the production broad-candidate rerank threshold. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q8_c64/c256/c1024/c4096_d1024` | 97.5 µs / 382.0 µs / 428.5 µs / 1.135 ms (quick) | Scores 8 canonical candidate sets against 8 1024-dim cosine queries. Repeated c64/c256 sets reuse candidate property lookups below the batch threshold; c1024 uses query-level Rayon; c4096 uses candidate-major parallel scoring. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c64/c256/c1024/c4096_d1024` | 177.1 µs / 604.2 µs / 2.270 ms / 8.461 ms (quick) | Scores 64 canonical candidate sets against 64 queries. Repeated c64/c256/c1024 sets stay on query-level Rayon; c4096 switches to candidate-major parallel scoring to reuse property lookups across queries. |
| `graph_vector_candidate_set/score_nodes_batch_cosine_q8_c64/c256/c1024/c4096_d1024` | 102.3 µs / 402.4 µs / 423.1 µs / 1.500 ms (quick) | Scores 8 explicit node-slice candidate sets through the generic API. It now normalizes to canonical sets and follows the same q8 threshold behavior. |
| `graph_vector_candidate_set/score_nodes_batch_cosine_q64_c64/c256/c1024/c4096_d1024` | 185.3 µs / 631.1 µs / 2.351 ms / 9.561 ms (quick) | Scores 64 explicit node-slice candidate sets through the generic API. The normalization cost is small relative to the query-level batch win. |
| `graph_vector_candidate_set/score_neighbors_batch_cosine_q8_c64/c256/c1024/c4096_d1024` | 102.2 µs / 404.3 µs / 435.7 µs / 1.529 ms (quick) | Scores 8 graph-neighbor candidate sets through the neighbor batch API. The repeated-anchor fixture reuses one derived candidate set before canonical batch scoring. |
| `graph_vector_candidate_set/score_neighbors_batch_cosine_q64_c64/c256/c1024/c4096_d1024` | 181.8 µs / 609.8 µs / 2.315 ms / 9.430 ms (quick) | Scores 64 graph-neighbor candidate sets. Repeated-anchor reuse helps c64/c256; wider rows are dominated by reranking. |
| `graph_vector_candidate_set/score_expanded_batch_cosine_q8_c64/c256/c1024/c4096_d1024` | 103.0 µs / 407.5 µs / 437.2 µs / 1.557 ms (quick) | Scores 8 graph-expanded root sets through the expanded batch API. This stays below the 16-set expansion parallel threshold. |
| `graph_vector_candidate_set/score_expanded_batch_cosine_q64_c64/c256/c1024/c4096_d1024` | 189.1 µs / 624.6 µs / 2.304 ms / 9.348 ms (quick) | Scores 64 graph-expanded root sets. The sampled expanded-work gate leaves c64 serial and parallelizes wider expanded batches. |
| `graph_vector_candidate_state/maintained_active_c512_total1024` | 343.9 ns (quick) | Materializes a provider-maintained 512-node current set from a 1,024-node fixture with stale nodes disqualified by `SUPERSEDED_BY`. |
| `graph_vector_candidate_state/dynamic_active_scan_c512_total1024` | 12.79 µs (quick) | Benchmark-local query-time baseline: scans all 1,024 document nodes and checks outgoing `SUPERSEDED_BY`, showing maintained state is ~37x faster for this currentness slice. |
| `graph_vector_candidate_set/set_intersection_l256_r256_o128` | 153.2 ns (quick) | Intersects two canonical 256-node sets with 128 overlapping ids using the merge path; this is the balanced graph/ANN/active-set composition primitive. |
| `graph_vector_candidate_set/set_intersection_l256_r256_o0` | 1.3378 ns (quick) | Equal-width disjoint canonical ranges return an empty candidate set before allocation or merge scanning. PR-local A/B: 135.45 ns → 1.3378 ns. |
| `graph_vector_candidate_set/set_intersection_l8_r1024_o8` | 31.10 ns (quick) | Intersects a tiny dependency-style set with a much larger maintained active set using the binary-search probe path. |
| `graph_vector_candidate_set/set_union_l256_r256_o128` | 170.6 ns (quick) | Unions two canonical 256-node sets into a 384-node canonical candidate set. |
| `graph_vector_candidate_set/set_union_l256_r256_o0` | 166.60 ns (quick) | Unions two disjoint canonical 256-node ranges into a 512-node candidate set; this keeps disjoint append opportunities visible beside the overlap row. |
| `graph_vector_candidate_set/set_difference_l256_r256_o128` | 178.5 ns (quick) | Computes the graph-side exclusion path for two canonical 256-node sets with 128 overlapping ids. |
| `graph_vector_candidate_set/set_difference_l256_r256_o0` | 149.79 ns (quick) | Computes graph-side exclusion when the right-hand canonical 256-node range is fully above the left-hand range. |
| `graph_vector_candidate_set/from_nodes_reverse_l256_r256_o128` | 186.33 ns (quick) | Builds a canonical candidate set from 256 reverse-sorted node ids. PR-local canonical-input guard A/B: 199.68 ns → 186.33 ns by using an early-exit ascending check before the existing sort/dedup path. |
| `graph_vector_candidate_set/from_nodes_sorted_l256_r256_o128` | 108.15 ns (quick) | Builds a canonical candidate set from 256 already-canonical node ids. PR-local canonical-input fast path A/B: 190.68 ns → 108.15 ns by skipping redundant sort/dedup work. |
| `graph_vector_candidate_set/from_search_hits_l256_r256_o128` | 179.8 ns (quick) | Builds a canonical candidate set from 256 vector-search hits, covering ANN/search-output composition. |
| `graph_vector_index_rebuild/hnsw_l2_dim128_default` | 118.9 ms (quick) | Rebuilds a 128-dim HNSW L2 index after 10% vector updates + 5% deletes; compact level-0 links preserve the same link counts while reclaiming 150 stale HNSW entries. |
| `graph_vector_index_rebuild/hnsw_l2_dim128_m24ef64` | 200.7 ms (quick) | Tuned `M=24, ef_construction=64` rebuild row; keeps the high-recall research config covered with compacted post-rebuild level-0 links. |
| `graph_vector_index_rebuild/hnsw_cos_dim128_default` | 146.1 ms (quick) | Same rebuild fixture for 128-dim HNSW cosine, covering construction-side scorer reuse for metrics with bound query state. |
| `graph_vector_index_rebuild/hnsw_cos_dim128_m24ef64` | 247.1 ms (quick) | Tuned cosine rebuild row; link counts and recall shape are unchanged, but level-0 storage compacts after rebuild. |
| `graph_vector_index_rebuild/ivf_l2_dim128` | 2.108 ms (quick) | IVF rebuild row for the same 1k / 10% update / 5% delete fixture; replacements reuse IVF entries, so the suffix now reclaims only 50 delete-stale entries (`b1k-950-50`). |
| `graph_vector_index_rebuild/ivf_cos_dim128` | 2.124 ms (quick) | IVF cosine rebuild row with replacement reuse; bound cosine scorer cost is now mostly hidden by deterministic centroid retraining at this scale. |
| `graph_vector_index_ivf_target_centroid_rebuild/ivf_cos_dim128_default` | 1.704 ms at 1k / 11.25 ms at 10k (quick) | Default IVF target-list rebuild baseline for the 10% update / 5% delete fixture; suffixes reclaim 50 stale entries at 1k and 500 at 10k. |
| `graph_vector_index_ivf_target_centroid_rebuild/ivf_cos_dim128_c16` | 950.5 µs at 1k / 3.267 ms at 10k (quick) | Coarse 16-list rebuild row; cheapest retrain/reassignment cost, but read-side target-centroid pressure showed coarse lists can hurt 10k candidate fanout. |
| `graph_vector_index_ivf_target_centroid_rebuild/ivf_cos_dim128_c128` | 6.625 ms at 1k / 13.08 ms at 10k (quick) | Explicit 128-list rebuild row; near default cost at 10k, but much more expensive than coarse lists at 1k. |
| `graph_vector_index_ivf_target_centroid_rebuild/ivf_cos_dim128_c512` | 26.23 ms at 1k / 46.46 ms at 10k (quick) | Over-wide 512-list rebuild row; matches the read-side finding that very high list counts add cost before improving this fixture. |
| `graph_vector_index_recommended_rebuild/ivf_l2_dim128_recommended` | 2.256 ms at 1k / 12.92 ms at 10k (quick) | Multi-index IVF fixture with 4 registered indexes and one hot index above the rebuild threshold. Recommended maintenance rebuilds only the hot index (`idx4_rb1`). |
| `graph_vector_index_recommended_rebuild/ivf_l2_dim128_full` | 8.022 ms at 1k / 46.68 ms at 10k (quick) | Same fixture with full rebuild (`idx4_rb4`), grounding the avoided cold-index rebuild cost for maintenance orchestration. |
| `graph_vector_index_recommended_rebuild/ivf_cos_dim128_recommended` | 2.131 ms at 1k / 12.63 ms at 10k (quick) | Cosine variant of the recommended-only maintenance row. |
| `graph_vector_index_recommended_rebuild/ivf_cos_dim128_full` | 7.595 ms at 1k / 45.64 ms at 10k (quick) | Cosine full-rebuild comparison for the same 4-index fixture. |
| `graph_vector_index_stale_query/hnsw_l2_dim128_default` | 11.24 µs stale / 10.92 µs rebuilt (quick) | 1k fixture after 10% updates + 5% deletes. Stale overlay/mutable state reports `m478-1028`; rebuild compacts to `m212-687`. |
| `graph_vector_index_stale_query/hnsw_cos_dim128_default` | 12.12 µs stale / 12.59 µs rebuilt (quick) | Same churn shape under cosine. On this small fixture, rebuild is still a memory-control operation more than a strict query-latency win. |
| `graph_vector_index_stale_query/hnsw_l2_dim128_m24ef64` | 14.02 µs stale / 13.85 µs rebuilt (quick) | Tuned `M=24, ef_construction=64`; stale `m578-*` compacts to rebuilt `m258-*` while latency stays effectively neutral. |
| `graph_vector_index_stale_query/hnsw_cos_dim128_m24ef64` | 14.90 µs stale / 14.93 µs rebuilt (quick) | Tuned cosine row; memory compaction is visible, latency delta is noise-scale. |
| `graph_vector_index_stale_query/ivf_l2_dim128` | 19.30 µs stale / 19.69 µs rebuilt (quick) | IVF L2 probes 64 lists on the 1k fixture; replacement reuse plus delete-unlinking keeps assigned entries live-only while storage holes still compact from `ve1kl950d50_m46-562` to `ve950l950d0_m47-537`. |
| `graph_vector_index_stale_query/ivf_cos_dim128` | 18.97 µs stale / 19.43 µs rebuilt (quick) | IVF cosine query row; update churn no longer inflates stale candidate checks, and deleted entries are unlinked from probed lists before rebuild storage compaction. |
| `graph_vector_index_dimension_projection/hnsw_l2_default_dim128` | 10.98 µs (quick) | 1k HNSW L2 query row with suffix `m221-721`: ~221 KiB index-owned bytes and ~721 KiB reachable bytes after compact level-0 storage. |
| `graph_vector_index_dimension_projection/hnsw_l2_default_dim768` | 42.34 µs (quick) | Same HNSW topology/link count as dim128; reachable bytes rise to ~3.15 MiB because full-precision vector components dominate. |
| `graph_vector_index_dimension_projection/hnsw_l2_default_dim1536` | 81.01 µs (quick) | Reachable bytes rise to ~6.08 MiB at 1k vectors; extrapolation pressure is raw vector storage, not graph-link storage. |

PR-local PQ candidate compression spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_pq_candidate_recall/cluster_l2/m16_k16_c64_d128_k10_recallbp2250_m1570-full50000` | 8.75 ms (quick) | Benchmark-only product quantization over 100k 128-dim vectors and 16 queries. The compressed codebook+codes footprint is ~1.53 MiB vs ~48.8 MiB full vectors, but 64 candidates is too narrow for standalone recall. |
| `graph_pq_candidate_recall/cluster_l2/m16_k16_c256_d128_k10_recallbp9062_m1570-full50000` | 9.62 ms (quick) | Same compression footprint; widening exact rerank to 256 candidates restores most top-k overlap while staying under 10 ms for the 16-query batch. |
| `graph_pq_candidate_recall/cluster_l2/m16_k16_c1024_d128_k10_recallbp10000_m1570-full50000` | 12.60 ms (quick) | High-recall anchor: 1024 rerank candidates reaches 10000 bp on this corpus, but exact rerank cost becomes visible. |
| `graph_pq_candidate_recall/cluster_l2/m16_k64_c64_d128_k10_recallbp4625_m1594-full50000` | 8.87 ms (quick) | Larger subquantizer codebooks improve 64-candidate recall, with compressed storage still only ~1.56 MiB. |
| `graph_pq_candidate_recall/cluster_l2/m16_k64_c256_d128_k10_recallbp9500_m1594-full50000` | 9.68 ms (quick) | Best medium-width row: higher recall than `k16` at the same 256-candidate rerank width, for a small codebook-memory increase. |
| `graph_pq_candidate_recall/cluster_l2/m16_k64_c1024_d128_k10_recallbp10000_m1594-full50000` | 12.73 ms (quick) | Matches the 10000 bp high-recall row; useful as the baseline for future IVF/HNSW plus PQ layering rather than standalone full-code scans. |

PR-local scalar quantization spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_scalar_quant_candidate_recall/cluster_l2/u8_c64_d128_k10_recallbp10000_m12501-full50000` | 79.90 ms (quick) | Benchmark-only per-dimension u8 scalar quantization over 100k 128-dim vectors and 16 queries. Compressed storage is ~12.2 MiB vs ~48.8 MiB full vectors, and 64 exact-rerank candidates reach 10000 bp recall on this clustered fixture. |
| `graph_scalar_quant_candidate_recall/cluster_l2/u8_c256_d128_k10_recallbp10000_m12501-full50000` | 81.00 ms (quick) | Wider rerank has no recall upside on this corpus and adds a small exact-rerank cost. The full compressed scan remains the dominant cost. |
| `graph_scalar_quant_candidate_recall/cluster_l2/u8_c1024_d128_k10_recallbp10000_m12501-full50000` | 85.35 ms (quick) | High-candidate anchor for comparison with PQ. Scalar quantization is simple and training-free, but standalone row-wise dequantized scoring is much slower than PQ and IVF+PQ candidate generation without SIMD/block scoring. |
| `graph_scalar_code_quant_candidate_recall/cluster_l2/u8code_c64_d128_k10_recallbp10000_m12501-full50000` | 20.49 ms (quick) | Ranks by integer L2 over per-dimension u8 codes, then exact-reranks full vectors. It keeps scalar u8's full recall and storage shape while avoiding row-wise dequantization, but remains slower than packed binary and standalone PQ full-recall rows. |
| `graph_scalar_code_quant_candidate_recall/cluster_l2/u8code_c256_d128_k10_recallbp10000_m12501-full50000` | 21.41 ms (quick) | Wider rerank has no recall upside and adds modest exact-rerank cost; c64 is the scalar code-space knee on this fixture. |
| `graph_scalar_code_quant_candidate_recall/cluster_l2/u8code_c1024_d128_k10_recallbp10000_m12501-full50000` | 24.96 ms (quick) | High-candidate scalar code-space anchor. This narrows scalar's cost gap versus dequantized scoring by roughly 4x, but still does not beat simpler packed binary sign-code filtering. |

PR-local binary quantization spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_binary_quant_candidate_recall/cluster_l2/sign_c32_d128_k10_recallbp3375_m1562-full50000` | 3.35 ms (quick) | Benchmark-only packed sign-bit quantization over 100k 128-dim vectors and 16 queries. Memory is ~1.53 MiB vs ~48.8 MiB full vectors, but 32 candidates is too narrow. |
| `graph_binary_quant_candidate_recall/cluster_l2/sign_c64_d128_k10_recallbp6625_m1562-full50000` | 3.46 ms (quick) | Hamming prefilter plus exact rerank remains fast, but 64 candidates still loses material recall on this clustered L2 fixture. |
| `graph_binary_quant_candidate_recall/cluster_l2/sign_c256_d128_k10_recallbp10000_m1562-full50000` | 4.10 ms (quick) | First full-recall binary row: same compressed footprint as narrow binary rows, roughly 3x faster than standalone PQ full-recall rows and much smaller/faster than scalar u8. |
| `graph_binary_quant_candidate_recall/cluster_l2/sign_c1024_d128_k10_recallbp10000_m1562-full50000` | 7.15 ms (quick) | Wider exact rerank has no recall upside on this fixture and doubles latency versus the c256 knee. |

PR-local TurboQuant-style compression spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_turbo_quant_candidate_recall/cluster_cos/tq2_c256_d128_k10_recallbp1875_m3515-full50000` | 142.02 ms (quick) | Benchmark-only portable TurboQuant-style row over 100k 128-dim vectors and 16 cosine queries. It normalizes vectors, applies deterministic safe orthogonal mixing, scans clipped-uniform packed 2-bit coordinate codes with per-vector scale correction, then exact-reranks full vectors. Memory is ~3.43 MiB vs ~48.8 MiB full vectors, but 256 candidates recover too little of the exact cosine top-k. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tq2_c1024_d128_k10_recallbp8125_m3515-full50000` | 146.29 ms (quick) | Wider exact rerank improves clipped-uniform 2-bit recall to 8125 bp without changing compressed storage, but the scalar packed-code scan is still far slower than existing packed binary and PQ rows. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tq3_c256_d128_k10_recallbp2125_m5078-full50000` | 142.03 ms (quick) | Clipped-uniform 3-bit uses ~4.96 MiB and remains weak at 256 candidates, so bit width alone is not a quality win without a stronger codebook/scoring path. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tq3_c1024_d128_k10_recallbp7500_m5078-full50000` | 146.20 ms (quick) | High-candidate clipped-uniform 3-bit trails the 2-bit high-candidate recall while using more memory. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tq4_c64_d128_k10_recallbp1875_m6640-full50000` | 140.39 ms (quick) | Narrow clipped-uniform 4-bit is still too narrow for this scorer even after scale correction. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tq4_c256_d128_k10_recallbp6250_m6640-full50000` | 141.70 ms (quick) | The per-vector scale correction makes clipped-uniform 4-bit quality respond to wider rerank, but latency remains dominated by scalar packed-code scoring. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tq4_c1024_d128_k10_recallbp10000_m6640-full50000` | 146.15 ms (quick) | High-candidate clipped-uniform 4-bit reaches full recall with ~6.48 MiB compressed storage, giving the first quality-positive TurboQuant-style row. It is still about an order of magnitude slower than standalone PQ full-recall rows and far slower than packed binary. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqlm2_c256_d128_k10_recallbp2875_m3515-full50000` | 141.66 ms (quick) | Normal-limit Lloyd-Max 2-bit improves the 256-candidate clipped-uniform recall, but not enough to become useful. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqlm2_c1024_d128_k10_recallbp7500_m3515-full50000` | 148.87 ms (quick) | The same Lloyd-Max 2-bit codebook trails clipped-uniform at 1024 candidates, so it is not a clear 2-bit win on this fixture. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqlm3_c256_d128_k10_recallbp4375_m5078-full50000` | 142.13 ms (quick) | Normal-limit Lloyd-Max 3-bit roughly doubles the clipped-uniform 256-candidate recall at the same memory. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqlm3_c1024_d128_k10_recallbp8125_m5078-full50000` | 146.23 ms (quick) | Lloyd-Max 3-bit beats clipped-uniform 3-bit at 1024 candidates, but still does not reach the clipped-uniform 4-bit full-recall row. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqlm4_c64_d128_k10_recallbp6875_m6640-full50000` | 140.49 ms (quick) | Normal-limit Lloyd-Max 4-bit makes the narrow 64-candidate row useful where clipped-uniform 4-bit was too weak. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqlm4_c256_d128_k10_recallbp8500_m6640-full50000` | 141.57 ms (quick) | Lloyd-Max 4-bit improves medium-width recall at the same memory and latency envelope. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqlm4_c1024_d128_k10_recallbp9375_m6640-full50000` | 145.99 ms (quick) | Lloyd-Max 4-bit falls short of full recall at 1024 candidates, while clipped-uniform 4-bit reaches 10000 bp; the TQ+ rows below isolate whether calibration closes that quality gap. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqplus4_c64_d128_k10_recallbp4375_m6641-full50000` | 157.94 ms (quick) | Quantile-calibrated TQ+ over the Lloyd-Max 4-bit codebook adds per-coordinate shift/scale state. It hurts the narrow row versus uncalibrated Lloyd-Max, so calibration is not a blanket win. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqplus4_c256_d128_k10_recallbp9000_m6641-full50000` | 161.85 ms (quick) | TQ+ improves the medium row from 8500 bp to 9000 bp, with the memory suffix increasing by only ~1 KiB for calibration metadata. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqplus4_c1024_d128_k10_recallbp10000_m6641-full50000` | 164.27 ms (quick) | TQ+ restores full recall for the Lloyd-Max 4-bit high-candidate row, matching clipped-uniform recall with a more TurboVec-shaped codec. The scalar scorer remains too slow for production promotion; the next blocker is fused LUT/block scoring. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqplus4lut_c64_d128_k10_recallbp4375_m6641-full50000` | 41.89 ms (quick) | Byte-LUT scoring preserves the calibrated 4-bit c64 recall while cutting scalar scorer latency by about 3.8x. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqplus4lut_c256_d128_k10_recallbp9000_m6641-full50000` | 43.38 ms (quick) | The LUT scorer keeps the useful 9000 bp medium-width row and makes calibrated TurboQuant much closer to the existing scalar-code rows. |
| `graph_turbo_quant_candidate_recall/cluster_cos/tqplus4lut_c1024_d128_k10_recallbp10000_m6641-full50000` | 48.35 ms (quick) | First full-recall TurboQuant-style row with a fused safe lookup scorer. It is still slower than standalone PQ full-recall rows and far slower than packed binary, but the remaining gap is now scorer/layout engineering rather than raw scalar decode cost. |

PR-local TurboQuant dimension-projection spot-check:

| Bench | 10k | Notes |
|---|---:|---|
| `graph_turbo_quant_dimension_projection/cluster_cos/tqplus4lut_c1024_d128_n10k_k10_recallbp10000_m665-full5000` | 3.7606 ms (quick) | Fixed 10k-row dimension sweep using the calibrated 4-bit byte-LUT scorer and exact cosine rerank. The 128-dim row preserves full recall with ~665 KiB compressed storage versus ~4.88 MiB full vectors. |
| `graph_turbo_quant_dimension_projection/cluster_cos/tqplus4lut_c1024_d768_n10k_k10_recallbp10000_m3795-full30000` | 21.382 ms (quick) | Block-Hadamard rotation handles the common 768-dim, non-power-of-two shape without dense rotation dependencies. Storage remains about 7.9x smaller than full vectors, but scan latency scales with dimension. |
| `graph_turbo_quant_dimension_projection/cluster_cos/tqplus4lut_c1024_d1536_n10k_k10_recallbp10000_m7551-full60000` | 42.732 ms (quick) | 1536-dim storage is ~7.37 MiB compressed versus ~58.6 MiB full vectors at 10k rows. Quality stays full on the clustered fixture; the open problem is still candidate gating and scorer throughput, not storage ratio. |

PR-local blocked TurboQuant dimension-projection spot-check:

| Bench | 10k | Notes |
|---|---:|---|
| `graph_turbo_quant_blocked_dimension_projection/cluster_cos/tqplus4blocked_c1024_d128_n10k_k10_recallbp10000_m666-full5000` | 2.7354 ms (quick) | Benchmark-only FastScan-shaped 32-row blocked layout over the same calibrated 4-bit byte-LUT scorer. The blocked scan preserves full recall and cuts the same-run row-major 128-dim row from 3.6788 ms to 2.7354 ms without changing the approximate candidate count. |
| `graph_turbo_quant_blocked_dimension_projection/cluster_cos/tqplus4blocked_c1024_d768_n10k_k10_recallbp10000_m3801-full30000` | 16.150 ms (quick) | Blocking scans one byte position across 32 rows at a time, improving the same-run row-major 768-dim row from 20.516 ms while keeping storage in the same ~3.7 MiB compressed range. This is the clearest production-layout candidate after the current row-major TurboQuant index. |
| `graph_turbo_quant_blocked_dimension_projection/cluster_cos/tqplus4blocked_c1024_d1536_n10k_k10_recallbp10000_m7563-full60000` | 32.324 ms (quick) | The 1536-dim blocked scorer improves the same-run row-major row from 41.516 ms with full recall. The remaining gap to production parallel scans points to combining block-major storage with Rayon and later safe-SIMD or FastScan-style in-register accumulation. |

PR-local wide blocked TurboQuant dimension-projection spot-check:

Command: `scripts/run-benches.sh --profile quick --bench vector_turbo_projection --filter graph_turbo_quant_blocked_wide_dimension_projection` after a same-run scalar blocked guard.

| Bench | Scalar blocked | Wide blocked | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_blocked_wide_dimension_projection/cluster_cos/tqplus4blockedwide_c1024_d128_n10k_k10_recallbp10000_m666-full5000` | 2.9893 ms | 2.0547 ms | Safe `wide::f64x4` accumulates four lanes of the 32-row block at a time and preserves the same full-recall exact rerank. |
| `graph_turbo_quant_blocked_wide_dimension_projection/cluster_cos/tqplus4blockedwide_c1024_d768_n10k_k10_recallbp10000_m3801-full30000` | 16.039 ms | 14.254 ms | The in-register lane accumulator keeps the same block-major bytes and candidate count while reducing the high-dimensional scan cost. |
| `graph_turbo_quant_blocked_wide_dimension_projection/cluster_cos/tqplus4blockedwide_c1024_d1536_n10k_k10_recallbp10000_m7563-full60000` | 33.277 ms | 28.172 ms | The widest row improves enough to promote the same accumulator shape into the production slot-order TurboQuant scan. |

PR-local FastScan-shaped TurboQuant dimension-projection spot-check:

Command: `scripts/run-benches.sh --profile quick --bench vector_turbo_projection --filter graph_turbo_quant_blocked_fast_scan_dimension_projection` with a same-run blocked-wide comparison.

| Bench | FastScan-shaped | Same-run wide blocked | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_blocked_fast_scan_dimension_projection/cluster_cos/tqplus4fastscan_c1024_d128_n10k_k10_recallbp10000_m666-full5000` | 1.7350 ms | 2.0683 ms | Benchmark-only safe `wide` byte-swizzle scorer uses 4-bit code nibbles, bounded `u16` accumulation, and exact cosine rerank. Full recall is preserved on the clustered fixture. |
| `graph_turbo_quant_blocked_fast_scan_dimension_projection/cluster_cos/tqplus4fastscan_c1024_d768_n10k_k10_recallbp10000_m3801-full30000` | 5.8985 ms | 14.158 ms | Quantized per-query LUTs remove the f64 byte-table load from the inner block scan; this is the first strong benchmark signal for a FastScan-style production scorer. |
| `graph_turbo_quant_blocked_fast_scan_dimension_projection/cluster_cos/tqplus4fastscan_c1024_d1536_n10k_k10_recallbp10000_m7563-full60000` | 11.176 ms | 27.882 ms | The high-dimensional row keeps full recall while cutting the same-run blocked-wide latency by roughly 2.5x. The production scorer below promotes this safe `wide` FastScan shape with tests around quantized-LUT bounds and candidate ordering. |

PR-local production TurboQuant dimension-projection spot-check:

| Bench | 10k | Notes |
|---|---:|---|
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c512_d128_n10k_k10_recallbp10000_m867-full5000` | 1.6700 ms (quick) | Production `VectorIndexKind::TurboQuantCosine` uses the current omitted search-width default (`512`) with the safe `wide` FastScan slot-order scorer, wider low-dimension Rayon chunks, bounded `u16` accumulators, and exact primary-vector rerank. |
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c512_d768_n10k_k10_recallbp10000_m4005-full30000` | 3.3273 ms (quick) | The 768-dim production row keeps full recall with the default c512 candidate envelope and the same compressed index around 3.9 MiB versus ~29.3 MiB primary vector components. |
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c512_d1536_n10k_k10_recallbp10000_m7770-full60000` | 5.3378 ms (quick) | 1536-dim production search stays full-recall at the c512 default width while preserving exact primary-vector rerank. |
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c512_d3072_n10k_k10_recallbp10000_m15300-full120000` | 8.4291 ms (quick) | 3072-dim production search keeps the same full-recall default-width guard used by TurboQuant/TurboVec-style high-dimensional rows while preserving exact primary-vector rerank. |

PR-local production TurboQuant batch dimension-projection spot-check:

| Bench | 10k x 8 queries | Notes |
|---|---:|---|
| `graph_turbo_quant_production_batch_dimension_projection/cluster_cos/tqcos_batch_c512_d128_q8_n10k_k10_recallbp10000_m867-full5000` | 1.0523 ms (quick) | Batch fusion shares each slot-order block scan across eight default-width queries while keeping full recall and exact primary-vector rerank. |
| `graph_turbo_quant_production_batch_dimension_projection/cluster_cos/tqcos_batch_c512_d768_q8_n10k_k10_recallbp10000_m4005-full30000` | 2.1693 ms (quick) | The common 768-dim batch row stays full-recall and keeps the default c512 fused FastScan path below the old c1024 single-query envelope. |
| `graph_turbo_quant_production_batch_dimension_projection/cluster_cos/tqcos_batch_c512_d1536_q8_n10k_k10_recallbp10000_m7770-full60000` | 3.5734 ms (quick) | The 1536-dim batch row stays inside the bounded FastScan accumulator envelope and remains the preferred path for multiple independent embedding lookups over the same index. |
| `graph_turbo_quant_production_batch_dimension_projection/cluster_cos/tqcos_batch_c512_d3072_q8_n10k_k10_recallbp10000_m15300-full120000` | 6.1372 ms (quick) | 3072-dim q8 batch search stays full-recall and keeps the shared FastScan block scan meaningfully below running eight independent high-dimensional searches. |

PR-local production filtered TurboQuant candidate-set spot-check:

Command: `scripts/run-benches.sh --profile quick --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_dimension_projection`.

| Bench | 10k x 8 queries over 4,243 candidates/query | Notes |
|---|---:|---|
| `graph_turbo_quant_production_filtered_dimension_projection/cluster_cos/tqcos_filtered_c512_d128_q8_cand4243_k10_recallbp10000_m867-full5000` | 1.9831 ms (quick) | Query-specific candidate sets use row-filtered TurboQuant allowlist scoring, then exact-rerank primary vectors. The row preserves full recall under the default c512 width. |
| `graph_turbo_quant_production_filtered_dimension_projection/cluster_cos/tqcos_filtered_c512_d768_q8_cand4243_k10_recallbp10000_m4005-full30000` | 3.6674 ms (quick) | The filtered FastScan path intersects caller candidates with registered index rows before compressed scoring, avoiding wrong-label/state spillover while preserving exact final distances. |
| `graph_turbo_quant_production_filtered_dimension_projection/cluster_cos/tqcos_filtered_c512_d1536_q8_cand4243_k10_recallbp10000_m7770-full60000` | 5.3210 ms (quick) | At 1536 dimensions, the candidate-filtered path stays full-recall at the default width and remains the single-query primitive for graph/state-gated retrieval. |
| `graph_turbo_quant_production_filtered_dimension_projection/cluster_cos/tqcos_filtered_c512_d3072_q8_cand4243_k10_recallbp10000_m15300-full120000` | 8.1950 ms (quick) | The 3072-dim filtered path keeps full recall with in-kernel allowlist scoring, validating the high-dimensional graph/state-gated shape without over-fetch fallback. |

PR-local production filtered batch TurboQuant candidate-set spot-check:

Command: `scripts/run-benches.sh --profile quick --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_batch_dimension_projection`.

| Bench | 10k x 8 queries over 4,243 candidates/query | Notes |
|---|---:|---|
| `graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c512_d128_q8_cand4243_k10_recallbp10000_m867-full5000` | 1.1935 ms (quick) | Fused filtered-batch FastScan shares slot-order block reads across query-specific candidate sets while preserving exact primary-vector rerank and full recall at the default width. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c512_d768_q8_cand4243_k10_recallbp10000_m4005-full30000` | 2.3456 ms (quick) | The 768-dim row keeps candidate-set isolation per query but fuses compressed scoring over shared blocks, staying well below the single-query filtered path. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c512_d1536_q8_cand4243_k10_recallbp10000_m7770-full60000` | 3.6223 ms (quick) | The 1536-dim row stays inside the bounded FastScan accumulator envelope and gives graph-filtered multi-query workloads the fastest current production path. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c512_d3072_q8_cand4243_k10_recallbp10000_m15300-full120000` | 6.2490 ms (quick) | 3072-dim query-specific filtered batches preserve full recall and keep the fused compressed scan below the single-query filtered high-dimensional path. |

PR-local production filtered batch FastScan query-scale A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c512_d1536 --save-baseline tq_fast_scan_max_contrib_d1536_pre`
with the per-dimension centroid scan, then the same command with
`--baseline tq_fast_scan_max_contrib_d1536_pre` after reducing max query
contribution against the codebook's maximum absolute centroid. The d128 guard
used the same command shape with `...d128` and baseline
`tq_fast_scan_max_contrib_d128_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...d1536` | 3.6018 ms | 3.4985 ms | FastScan LUT prep now computes the quantization scale with one codebook max and one pass over query dimensions, improving the high-dimensional filtered batch row by 2.61% (`p=0.00`). |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...d128` | 1.2351 ms | 1.2397 ms | Low-dimensional guard remains within Criterion's noise threshold, so the query-scale simplification does not cost the short-vector filtered batch path. |

PR-local production sparse/mixed filtered batch TurboQuant A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_projection --filter graph_turbo_quant_production_sparse_filtered_batch_dimension_projection/cluster_cos/tqcos_sparse_filtered_batch_c512_d1536 --save-baseline tq_sparse_lut_pre`
with the old sparse byte-LUT builder, then the same command with
`--baseline tq_sparse_lut_pre` after hoisting per-byte query components.
Mixed and dense guardrails used the same profile/filter shape against
`graph_turbo_quant_production_mixed_filtered_batch_dimension_projection/...d1536`
and `graph_turbo_quant_production_filtered_batch_dimension_projection/...d1536`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_sparse_filtered_batch_dimension_projection/...d1536` | 1.9403 ms | 1.3195 ms | Sparse 64-candidate/query batches use the live-map byte-LUT path. Hoisting query components out of the 256-entry packed-byte loop improves this row by 31.93% (`p=0.00`) while preserving exact primary-vector rerank and full recall. |
| `graph_turbo_quant_production_mixed_filtered_batch_dimension_projection/...d1536` | 2.5597 ms | 2.5703 ms | Mixed dense+sparse candidate sets stay within Criterion's noise threshold, so the sparse LUT change does not move the existing fused FastScan policy. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...d1536` | documented 3.6223 ms / local noisy guard | 3.7735 ms | Dense 4,243-candidate/query filtered batches remain on the FastScan path; rerun reported no detected performance change after one outlier-heavy sample. |

Follow-up sparse byte-LUT fill A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_projection --filter graph_turbo_quant_production_sparse_filtered_batch_dimension_projection/cluster_cos/tqcos_sparse_filtered_batch_c512_d1536 --save-baseline tq_sparse_fill_pre`
with the post-hoist byte-LUT loop, then the same command with
`--baseline tq_sparse_fill_pre` after converting the 16 codebook centroids once
and using explicit full-byte / odd-tail LUT fill branches.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_sparse_filtered_batch_dimension_projection/...d1536` | 1.3196 ms | 1.0617 ms | Branch-free full-byte LUT fill removes per-packed `Option` work and repeated centroid widening, improving the sparse 64-candidate/query row by 19.73% (`p=0.00`) on top of the earlier hoist. |

PR-local production shared-filtered batch TurboQuant candidate-set A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_projection --filter graph_turbo_quant_production_shared_filtered_batch_dimension_projection --save-baseline tq_shared_filter_pre`
with the new benchmark row on the pre-change filtered batch path, then the same
command with `--baseline tq_shared_filter_pre` after routing repeated candidate
sets through one row allowlist and a shared-lane FastScan mask.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_shared_filtered_batch_dimension_projection/...d128` | 1.2165 ms | 773.08 µs | Repeated graph/state candidate sets now convert to index rows once and build one per-block lane mask for all queries. |
| `graph_turbo_quant_production_shared_filtered_batch_dimension_projection/...d768` | 2.3107 ms | 1.8740 ms | The shared allowlist path avoids duplicate row conversion while preserving full-recall exact primary-vector rerank. |
| `graph_turbo_quant_production_shared_filtered_batch_dimension_projection/...d1536` | 3.4826 ms | 3.1296 ms | High-dimensional shared-filtered batches keep the same bounded FastScan accumulator and reduce duplicate filter bookkeeping. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...d128/d768/d1536` | 1.2137 ms / 2.3291 ms / 3.5864 ms | 1.2202 ms / 2.3073 ms / 3.6319 ms | Distinct per-query candidate sets use endpoint-guarded equality detection and stay within Criterion's noise threshold. |

Additional d3072 shared-filter guard:

| Bench | 10k x 8 queries over one shared 4,243-candidate set | Notes |
|---|---:|---|
| `graph_turbo_quant_production_shared_filtered_batch_dimension_projection/cluster_cos/tqcos_shared_filtered_batch_c512_d3072_q8_cand4243_k10_recallbp3875_m15300-full120000` | 5.8070 ms (quick) | This row intentionally reuses one allowlist across all queries, so recall reflects candidate-set mismatch; it is a high-dimensional latency/bookkeeping guard for repeated graph/state filters. |

PR-local production FastScan accumulator-flush spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection --baseline tq_fastscan_flush_single_pre`;
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_projection --filter graph_turbo_quant_production_batch_dimension_projection --baseline tq_batch_insert_pre`;
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_batch_dimension_projection --baseline tq_filtered_batch_mask_pre`.

| Bench | 10k rows / q8 where applicable | Notes |
|---|---:|---|
| `graph_turbo_quant_production_dimension_projection/...d128` | 3.3467 ms (quick) | FastScan now keeps the full 7-bit query LUT and flushes `u16` accumulators every 128 packed bytes when needed. The 128-dim single-query row is unchanged versus the saved baseline. |
| `graph_turbo_quant_production_dimension_projection/...d768` | 5.5840 ms (quick) | Single-query 768-dim search stays within Criterion's noise threshold versus the saved pre-flush baseline. |
| `graph_turbo_quant_production_dimension_projection/...d1536` | 8.1866 ms (quick) | Single-query 1536-dim search trends lower than the saved pre-flush baseline but remains within the noise threshold. |
| `graph_turbo_quant_production_batch_dimension_projection/...d128/d768/d1536` | 2.3157 ms / 4.1601 ms / 6.3195 ms (quick) | Fused full-index batch search remains neutral across the same 30-sample comparison, so higher-precision FastScan does not cost the core batch path. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...d128/d768/d1536` | 2.2700 ms / 3.8392 ms / 5.9899 ms (quick) | Query-specific filtered batch search improves the 128-dim row significantly and trends lower at 768/1536 while preserving full recall and exact primary-vector rerank. |

PR-local TurboQuant compact-slot scan spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d128 --baseline tq_slot_live_lookup_pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c1024_d128 --baseline tq_slot_live_lookup_filtered_batch_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_dimension_projection/...d128` | 3.3535 ms | 3.3232 ms | Compact TurboQuant slots now read `rows[slot]` directly during slot-order scans while keeping row-to-slot hash alignment as a debug assertion. The full-index row improves 1.26% (`p=0.00`), a small but measurable removal of per-slot liveness lookups. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...d128` | 2.4972 ms | 2.2394 ms | Candidate-set filtered batch search benefits most because each allowed-row check shares the compact slot lookup. The row improves 10.30% (`p=0.00`) while preserving exact primary-vector rerank semantics. |

PR-local TurboQuant filtered FastScan lane-mask spot-check:

Command: `scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_dimension_projection/cluster_cos/tqcos_filtered_c1024_d128 --baseline tq_filter_single_mask_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_filtered_dimension_projection/...d128` | 3.1305 ms | 2.9552 ms | Single-query candidate-set FastScan now builds one per-block allowed-lane mask before compressed scoring, so block filtering and hit emission share the same Roaring membership pass. The row improves 5.28% (`p=0.00`) while preserving exact primary-vector rerank. |

PR-local TurboQuant filtered batch lane-mask spot-check:

Command: `scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c1024_d128 --baseline filtered_batch_masks_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...d128` | 2.0060 ms | 1.9425 ms | Fused filtered batch FastScan now precomputes per-query allowed-lane masks and lane scales once per block, so hit emission walks set bits instead of repeating row lookups and Roaring membership checks for every query/lane pair. The row improves 2.64% (`p=0.00`) while preserving exact primary-vector rerank. |

PR-local TurboQuant default search-width spot-check:

Command: `scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection`.

This is the historical c1024-to-c512 comparison captured before the
low-dimension chunk tuning below; the current c512 production medians are in
the tables above.

| Bench | c1024 median | c512 median | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_dimension_projection/...tqcos_c512_d128` | 2.936 ms | 2.029 ms | The committed production bench now carries the default-width row name. It preserves `recallbp10000` on the 10k clustered cosine fixture while keeping the omitted-width production search envelope about 31% below the prior c1024 row. |
| `graph_turbo_quant_production_dimension_projection/...tqcos_c512_d768` | 5.243 ms | 3.621 ms | The common 768-dim embedding shape also preserves full recall at c512 and keeps the same roughly 31% improvement versus c1024. |
| `graph_turbo_quant_production_dimension_projection/...tqcos_c512_d1536` | 7.730 ms | 5.337 ms | The high-dimensional guard row remains full-recall at c512 and now has a first-class benchmark ID, keeping the TurboQuant default-width decision reproducible. |

PR-local TurboQuant low-dimension parallel chunk spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c512_d128 --baseline tq_chunk1024_projection_pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c512_d1536 --baseline tq_chunk1024_d1536_pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_churn --filter graph_turbo_quant_churn --baseline churn_c512_pre`.

| Bench | 1024-entry chunks | Dimension-aware chunks | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_dimension_projection/...tqcos_c512_d128` | 2.0273 ms | 1.7387 ms | 128-dim c512 scans use 2,048-entry Rayon chunks, reducing merge overhead while preserving full recall and exact primary-vector rerank. Criterion reports a 14.35% improvement (`p=0.00`). |
| `graph_turbo_quant_production_dimension_projection/...tqcos_c512_d1536` | 5.3562 ms | 5.4626 ms | The 1536-dim guard stays on the existing 1,024-entry chunk size; Criterion reports the comparison as within the noise threshold, so the threshold avoids the broad 2,048-entry regression seen in discarded experiments. |
| `graph_turbo_quant_churn/tqcos_update10_delete5/c512_n10k` | 227.21 µs | 196.13 µs | The default-width churn fixture also benefits from the 128-dim chunk rule after 10% updates and 5% deletes, improving 13.35% (`p=0.00`). |

PR-local TurboQuant full-scan low-dimension chunk spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 40 --measurement-time 4 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c512_d128 --baseline tq_single_chunk2048_pre`;
`scripts/run-benches.sh --profile quick --sample-size 40 --measurement-time 4 --bench vector_turbo_projection --filter graph_turbo_quant_production_batch_dimension_projection/cluster_cos/tqcos_batch_c512_d128 --baseline tq_batch_chunk2048_pre`;
`scripts/run-benches.sh --profile quick --sample-size 40 --measurement-time 4 --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c512_d128 --baseline tq_filtered_chunk2048_fresh_pre`.

| Bench | 2048-entry chunks | Full-scan chunks | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_dimension_projection/...tqcos_c512_d128` | 1.6486 ms | 1.2042 ms | Full-index 128-dim c512 scans use a separate 4,096-entry chunk helper, reducing Rayon split/merge overhead while preserving full recall and exact primary-vector rerank. Criterion reports a 26.83% improvement (`p=0.00`). |
| `graph_turbo_quant_production_batch_dimension_projection/...tqcos_batch_c512_d128_q8` | 942.85 µs | 872.65 µs | Full-index q8 batch scans use the same full-scan helper and improve 7.84% (`p=0.00`) against the saved 2,048-entry baseline. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...tqcos_filtered_batch_c512_d128` | 1.2157 ms | 1.2045 ms | Filtered candidate-set scans intentionally stay on the existing 2,048-entry helper; Criterion reports the fresh comparison as within the noise threshold. |

PR-local TurboQuant row-key top-k spot-check:

Command: `scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d128 --baseline tq_row_key_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_dimension_projection/...d128` | 3.3304 ms | 2.9044 ms | TurboQuant approximate candidate heaps now carry row ids directly instead of `(slot, row)` pairs. Scan loops still use slots locally for packed-code and scale lookups, then emit compact row keys for top-k ordering before exact primary-vector rerank. The row improves 13.05% (`p=0.00`). |

PR-local VectorTopK heap preallocation spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c1024_d128 --baseline topk_filter_batch_pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d128 --baseline topk_prealloc_pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench value_clone --filter core_vector_exact_top_k/cosine_2048x128_k10 --baseline topk_core_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...d128` | 2.0266 ms | 1.9426 ms | `VectorTopK::new` now reserves its retained-hit heap up front. The query-specific filtered batch path builds multiple candidate heaps per scan, so it benefits most; the row improves 4.62% (`p=0.00`). |
| `graph_turbo_quant_production_dimension_projection/...d128` | 2.9299 ms | 2.8888 ms | Full-index single-query TurboQuant trends 1.36% faster (`p=0.00`) but remains within Criterion's noise threshold. |
| `core_vector_exact_top_k/cosine_2048x128_k10` | 52.877 us | 52.658 us | Core exact top-k construction also trends lower but remains within the noise threshold; the production filtered-batch row is the keep/drop signal. |

PR-local VectorTopK heap replacement spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench value_clone --filter core_vector_exact_top_k/cosine_2048x128_k10 --baseline topk_peek_mut_core_pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c512_d128 --baseline fastscan_first_flush_single_pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_turbo_projection --filter graph_turbo_quant_production_filtered_batch_dimension_projection/cluster_cos/tqcos_filtered_batch_c512_d128 --baseline fastscan_first_flush_filtered_batch_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `core_vector_exact_top_k/cosine_2048x128_k10` | 53.115 us | 51.958 us | `VectorTopK::push_distance` now replaces the retained worst hit through `BinaryHeap::peek_mut()` instead of `pop()` plus `push()`. The bound-query exact rerank row improves 3.71% (`p=0.00`). |
| `graph_turbo_quant_production_dimension_projection/...tqcos_c512_d128` | 1.7669 ms | 1.6700 ms | Full-index TurboQuant benefits because approximate candidate selection and exact primary-vector rerank both use bounded top-k heaps. The row improves 5.55% (`p=0.00`) while preserving full recall. |
| `graph_turbo_quant_production_filtered_batch_dimension_projection/...tqcos_filtered_batch_c512_d128` | 1.2508 ms | 1.1935 ms | Filtered batch search builds one retained heap per query and sees a 4.43% improvement (`p=0.00`) while preserving candidate-set isolation and exact rerank. |

PR-local TurboQuant slot-map storage spot-check:

Command: `scripts/run-benches.sh --profile quick --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection`.

| Bench | Index storage | Notes |
|---|---:|---|
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d128_n10k_k10_recallbp10000_m931-full5000` | 931 KiB / 10k rows | TurboQuant row-to-slot metadata uses `u32` slot keys while preserving full-recall candidate search and exact primary-vector rerank. |
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d768_n10k_k10_recallbp10000_m4069-full30000` | 4,069 KiB / 10k rows | The metadata compaction is dimension-independent, so the higher-dimensional rows retain the same compressed-code layout and calibration arrays. |
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d1536_n10k_k10_recallbp10000_m7834-full60000` | 7,834 KiB / 10k rows | The current resident estimate excludes primary `VECTOR` components, which remain graph-owned source-of-truth values for exact rerank. |

PR-local TurboQuant row-slot storage spot-check:

Command: `scripts/run-benches.sh --profile quick --bench vector_turbo_projection --filter graph_turbo_quant_production_dimension_projection`.

| Bench | Index storage | Notes |
|---|---:|---|
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d128_n10k_k10_recallbp10000_m867-full5000` | 867 KiB / 10k rows | TurboQuant slot metadata now stores row ids in a compact `Vec<u32>` and derives liveness from the row-to-slot map, removing the padded per-slot deleted flag while preserving full-recall candidate search. |
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d768_n10k_k10_recallbp10000_m4005-full30000` | 4,005 KiB / 10k rows | Higher-dimensional storage remains dominated by packed coordinate codes and calibration arrays; the row-slot compaction reduces the fixed per-row metadata. |
| `graph_turbo_quant_production_dimension_projection/cluster_cos/tqcos_c1024_d1536_n10k_k10_recallbp10000_m7770-full60000` | 7,770 KiB / 10k rows | The exact primary `VECTOR` values remain graph-owned and are still used for final rerank; the compact TurboQuant index remains derived, rebuildable state. |

PR-local TurboQuant churn compaction spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn --save-baseline tq_swap_remove_pre_stable`;
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn --baseline tq_swap_remove_pre_stable`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_update10_delete5/n10k` | 387.01 µs | 347.14 µs | 10k-row production `TurboQuantCosine` fixture after 10% vector updates and 5% deletes. Immediate slot compaction improves query time by 6.3-9.7% and shrinks resident derived-state counters from `tqe11kl9500d1500_m1022-1022` to `tqe9500l9500d0_m850-850`. |

PR-local TurboQuant bulk indexing spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_churn --filter tqcos_create_index --save-baseline tq_bulk_encode_pre`;
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_churn --filter tqcos_create_index --baseline tq_bulk_encode_pre`;
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_churn --filter tqcos_update10_delete5`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_create_index/d128_n10k` | 36.570 ms | 25.953 ms | New-index bulk insert now stores rotated calibration input and defers packed-code writes until TQ+ calibration is known, avoiding the discarded pre-calibration encode pass. Criterion reports a 29.03% improvement (`p=0.00`). |
| `graph_turbo_quant_churn/tqcos_update10_delete5/c512_n10k` | n/a | 146.61 µs | Existing post-churn query guardrail on the same run; finalized search still uses calibrated packed codes plus exact primary-vector rerank. |

PR-local TurboQuant row-byte encode spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d1536_n2k --save-baseline tq_encode_rowbytes_d1536_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d1536_n2k --baseline tq_encode_rowbytes_d1536_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d128_n10k --save-baseline tq_encode_rowbytes_d128_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d128_n10k --baseline tq_encode_rowbytes_d128_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_create_index/d1536_n2k` | 47.958 ms | 45.533 ms | Production 4-bit TurboQuant encode now packs one row's nibbles and writes row bytes into blocked storage, avoiding per-coordinate generic bit writes. Criterion reports a 5.06% improvement (`p=0.00`). |
| `graph_turbo_quant_churn/tqcos_create_index/d128_n10k` | 17.917 ms | 15.229 ms | The lower-dimensional create row benefits more from reduced encode overhead, improving 15.00% (`p=0.00`) while retaining the same calibration and blocked-code layout. |

PR-local TurboQuant parallel bulk encode spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d1536_n2k --save-baseline tq_parallel_encode_d1536_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d1536_n2k --baseline tq_parallel_encode_d1536_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d128_n10k --save-baseline tq_parallel_encode_d128_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d128_n10k --baseline tq_parallel_encode_d128_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_create_index/d1536_n2k` | 45.839 ms | 17.549 ms | Post-calibration bulk encoding now packs row bytes in parallel once a build crosses the 1M rotated-value threshold, then copies the finished rows into blocked storage. Criterion reports a 61.72% improvement (`p=0.00`). |
| `graph_turbo_quant_churn/tqcos_create_index/d128_n10k` | 15.492 ms | 9.9322 ms | The lower-dimensional 10k-row create guardrail also crosses the threshold and improves 36.31% (`p=0.00`), with the same TQ+ calibration and blocked-code layout. |

PR-local TurboQuant codebook partition-point spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d1536_n2k --baseline tq_row_buffer_d1536_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter graph_turbo_quant_churn/tqcos_create_index/d128_n10k --baseline tq_row_buffer_d128_pre`.

| Bench | Linear boundary scan | Partition-point search | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_create_index/d1536_n2k` | 17.534 ms | 15.059 ms | TurboQuant scalar codebook encoding now uses strict `partition_point` over sorted centroid boundaries, preserving lower-code boundary ties while reducing per-coordinate comparisons during bulk encode. Criterion reports a 14.43% improvement (`p=0.00`). |
| `graph_turbo_quant_churn/tqcos_create_index/d128_n10k` | 9.9037 ms | 9.6178 ms | The row-count-heavy 128-dim build also benefits, improving 2.70% (`p=0.00`) on top of the parallel bulk-encode path. |

PR-local TurboQuant calibration selection spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_churn --filter tqcos_create_index --save-baseline tq_quantile_sort_pre`;
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_churn --filter tqcos_create_index --baseline tq_quantile_sort_pre`;
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench vector_turbo_churn --filter tqcos_update10_delete5`.

| Bench | Full coordinate sort | Two-quantile selection | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_create_index/d128_n10k` | 27.114 ms | 17.806 ms | TQ+ calibration now selects the 5% and 95% coordinate ranks directly instead of sorting each coordinate vector completely. Criterion reports a 34.33% improvement (`p=0.00`) on top of deferred bulk encoding. |
| `graph_turbo_quant_churn/tqcos_update10_delete5/c512_n10k` | n/a | 145.77 µs | Existing post-churn query guardrail remains stable after the calibration implementation change. |

PR-local TurboQuant high-dimensional calibration spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter d1536 --save-baseline tq_d1536_build_seq_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter d1536 --baseline tq_d1536_build_seq_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter 'd128|tqcos_update10_delete5'`.

| Bench | Sequential calibration | Parallel calibration | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_create_index/d1536_n2k` | 54.989 ms | 50.228 ms | High-dimensional TQ+ calibration now parallelizes coordinate quantile extraction once the build has at least 512 dimensions and 1M rotated values. Criterion reports an 8.66% improvement (`p=0.00`). |
| `graph_turbo_quant_churn/tqcos_create_index/d128_n10k` | n/a | 17.959 ms | Low-dimensional create-index guardrail stays on the sequential calibration path. |
| `graph_turbo_quant_churn/tqcos_update10_delete5/c512_n10k` | n/a | 148.40 µs | Existing post-churn query guardrail remains stable with the high-dimensional threshold in place. |

PR-local TurboQuant bulk-buffer move spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter d1536 --save-baseline tq_bulk_rotated_copy_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter d1536 --baseline tq_bulk_rotated_copy_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter 'd128|tqcos_update10_delete5'`.

| Bench | Copied bulk buffer | Moved bulk buffer | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_create_index/d1536_n2k` | 50.138 ms | 48.049 ms | `finish_bulk_load` now validates the compact-slot bulk buffer length and moves the pending rotated vectors into the final calibration/encoding pass instead of cloning them into a second vector. Criterion reports a 4.17% improvement (`p=0.00`). |
| `graph_turbo_quant_churn/tqcos_create_index/d128_n10k` | n/a | 17.900 ms | Low-dimensional create-index guardrail remains stable. |
| `graph_turbo_quant_churn/tqcos_update10_delete5/c512_n10k` | n/a | 145.87 µs | Existing post-churn query guardrail remains stable. |

PR-local TurboQuant bulk code allocation spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter d1536 --save-baseline tq_bulk_codes_resize_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter d1536 --baseline tq_bulk_codes_resize_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter d128 --save-baseline tq_bulk_codes_resize_d128_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter d128 --baseline tq_bulk_codes_resize_d128_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_turbo_churn --filter 'd128|tqcos_update10_delete5'`.

| Bench | Eager code allocation | Deferred code allocation | Notes |
|---|---:|---:|---|
| `graph_turbo_quant_churn/tqcos_create_index/d128_n10k` | 17.996 ms | 17.290 ms | Bulk insert now leaves the packed-code matrix unallocated until `finish_bulk_load`, avoiding repeated resize/zero-fill work before final TQ+ calibration. Criterion reports a 3.92% improvement (`p=0.00`). |
| `graph_turbo_quant_churn/tqcos_create_index/d1536_n2k` | 47.763 ms | 47.839 ms | High-dimensional build time is unchanged (`p=0.65`), so the win is limited to row-count-heavy bulk allocation pressure rather than calibration throughput. |
| `graph_turbo_quant_churn/tqcos_update10_delete5/c512_n10k` | n/a | 148.12 µs | Existing post-churn query guardrail remains stable after finalized indexes allocate and compact packed-code rows normally. |

PR-local IVF+TurboQuant layering spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_ivf_turbo_quant_candidate_recall/cluster_cos/tqplus4lut_c256_p1_d128_k10_recallbp9000_rows25008_m7454-full50000` | 1.4692 ms (quick) | Synthetic IVF probes one list per query, then scores calibrated 4-bit TurboQuant byte-LUT codes before exact cosine rerank. It scans ~25k rows across 16 queries and preserves the standalone c256 row's 9000 bp recall while cutting latency from ~43 ms to ~1.47 ms. |
| `graph_ivf_turbo_quant_candidate_recall/cluster_cos/tqplus4lut_c1024_p1_d128_k10_recallbp10000_rows25008_m7454-full50000` | 2.2484 ms (quick) | One-list IVF preserves the calibrated c1024 full-recall suffix while cutting standalone full-code latency from ~48 ms to ~2.25 ms, but it is still slower than IVF+PQ and IVF+binary full-recall rows. |
| `graph_ivf_turbo_quant_candidate_recall/cluster_cos/tqplus4lut_c1024_p4_d128_k10_recallbp10000_rows100015_m7454-full50000` | 4.5898 ms (quick) | Four-list probe keeps full recall but scans ~100k rows across the query batch, useful mainly as a guardrail for less separable fixtures. |

PR-local IVF+PQ layering spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_ivf_pq_candidate_recall/cluster_l2/m16_k64_c256_p1_d128_k10_recallbp9500_rows25008_m2407-full50000` | 472.64 µs (quick) | Coarse synthetic IVF-style partition probes one list per query, then PQ scores and exact-reranks 256 candidates. Scans ~25k total rows across 16 queries and keeps the full-code PQ row's 9500 bp recall while using ~2.35 MiB compressed/coarse index memory. |
| `graph_ivf_pq_candidate_recall/cluster_l2/m16_k64_c256_p2_d128_k10_recallbp9500_rows50010_m2407-full50000` | 609.20 µs (quick) | Probing two lists doubles candidate rows but does not improve recall on this corpus, which suggests the synthetic partition is already separating the query clusters cleanly. |
| `graph_ivf_pq_candidate_recall/cluster_l2/m16_k64_c1024_p1_d128_k10_recallbp10000_rows25008_m2407-full50000` | 1.0475 ms (quick) | High-recall layered row: matches standalone PQ's 10000 bp result while scanning ~25k rows across the 16-query batch instead of 1.6M full-code rows, running roughly 12x faster than the standalone `m16_k64_c1024` row. |
| `graph_ivf_pq_candidate_recall/cluster_l2/m16_k64_c1024_p2_d128_k10_recallbp10000_rows50010_m2407-full50000` | 1.1879 ms (quick) | Two-list probe keeps perfect recall but adds work without benefit on the clustered fixture; useful as a guardrail when future fixtures are less separable. |

PR-local IVF+scalar-code layering spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_ivf_scalar_code_candidate_recall/cluster_l2/u8code_c64_p1_d128_k10_recallbp10000_rows25008_m13314-full50000` | 526.58 µs (quick) | Synthetic IVF probes one list per query, then ranks probed rows by integer L2 over per-dimension u8 codes before exact rerank. It reaches full recall at the narrow c64 width and is about 2x faster than IVF+PQ full-recall p1, but uses ~13.0 MiB compressed/coarse memory. |
| `graph_ivf_scalar_code_candidate_recall/cluster_l2/u8code_c256_p1_d128_k10_recallbp10000_rows25008_m13314-full50000` | 819.44 µs (quick) | Wider rerank has no quality upside and adds exact-rerank cost; c64 is the scalar-code IVF knee on this fixture. |
| `graph_ivf_scalar_code_candidate_recall/cluster_l2/u8code_c256_p2_d128_k10_recallbp10000_rows50010_m13314-full50000` | 1.1598 ms (quick) | Two-list probing doubles searched rows with no recall upside on this separable corpus. |
| `graph_ivf_scalar_code_candidate_recall/cluster_l2/u8code_c1024_p1_d128_k10_recallbp10000_rows25008_m13314-full50000` | 1.3434 ms (quick) | High-candidate scalar-code IVF anchor. It is slower than the narrow c64 row and slower than IVF+PQ full-recall p1, so code-space scalar needs a memory/quality reason before it beats simpler alternatives. |

PR-local IVF+binary layering spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_ivf_binary_candidate_recall/cluster_l2/sign_c64_p1_d128_k10_recallbp6625_rows25008_m2375-full50000` | 150.2 µs (quick) | Synthetic IVF probes one list per query, then Hamming-scores packed sign bits and exact-reranks 64 candidates. Very fast, but still too narrow for recall. |
| `graph_ivf_binary_candidate_recall/cluster_l2/sign_c256_p1_d128_k10_recallbp10000_rows25008_m2375-full50000` | 309.8 µs (quick) | High-recall binary layered row: scans the same ~25k rows as IVF+PQ p1 but reaches 10000 bp about 3.4x faster than the `m16_k64_c1024_p1` IVF+PQ row, with similar compressed/coarse memory. |
| `graph_ivf_binary_candidate_recall/cluster_l2/sign_c256_p2_d128_k10_recallbp10000_rows50010_m2375-full50000` | 363.3 µs (quick) | Two-list probe doubles searched rows but has no recall upside on this separable fixture; p1 is the better knee. |
| `graph_ivf_binary_candidate_recall/cluster_l2/sign_c1024_p1_d128_k10_recallbp10000_rows25008_m2375-full50000` | 901.6 µs (quick) | Wider exact rerank stays below IVF+PQ full-recall latency but has no quality benefit over c256 on this corpus. |

PR-local IVF overlap-corpus compression spot-check:

| Bench | 100k | Notes |
|---|---:|---|
| `graph_ivf_overlap_candidate_recall/pq_overlap_l2/m16_k64_c1024_p1_d128_k10_recallbp5000_rows24996_m2407-full50000` | 1.334 ms (quick) | Harder overlap profile where cluster signal competes with local variation. One-list probing misses half the oracle hits even with 1024 PQ rerank candidates. |
| `graph_ivf_overlap_candidate_recall/pq_overlap_l2/m16_k64_c1024_p4_d128_k10_recallbp10000_rows100005_m2407-full50000` | 2.407 ms (quick) | Four-list probing restores full recall for the IVF+PQ high-recall row, but searches about 100k rows across the 16-query batch. |
| `graph_ivf_overlap_candidate_recall/pq_overlap_l2/m16_k64_c4096_p4_d128_k10_recallbp10000_rows100005_m2407-full50000` | 7.766 ms (quick) | Wider PQ rerank has no quality upside over 1024 candidates after four-list probing and mostly measures exact-rerank cost. |
| `graph_ivf_overlap_candidate_recall/binary_overlap_l2/sign_c256_p1_d128_k10_recallbp5000_rows24996_m2375-full50000` | 411.1 µs (quick) | Binary Hamming keeps the same recall failure mode as PQ under one-list probing, so the candidate producer, not the compressed scorer width, is the limiting factor here. |
| `graph_ivf_overlap_candidate_recall/binary_overlap_l2/sign_c256_p4_d128_k10_recallbp10000_rows100005_m2375-full50000` | 711.7 µs (quick) | Four-list probing restores full recall and remains about 3.4x faster than the IVF+PQ full-recall p4 row at similar compressed/coarse memory. |
| `graph_ivf_overlap_candidate_recall/binary_overlap_l2/sign_c1024_p1_d128_k10_recallbp5000_rows24996_m2375-full50000` | 1.013 ms (quick) | Wider exact rerank cannot recover missing coarse lists; recall remains 5000 bp. |
| `graph_ivf_overlap_candidate_recall/binary_overlap_l2/sign_c1024_p4_d128_k10_recallbp10000_rows100005_m2375-full50000` | 1.821 ms (quick) | Wider binary rerank has no recall upside over c256 after p4 and is slower, confirming c256 remains the overlap-profile knee. |
| `graph_ivf_overlap_candidate_recall/turbo_overlap_cos/tqplus4lut_c1024_p1_d128_k10_recallbp5125_rows24996_m7454-full50000` | 2.2772 ms (quick) | Cosine-oracle TurboQuant overlap row has the same one-list coarse miss pattern as the L2 PQ/binary rows: wider compressed rerank cannot recover hits absent from the probed lists. |
| `graph_ivf_overlap_candidate_recall/turbo_overlap_cos/tqplus4lut_c1024_p4_d128_k10_recallbp10000_rows100005_m7454-full50000` | 5.2229 ms (quick) | Four-list probing restores full recall, but calibrated 4-bit byte-LUT scoring is slower than IVF+PQ p4 and far slower than binary p4 on this overlap fixture. |

PR-local production IVF candidate-pressure spot-check:

| Bench | 10k | Notes |
|---|---:|---|
| `graph_ivf_candidate_pressure/cluster_cos/d128_k10_w1_idbp9750_dqbp9750_lists100ne100max137avg100avgq100maxq137...` | 93.19 µs (quick) | Production IVF index with 100 lists, all non-empty, max list 137, average list 100. Width 1 is close to HNSW latency but misses one oracle hit across the 16-query clustered-cosine fixture. |
| `graph_ivf_candidate_pressure/cluster_cos/d128_k10_w2_idbp10000_dqbp10000_lists100ne100max137avg100avgq200maxq274...` | 140.96 µs (quick) | High-recall knee after the clean-index fast path: perfect recall at about 200 average candidates/query and 274 worst-case candidates/query, making this the production-IVF pressure baseline for 60/40 read/write planning. |
| `graph_ivf_candidate_pressure/cluster_cos/d128_k10_w4_idbp10000_dqbp10000_lists100ne100max137avg100avgq400maxq548...` | 254.74 µs (quick) | Keeps perfect recall but doubles candidate pressure versus width 2; useful as the first guardrail for less separable future fixtures. |
| `graph_ivf_candidate_pressure/cluster_cos/d128_k10_w64_idbp10000_dqbp10000_lists100ne100max137avg100avgq6400maxq8768...` | 2.9860 ms (quick) | Width 64 is excessive on this corpus: it scans about 64% of the corpus per query on average and mainly bounds the high-probe tail. |

PR-local explicit IVF target-centroid sweep:

| Variant | 1k perfect-recall knee | 10k perfect-recall knee | Notes |
|---|---:|---:|---|
| `ivf_default` | width 2 / 94.80 µs | width 2 / 139.67 µs | Default `ceil(sqrt(n))` list count: 32 lists at 1k, 100 lists at 10k. Width 2 reaches 10000 bp recall/quality at both scales and remains the current 10k knee. |
| `ivf_c16` | width 2 / 82.25 µs | width 1 / 621.23 µs | Low list count keeps recall perfect but creates very large 10k lists (`avg625`, max ~2k), so read cost scales poorly despite low centroid-scoring overhead. |
| `ivf_c128` | width 4 / 66.46 µs | width 2 / 137.30 µs | At 1k, 128 lists with width 4 is the fastest perfect-recall row; at 10k, width 2 is slightly faster than default while using more centroid-scoring work. |
| `ivf_c512` | no perfect row at width <= 4 | width 2 / 230.78 µs | High list count reduces candidate rows but pays heavy centroid scoring. On 1k it only reaches 7750 bp at width 4; on 10k width 2 is perfect but slower than default/c128 width 2. |

After graph-level ANN row-hit conversion stopped re-heaping already bounded index
hits, the same quick run measured width 1 / 2 / 4 / 64 at 92.38 µs / 140.91 µs /
254.37 µs / 2.997 ms. The delta is intentionally treated as noise-scale at
`k=10`; the value is avoiding redundant heap work on the read path while
preserving dead-row filtering and deterministic `NodeId` tie ordering.

The same pressure bench also carries a `k=50` top-k sweep to catch wider
agent-memory retrieval requests. On the 10k clustered-cosine fixture, width 1
falls to 8725 bp recall/quality at ~112.4 µs, while width 2 restores 10000 bp at
~160.8 µs. Width 4 / 8 / 64 remain perfect-recall guardrails at ~274.4 µs /
~494.1 µs / ~3.065 ms.

PR-local IVF incremental-insert drift spot-check. Bench IDs include
`_d{basis_points}bp`; the suffix also includes pending-retrain pressure as
`pend{count}pdbp{basis_points}`.

| Drift | Mode | 1k width-2 | 1k recall/quality | 10k width-2 | 10k recall/quality | Notes |
|---:|---|---:|---:|---:|---:|---|
| 1% | incremental | 151.48 µs | 7063 bp | 108.06 µs | 3188 bp | Tiny novel clusters can be badly missed at default width before retraining. |
| 1% | rebuilt | 37.70 µs | 3500 bp | 92.06 µs | 10000 bp | At 1k, retraining too early hurts width-2 recall; at 10k it restores recall and lowers latency. |
| 5% | incremental | 161.46 µs | 8500 bp | 125.89 µs | 7313 bp | Drift becomes visible but still scale-sensitive. |
| 5% | rebuilt | 39.11 µs | 6000 bp | 90.99 µs | 10000 bp | 10k has enough novel mass for rebuild to be clearly useful. |
| 10% | incremental | 176.51 µs | 9563 bp | 150.12 µs | 8250 bp | Matches the original 10% drift signal: default width is degraded before retrain. |
| 10% | rebuilt | 38.23 µs | 10000 bp | 121.81 µs | 10000 bp | Rebuild restores default-width recall once the novel cluster is large enough. |

Width-64 guardrail rows still recover recall without rebuild, but they are much
slower than width-2 on the same fixture: 1k ranges from ~357-400 µs incremental
and ~342-370 µs rebuilt; 10k ranges from ~2.38-3.26 ms incremental and
~2.35-3.13 ms rebuilt. This supports a measured retrain policy over simply
raising IVF's default probe width under the 60% read / 40% write workload.

PR-local ANN recall spot-check:

| Bench | 10k | Notes |
|---|---:|---|
| `graph_ann_recall_validation/cluster_cos_hnsw_d128_k10_ef10_idbp9875_dqbp9875` | 108.25 µs (quick) | Default `M=18, ef_construction=64`; suffix starts `m2491-7491_n10k_he10k...`, so compact HNSW level-0 storage keeps index-owned memory around 2.43 MiB while ID-overlap and distance-quality recall stay at 9875 bp on this 10k corpus. |
| `graph_ann_recall_validation/cluster_cos_hnsw_m24ef64_d128_k10_ef10_idbp10000_dqbp10000` | 130.78 µs (quick) | Configured `M=24, ef_construction=64`; suffix starts `m2975-7975_n10k_he10k...`. Reaches 10000 bp ID-overlap and distance-quality recall, but costs ~21% slower ef10 search and ~19% more index memory than the default HNSW row. |
| `graph_ann_recall_validation/cluster_cos_ivf_d128_k10_ef10_idbp10000_dqbp10000` | 768.18 µs (quick) | First IVF recall row for the same 10k corpus; suffix starts `m615-5665_n10k_ve10k...c100q100a10k`, so IVF uses far less index-owned memory than HNSW and reaches 10000 bp recall/quality, but current probe/rerank defaults are much slower. |

PR-local IVF probe sweep:

| Bench | 10k | Notes |
|---|---:|---|
| `graph_ann_recall_validation/cluster_cos_ivf_d128_k10_ef1_idbp9750_dqbp9750` | 128.38 µs (quick) | One-list IVF probe is close to tuned-HNSW latency but misses one exact-oracle hit on the 16-query clustered-cosine fixture. |
| `graph_ann_recall_validation/cluster_cos_ivf_d128_k10_ef2_idbp10000_dqbp10000` | 194.98 µs (quick) | Two-list IVF probe restores 10000 bp ID-overlap and distance-quality recall while cutting latency ~3.9x versus ef10 and ~22x versus ef64 on this fixture. |
| `graph_ann_recall_validation/cluster_cos_ivf_d128_k10_ef4_idbp10000_dqbp10000` | 349.02 µs (quick) | Still perfect on this corpus, but extra probes are already dominated by exact rerank work. |

PR-local IVF batch candidate-production spot-check:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_ivf_pressure --filter graph_ivf_candidate_pressure/cluster_cos/d128_k10_w2 --save-baseline ivf_batch_parallel_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_ivf_pressure --filter graph_ivf_candidate_pressure/cluster_cos/d128_k10_w2 --baseline ivf_batch_parallel_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_ivf_pressure --filter 'graph_ivf_candidate_pressure/cluster_cos/d128_k10_w1|graph_ivf_candidate_pressure/cluster_cos/d128_k10_w4' --save-baseline ivf_batch_parallel_widths_pre`;
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_ivf_pressure --filter 'graph_ivf_candidate_pressure/cluster_cos/d128_k10_w1|graph_ivf_candidate_pressure/cluster_cos/d128_k10_w4' --baseline ivf_batch_parallel_widths_pre`.

| Bench | Serial batch | Parallel IVF batch | Notes |
|---|---:|---:|---|
| `graph_ivf_candidate_pressure/cluster_cos/d128_k10_w1_idbp9188_dqbp9188` | 50.265 µs | 42.790 µs | IVF now produces independent batch query candidates through a parallel read-only path once query count and estimated probed work clear the threshold. Criterion reports a 14.88% improvement (`p=0.00`). |
| `graph_ivf_candidate_pressure/cluster_cos/d128_k10_w2_idbp10000_dqbp10000` | 67.843 µs | 50.413 µs | The width-2 high-recall knee improves 25.41% (`p=0.00`) while preserving the existing batch-equals-single API contract. |
| `graph_ivf_candidate_pressure/cluster_cos/d128_k10_w4_idbp10000_dqbp10000` | 78.518 µs | 52.602 µs | Wider probing benefits more from parallel candidate production, improving 33.75% (`p=0.00`). |
| `graph_ivf_candidate_pressure/cluster_cos/d128_k10_w16_idbp10000_dqbp10000` | 197.13 µs | 81.009 µs | The same neighbor guard command also matched the `w16` row; high probe width improves 58.74% (`p=0.00`) but remains a guardrail rather than the preferred recall knee. |

PR-local mixed vector read/write spot-check:

| Bench | 1k | 10k | Notes |
|---|---:|---:|---|
| `graph_vector_mixed_workload/ivf_cos_dim128_k10_r60w40_ef2` | 2.09 ms | 7.48 ms | One measured cycle interleaves 60 IVF cosine ANN reads and 40 vector-property updates over a 128-dim index. Fixture build is excluded; timed replacement writes reuse IVF entries, so routine updates no longer add stale IVF rows before rebuild compaction. |
| `graph_vector_mixed_workload/point_read_ivf_update_r60w40_dim128` | 1.625 ms | 6.978 ms | Same IVF vector fixture and 40 indexed vector-property updates, but the 60 reads are point `node_properties` lookups. This isolates routine IVF update maintenance from ANN query cost. |
| `graph_vector_mixed_workload/write_ivf_update_w40_dim128` | 1.5064 ms | n/a | Write-only companion over the same IVF fixture. Caching each entry's assigned list avoids recomputing the old centroid list and preserves same-list position on replacement, improving the isolated update row from 1.5604 ms by 3.28% (`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_mixed_workload --filter write_ivf_update_w40_dim128 --baseline ivf_entry_list_write_pre`). |
| `graph_vector_mixed_workload/point_read_tqcos_update_r60w40_dim128` | 1.4999 ms | n/a | Same 60 point reads / 40 indexed updates shape for production `TurboQuantCosine`. In-place replacement keeps the row's slot stable instead of remove-and-append churn; the high-fidelity 1k comparison against the old path was polluted by a noisy baseline, so this row is a guardrail rather than a headline speedup. |
| `graph_vector_mixed_workload/write_tqcos_update_w40_dim128` | 1.4785 ms | n/a | Write-only companion over the same `TurboQuantCosine` fixture. Criterion reported the +0.56% timing delta versus the old path as within noise (`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench vector_mixed_workload --filter write_tqcos_update_w40_dim128 --baseline tq_inplace_write_pre`), so the row protects replacement maintenance from regressing while unit tests assert stable slots and zero deleted entries. |
| `graph_vector_mixed_workload/ivf_cos_dim128_k10_r60w40x10_ef2_maint_cap1` | 51.67 ms | 84.64 ms | Ten measured cycles run 60 reads / 40 writes per cycle across four IVF cosine indexes, then rebuild at most one recommended index. Each index reaches 100 pending retrain updates before maintenance. |
| `graph_vector_mixed_workload/ivf_cos_dim128_k10_r60w40x10_ef2_maint_all` | 57.23 ms | 116.28 ms | Same fixture and 10-cycle workload, but maintenance rebuilds every recommended IVF index. At 10k this isolates the cost of rebuilding four drifted indexes instead of pacing maintenance one index at a time. |

## §3 selene-graph — write pipeline & concurrency

Bench bins: `write_txn_lifecycle`, `provider_fanout`, `bound_type_validation`,
`concurrent_writers`, `graph_hub_delete`, `graph_delete_reclamation`,
`graph_read_under_write`, `graph_mixed_workload`.

### §3a Write-pipeline microbenches

_The `write_txn_lifecycle` rows in the table below were re-measured 2026-08-16
(same M5 box, rustc 1.97.1) via `scripts/run-benches.sh --profile full --bench
write_txn_lifecycle`. Every other row in §3a still carries the file-level
2026-06-01 stamp._

`write_txn_lifecycle` create/delete rows below show the **batch axis at the 100k
fixture** (the headline scale); `empty_commit` shows the scale axis.

The `graph_clone` / `begin_rollback` rows are commit-floor attribution
instruments: the committer handoff has no direct row (`seal` is
crate-private), so derive it as `empty_commit − graph_clone −
begin_rollback`. From the 2026-08-16 same-run medians (12.25 / 35.14 / 50.12 µs
`empty_commit`, 1.106 / 11.43 / 24.6 µs `graph_clone`, 14 ns `begin_rollback`),
the derived handoff is **~11.1 / ~23.7 / ~25.5 µs**.

**The E3 conclusion still holds, but only below 100k.** The handoff outweighs
the snapshot clone 10:1 at 10k and 2:1 at 50k, so it remains the term to attack
for the empty-commit floor. At 100k it no longer dominates: ~25.5 µs of handoff
against 24.6 µs of clone is a near-even split, because #1118's chunked-tree
clone costs slightly more at the largest scale (see the persistent-map A/B
below) while the handoff barely moved. Any future B26-style work should state
which scale it targets rather than citing one handoff figure.

The prior attribution was measured with
`scripts/run-benches.sh --profile full --bench write_txn_lifecycle --save-baseline pre`:
same-run `empty_commit` medians were 12.10 / 37.99 / 47.53 µs, giving a
derived handoff of ~11.0 / ~27.5 / ~23.9 µs. A local
drop-relocation probe (`ArcSwap::swap` old snapshot returned through the commit
ack) regressed `empty_commit` to 39.50 µs at 50k (+4.0%) and 50.82 µs at 100k
(+6.9%), so it was rejected rather than landed.

The `indexed_*` rows use a no-edge fixture with a high-cardinality typed key and
populated composite, flat-vector, and text indexes. They isolate index registry
clone cost from adjacency/storage clone cost; current results show index data is
already mostly Arc-backed and does not dominate the commit floor.

| Bench | Variant | Median | Notes |
|---|---|---:|---|
| `write_txn_lifecycle/empty_commit` | 10k / 50k / 100k | 12.25 / 35.14 / 50.12 µs | Empty-transaction commit floor. Re-measured 2026-08-16; 100k +5.4%. |
| `write_txn_lifecycle/graph_clone` | 10k / 50k / 100k | 1.11 / 11.43 / 24.60 µs | One full `SeleneGraph` clone + drop — the snapshot fork `seal`'s first `guard_mut` pays. Re-measured 2026-08-16; 100k +4.3%, the priced-in #1118 chunked-tree tradeoff. |
| `write_txn_lifecycle/indexed_empty_commit` | 10k / 50k / 100k | 11.28 / 32.41 / 37.42 µs | No-edge typed/composite/vector/text indexed fixture; measured with `--filter write_txn_lifecycle/indexed`. Re-measured 2026-08-16; **10k −48.4%**. |
| `write_txn_lifecycle/indexed_graph_clone` | 10k / 50k / 100k | 0.199 / 2.24 / 5.42 µs | Index-rich clone attribution row; populated indexes are Arc-backed enough that registry clone is not the dominant term. Re-measured 2026-08-16; flat. |
| `write_txn_lifecycle/begin_rollback` | 10k / 50k / 100k | 14.0 ns (flat) | Write-lock + allocator + `WriteTxn` build, no snapshot fork, no handoff. Re-measured 2026-08-16; still flat across scales. |
| `write_txn_lifecycle/create_only` @100k | batch 1 / 10 / 100 / 1000 | 140 / 144 / 180 / 356 µs | Isolated node create + commit. Re-measured 2026-08-16; **−59% / −60% / −62% / −70%**. |
| `write_txn_lifecycle/delete_only` @100k | batch 1 / 10 / 100 / 1000 | 125 / 130 / 153 / 493 µs | Fixture seed excluded from timed body. Re-measured 2026-08-16; **−44% / −44% / −51% / −34%**. |
| `provider_fanout/core_only` | providers=core | 258.7 µs | Commit-notification baseline. |
| `provider_fanout/extra_k1` / `k4` / `k16` | extra providers | 223.8 / 225.6 / 227.6 µs | No-op provider fanout — flat (notification is cheap). |
| `provider_fanout/extra_k4_with_error_one` | extra=4 + error | 227.4 µs | Error-path notification scaling. |
| `provider_fanout/extra_k4_with_panic_one` | extra=4 + panic | n/a | Opt-in `SELENE_BENCH_INCLUDE_PANIC_PROVIDER=1`. |
| `provider_fanout/active_set_edge_create_k40` | 40 edge creates + active-set provider | 283.2 µs | In-memory commit/provider path for `CONTRADICTS`-style active-set removal; no WAL. |
| `provider_fanout/active_set_edge_delete_k40` | 40 edge deletes + active-set provider | 218.8 µs | Delete path uses provider-owned `edge_id -> source` state to reinsert active nodes; seed excluded from timed body. |
| `provider_fanout/active_set_wal_edge_create_k40` | 40 edge creates + WAL + active-set provider | 4.75 ms | Core WAL durability plus provider removal; provider state itself remains in-memory. |
| `provider_fanout/active_set_wal_edge_delete_k40` | 40 edge deletes + WAL + active-set provider | 4.21 ms | Core WAL durability plus provider reinsertion; seed excluded from timed body. |
| `provider_fanout/active_hint_recent_edge_create_k40` | 40 `RECENT_IN` creates + active-hint provider | 242.5 µs | Maintains window→member state in provider memory; no WAL. |
| `provider_fanout/active_hint_recent_edge_delete_k40` | 40 `RECENT_IN` deletes + active-hint provider | 199.4 µs | Delete path uses provider-owned edge provenance to remove window members. |
| `provider_fanout/active_hint_wal_recent_edge_create_k40` | 40 `RECENT_IN` creates + WAL + active-hint provider | 4.78 ms | Core WAL durability dominates active-hint membership maintenance. |
| `provider_fanout/active_hint_wal_recent_edge_delete_k40` | 40 `RECENT_IN` deletes + WAL + active-hint provider | 4.41 ms | WAL-backed delete path remains near the active-set WAL boundary. |
| `provider_fanout/active_hint_dependency_edge_create_k40` | 40 `DEPENDS_ON` creates + active-hint provider | 300.3 µs | Maintains anchor→dependency state for one broad task anchor; no WAL. |
| `provider_fanout/active_hint_dependency_edge_delete_k40` | 40 `DEPENDS_ON` deletes + active-hint provider | 199.4 µs | Delete path removes dependency targets through provider-owned edge provenance. |
| `provider_fanout/active_hint_wal_dependency_edge_create_k40` | 40 `DEPENDS_ON` creates + WAL + active-hint provider | 4.57 ms | Core WAL durability dominates dependency maintenance. |
| `provider_fanout/active_hint_wal_dependency_edge_delete_k40` | 40 `DEPENDS_ON` deletes + WAL + active-hint provider | 4.55 ms | WAL-backed dependency deletes stay in the same cost band as active-set deletes. |
| `bound_type_validation/unbound_commit` | 10k / 50k / 100k | 291 / 246 / 320 µs | Commit without graph-type validation. |
| `bound_type_validation/bound_commit_simple` | 10k / 50k / 100k | 304 / 250 / 350 µs | Typed-commit validation delta (small). |
| `bound_type_validation/bound_commit_unique` | 1k quick | 108.11 µs | Unique declaration present, but the 100-write batch updates a non-unique property and stays on the delta gate. Command: `scripts/run-benches.sh --profile quick --bench bound_type_validation --filter bound_commit_unique/1000`. |
| `bound_type_validation/bound_commit_unique_value_update` | 1k quick | 320.91 µs | 100 unique string property updates validated through delta-scoped candidate conflict checks instead of rebuilding all unique-property state. Command: `scripts/run-benches.sh --profile quick --bench bound_type_validation --filter bound_commit_unique_value_update`. |
| `bound_type_validation/bound_commit_rich` | 10k / 50k / 100k | 1.01 / 1.14 / 1.67 ms | Wider type-graph validation delta. |
| `bound_type_validation/bound_schema_change` | 10k / 50k / 100k | 2.92 / 18.6 / 39.3 ms | Full graph-state revalidation; scales with N. |
| `bound_type_validation/bound_commit_descriptor_insert` | 10k / 50k / 100k | 354 / 360 / 635 µs | 100 creates with bounded `STRING` and `BYTES` descriptors; post-B8 in-envelope coercion reuses shared storage. |
| `bound_type_validation/bound_commit_descriptor_update` | 10k / 50k / 100k | 362 / 499 / 565 µs | 100 updates over bounded descriptor properties; post-B8 property diffs mutate values in place. |

PR-local persistent-map migration A/B:

Commands:
`scripts/run-benches.sh --profile full --bench write_txn_lifecycle --filter 'write_txn_lifecycle/(graph_clone|create_only|delete_only)' --save-baseline imbl-retire-pre`
and
`scripts/run-benches.sh --profile full --bench write_txn_lifecycle --filter 'write_txn_lifecycle/(graph_clone|create_only|delete_only)' --baseline imbl-retire-pre`.
The empty-commit control was repeated at 10k with
`scripts/run-benches.sh --profile full --bench write_txn_lifecycle --filter 'write_txn_lifecycle/empty_commit/10000$' --save-baseline imbl-retire-empty-repeat`
and the matching `--baseline imbl-retire-empty-repeat` invocation;
its 16.22 / 16.41 µs medians were statistically unchanged.

| Bench | Before | After | Median delta | Notes |
|---|---:|---:|---:|---|
| `write_txn_lifecycle/graph_clone/10000` | 1.093 µs | 1.066 µs | -2.40% | Small snapshot forks improve slightly. |
| `write_txn_lifecycle/graph_clone/50000` | 10.336 µs | 11.497 µs | +11.23% | The chunked-tree clone microbenchmark pays a measurable mid-scale tradeoff. |
| `write_txn_lifecycle/graph_clone/100000` | 23.614 µs | 24.746 µs | +4.79% | The large clone-only row regresses modestly while the empty-commit control stays flat. |
| `write_txn_lifecycle/create_only/n100000/100` | 268.71 µs | 196.77 µs | -26.77% | Batched label and id-map mutations path-copy less state. |
| `write_txn_lifecycle/create_only/n100000/1000` | 958.15 µs | 389.63 µs | -59.34% | The larger create batch amortizes chunked-tree updates. |
| `write_txn_lifecycle/delete_only/n100000/100` | 509.75 µs | 155.21 µs | -69.55% | Batched persistent-map removals avoid repeated HAMT update overhead. |
| `write_txn_lifecycle/delete_only/n100000/1000` | 3.600 ms | 487.13 µs | -86.47% | The largest delete batch is the strongest product-path gain. |

PR-local B7 incident-edge revalidation A/B:

Commands:
`scripts/run-benches.sh --profile full --bench bound_type_validation --filter bound_commit_incident_property_update --save-baseline b7_pre`
and
`scripts/run-benches.sh --profile full --bench bound_type_validation --filter bound_commit_incident_property_update --baseline b7_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `bound_type_validation/bound_commit_incident_property_update/10000` | 1.8135 ms | 75.443 µs | Property-only `NodeUpdated` no longer revalidates every typed incident edge. |
| `bound_type_validation/bound_commit_incident_property_update/50000` | 11.704 ms | 127.82 µs | Degree-sized fan-out collapses to the typed node-update floor. |
| `bound_type_validation/bound_commit_incident_property_update/100000` | 24.775 ms | 245.09 µs | Post row was noisy but still significant: Criterion reported −99.011% median time. |

PR-local B8 descriptor-coercion A/B:

Commands:
`scripts/run-benches.sh --profile full --bench bound_type_validation --filter bound_commit_descriptor --save-baseline b8_pre`
and
`scripts/run-benches.sh --profile full --bench bound_type_validation --filter bound_commit_descriptor --baseline b8_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `bound_type_validation/bound_commit_descriptor_insert/10000` | 356.65 µs | 354.09 µs | No statistically significant change; small row stays neutral. |
| `bound_type_validation/bound_commit_descriptor_update/10000` | 377.19 µs | 362.34 µs | No statistically significant change; directionally faster. |
| `bound_type_validation/bound_commit_descriptor_insert/50000` | 386.44 µs | 359.62 µs | Significant −6.94% median time from descriptor storage reuse. |
| `bound_type_validation/bound_commit_descriptor_update/50000` | 530.54 µs | 499.31 µs | Criterion marked the −5.89% median shift within its noise threshold. |
| `bound_type_validation/bound_commit_descriptor_insert/100000` | 649.94 µs | 634.95 µs | No statistically significant change; row was noisy. |
| `bound_type_validation/bound_commit_descriptor_update/100000` | 589.74 µs | 564.58 µs | Significant −4.27% median time on descriptor updates. |
| `graph_mixed_workload/point_read_update_r60w40` | 10k / 50k / 100k | 9.207 / 11.045 / 16.699 ms | One scalar cycle: 60 snapshot point reads interleaved with 40 non-indexed property-update commits. Fixture clone/setup excluded; no vector index or WAL. |
| `graph_mixed_workload/point_read_indexed_update_r60w40` | 10k / 50k / 100k | 9.261 / 11.129 / 16.842 ms | Same scalar cycle, but the 40 writes update `Person.age`, a registered typed property index. The close delta to the non-indexed row keeps property-index maintenance below the dominant sequential commit cost at these scales. |
| `graph_mixed_workload/candidate_state_edge_update_r60w40` | 10k / 50k / 100k | 3.196 / 5.310 / 11.826 ms | One maintained candidate-state cycle: 60 generation-checked `current` set reads plus 20 `SUPERSEDED_BY` edge deletes and 20 creates. Exercises provider reactivation and invalidation without WAL. |
| `graph_mixed_workload/candidate_state_metadata_edge_update_r60w40` | 10k / 50k / 100k | 2.976 / 4.333 / 9.922 ms | Same provider write cycle, but the 60 reads fetch generation-checked candidate-state metadata rather than materializing the full `current` set. The widening delta against the full-set row isolates set materialization cost. |
| `graph_mixed_workload/point_read_update_r60w40_wal` | 10k / 50k / 100k | 139.11 / 130.07 / 134.45 ms | Same scalar 60/40 cycle backed by a real per-iteration WAL tempdir with committer batching off. Setup/teardown excluded; the near scale-flat cost shows per-commit durability barriers dominate this sequential 40-write shape. |

PR-local candidate-state member-cache A/B:

Commands:
`scripts/run-benches.sh --profile full --bench graph_mixed_workload --filter candidate_state --save-baseline candidate_state_members_vec_full_pre`;
`scripts/run-benches.sh --profile full --bench graph_mixed_workload --filter candidate_state --baseline candidate_state_members_vec_full_pre`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_mixed_workload/candidate_state_edge_update_r60w40/10000` | 2.1069 ms | 1.9920 ms | -5.4552% | Maintained candidate-state members keep the `BTreeSet` update path and add a lazy sorted `Vec<NodeId>` cache, so repeated full-set reads clone contiguous canonical members instead of collecting from the tree; Criterion reports p=0.00. |
| `graph_mixed_workload/candidate_state_edge_update_r60w40/50000` | 5.1335 ms | 4.5895 ms | -10.596% | Larger maintained sets benefit more from amortizing full-set materialization across reads between writes; p=0.00. |
| `graph_mixed_workload/candidate_state_edge_update_r60w40/100000` | 11.814 ms | 10.768 ms | -8.8567% | The 100k full-set row keeps the same membership semantics while avoiding repeated tree collection; p=0.00. |
| `graph_mixed_workload/candidate_state_metadata_edge_update_r60w40/10000` | 1.9197 ms | 1.8751 ms | noise | Metadata reads do not materialize the full set and stayed statistically flat (`p=0.35`). |
| `graph_mixed_workload/candidate_state_metadata_edge_update_r60w40/50000` | 4.0672 ms | 4.0636 ms | noise | Keeping `BTreeSet` as the authoritative update structure avoids the plain-`Vec` write-regression shape; p=0.91. |
| `graph_mixed_workload/candidate_state_metadata_edge_update_r60w40/100000` | 9.8377 ms | 9.7083 ms | noise | Metadata/write-side guard remains flat at the largest full-profile scale (`p=0.27`). |

### §3b `graph_hub_delete` — high-degree hub deletion (GRAPH-05 ✓ shipped)

Deleting a node cascades over every incident edge. GRAPH-05 made adjacency
removal **in place**: the deleted node's own `adjacency_out`/`adjacency_in`
entries are dropped wholesale and incident edges are grouped by neighbor for
batched persistent-map updates. That avoids per-edge full-`SmallVec` clones and
the old O(D²) cascade; the curve below is linear. This sweeps the **degree**
axis (not node scale).

| Bench | degree=100 | degree=1000 | degree=10000 | Notes |
|---|---:|---:|---:|---|
| `graph_hub_delete` | 54.0 µs | 496 µs | 4.54 ms | Linear after GRAPH-05. Was 64.3 µs / 1.62 ms / 132.7 ms (O(D²)) — **30× faster at degree 10k**. |

PR-local B6 label-index removal A/B:

Commands:
`scripts/run-benches.sh --profile full --bench graph_hub_delete --save-baseline b6_pre`,
`scripts/run-benches.sh --profile full --bench graph_hub_delete --baseline b6_pre`,
`scripts/run-benches.sh --profile full --bench write_txn_lifecycle --filter write_txn_lifecycle/delete_only --save-baseline b6_pre`,
and
`scripts/run-benches.sh --profile full --bench write_txn_lifecycle --filter write_txn_lifecycle/delete_only --baseline b6_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_hub_delete/100` | 41.654 µs | 40.043 µs | At that historical baseline, label and edge-label bitmap removals began mutating through the persistent-map entry instead of cloning the whole bitmap per row. |
| `graph_hub_delete/1000` | 323.69 µs | 292.50 µs | Degree-1000 hub delete improves about 9.6% in this same-run full A/B. |
| `graph_hub_delete/10000` | 4.7949 ms | 3.5108 ms | The broad edge-label bitmap path is the main win: degree-10000 hub delete improves about 26.8%. |
| `write_txn_lifecycle/delete_only/n10000/1` | 93.417 µs | 93.511 µs | Guard row: single labeled-node delete stays neutral. |
| `write_txn_lifecycle/delete_only/n50000/100` | 381.39 µs | 342.29 µs | Mid-scale delete-only rows are historically noisy; this branch is modestly faster, not regressed. |
| `write_txn_lifecycle/delete_only/n100000/1000` | 3.5656 ms | 3.4152 ms | Large batch delete-only guard improves about 4.2% in this run. |

PR-local persistent-map adjacency A/B:

Commands:
`scripts/run-benches.sh --profile full --bench graph_hub_delete --save-baseline imbl-retire-pre`
and
`scripts/run-benches.sh --profile full --bench graph_hub_delete --baseline imbl-retire-pre`.

| Bench | Before | After | Median delta | Notes |
|---|---:|---:|---:|---|
| `graph_hub_delete/100` | 43.206 µs | 40.366 µs | -6.57% | Grouped neighbor cleanup offsets persistent-tree mutation overhead. |
| `graph_hub_delete/1000` | 307.03 µs | 285.74 µs | -6.93% | The batched adjacency path stays linear at degree 1k. |
| `graph_hub_delete/10000` | 3.655 ms | 3.105 ms | -15.05% | Sorted bulk key removal strengthens the high-degree path. |

PR-local incident-edge collector A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench graph_hub_delete --save-baseline hub-delete-btreeset-before`
and
`scripts/run-benches.sh --profile quick --bench graph_hub_delete --baseline hub-delete-btreeset-before`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_hub_delete/100` | 41.796 µs | 39.536 µs | Incident edge ids are collected into a contiguous `Vec`, sorted/deduped once, then cascaded in ascending id order; p=0.00. |
| `graph_hub_delete/1000` | 307.60 µs | 283.05 µs | Avoids per-edge `BTreeSet` node allocation while preserving deterministic cascade order; p=0.00. |

### §3c `graph_delete_reclamation` — delete payload clearing and compaction

`graph_delete_reclamation/*` isolates the storage side of deletes from vector
index maintenance. The fixture stores 768-dim `Value::Vector` payloads on
embedding nodes, deletes 10% of nodes, and reports the logical vector payload
cleared at delete time in the Criterion id suffix. Delete clears heavyweight
properties immediately while retaining dead rows for stable id mapping;
`compact_after_vector_delete` then measures row densification and asserts the
`CompactionReport` reclaims those dead rows.

| Bench | quick 1k | Notes |
|---|---:|---|
| `graph_delete_reclamation/vector_payload_delete/n1k_del100_dim768_payload300k` | 105.98 µs | Deletes 100 vector-bearing nodes and clears ~300 KiB of vector payload from the per-iteration graph; fixture clone/setup excluded. |
| `graph_delete_reclamation/compact_after_vector_delete/n1k_del100_dim768_payload300k` | 106.98 µs | Compacts the post-delete graph and asserts 100 dead node rows are reclaimed; delete setup excluded from the timed body. PR-local A/B replaced the temporary live-node endpoint-validation set hasher: 113.84 µs -> 106.98 µs (-6.45%, p=0.00). |

### §3d `graph_read_under_write` — lock-free reads under contention (D10)

Times a fixed read batch (8 threads × 20k = 160k reads) while one background
writer churns commits on the `ArcSwap` snapshot. The D10 promise is that a held
write lock never blocks a reader; a regression that puts reads behind the write
lock collapses this. Dual of `concurrent_writers` (which times the writers).

| Bench | 10k | 50k | 100k | Notes |
|---|---:|---:|---:|---|
| `graph_read_under_write` | 17.1 ms | 21.5 ms | 24.5 ms | ~107–153 ns/read; rises only with snapshot footprint, not lock contention. |

### §3e `concurrent_writers` — serialized writer queueing under contention

Thread fan-in arms sweep `[1, 2, 4, 8, 16, 32]` (representative `1/8/32` shown).
Two axes:

- **In-memory** (`threads{N}`, `threads{N}_with_readers8`) — no WAL; pure
  single-committer queueing + lock-free reads under contention. Group commit has
  nothing to coalesce here (no `fsync`), so it is not run on this axis.
- **WAL-backed** (`wal_threads{N}_batchOFF` vs `_batchON`) — a real on-disk WAL
  (tempdir per iteration; the committer is the sole `fsync` caller in
  `SyncPolicy::OnFlushOnly`). The only axis where group commit can win, because
  the win is coalesced `fsync` syscalls. `batchOFF` = `CommitBatching::Off` (one
  `fsync`/commit); `batchON` = `CommitBatching::DEFAULT_ON` (coalesce ≤64 commits
  / 8 MiB per `fsync`).

On `full`/`stress`, each WAL-backed arm also prints an **untimed**
`[concurrent_writers percentiles] … p50/p99/p999` line to stderr (the
tail-latency story the mean sample can't show).

_Re-measured 2026-08-16 (same M5 box, rustc 1.97.1) via
`scripts/run-benches.sh --profile full --bench concurrent_writers`._

| Bench | threads=1 | threads=8 | threads=32 | Notes |
|---|---:|---:|---:|---|
| `concurrent_writers/threads{N}` | 102.8 ms | 389.3 ms | 372.2 ms | In-memory; 1000 commits, 10 updates each. **Shape changed — see below.** |
| `concurrent_writers/threads{N}_with_readers8` | 171.3 ms | 319.3 ms | 305.5 ms | Same load + 8 snapshot readers. −76% / −50% / −53%. |
| `concurrent_writers/wal_threads{N}_batchOFF` | 4.023 s | 3.905 s | 3.857 s | Real WAL, one `fsync`/commit. Flat, as expected — `fsync` latency is the term. |
| `concurrent_writers/wal_threads{N}_batchON` | 3.800 s | 889.3 ms | **243.7 ms** | Group commit — **15.8× over batchOFF at 32 threads** (was 14×); ≈ batchOFF at 1 thread (nothing to coalesce). |

**The in-memory row is no longer flat, and that is the headline.** It used to
read 332 / 304 / 305 ms across 1 / 8 / 32 threads — near-flat, which said
single-committer queueing cost about the same however many writers queued. It
now reads 102.8 / 389.3 / 372.2 ms: the single-writer case got **3.2× faster**
while 8 and 32 writers got **~25% slower**.

Both halves point at the same place. Per-commit work genuinely fell this cycle
— `write_txn_lifecycle/create_only` is down 59–70% and `graph_mutation_commit_batch`
down 57–71% over the same window — so the uncontended writer finishes far
sooner. With the per-commit term that much cheaper, what remains at 8+ writers
is contention on the single committer, and it is now the dominant cost rather
than a rounding error on top of commit work. The crossover between "commit cost
dominates" and "queueing dominates" has moved down; it now sits between 1 and 2
writers on this fixture.

**Neither direction was bisected**, so treat the mechanism as the leading
explanation rather than a measured attribution. The `_with_readers8` row
improving 50–76% across the board while the writer-only row regresses is the
part that most deserves a follow-up: it says lock-free reads under contention
got cheaper at the same time writer queueing got more expensive.

The WAL axis is unaffected in shape — `fsync` latency still dominates it, group
commit still coalesces, and the 32-thread group-commit win got slightly better.

## §4 selene-persist — WAL & snapshot

Bench bins: `wal`, `snapshot`, `store_control`, plus `graph_snapshot_roundtrip` (lives in the
`selene-graph` crate but exercises the persist/D14 path end to end).

### Empty-store directory/control overhead (F02-PR01)

The `store_control` binary measures four `persist_store_control` rows over one
empty store (no graph nodes, transactions, or format-2 data WAL):

| Row | Inside the clock | Outside the clock |
|---|---|---|
| `anchor` | Ambient directory open, initial diagnostic canonicalization, metadata validation, handle drop | Empty directory creation/removal |
| `open_empty` | Writer/epoch lock acquisition, bounded CURRENT/manifest validation, handle drop | Initial generation-1 creation and retained directory anchoring |
| `create_empty` | Identity construction, locks, staging, encoding, file syncs, exclusive immutable/CURRENT publication and both directory barriers | Directory creation/anchoring and output/fixture destruction |
| `publish_empty` | Generation-1 validation, locks, generation-2 publication with both file syncs and directory barriers | Generation-1 creation, directory anchoring and output/fixture destruction |

Create/publication use Criterion `BatchSize::PerIteration`; returned handles and
fixtures are destroyed outside the timed interval. No sync is suppressed. Open
uses a warm generation-1 store; publication starts fresh at generation 1 for
every iteration, so these rows do not claim deep-history open performance.

The companion `persist_store_control_history` group measures successful reopen
at selected generations **1, 32 and 256**, first with every manifest retained,
then with all unselected manifests removed. Each history is created with real
file/directory syncs before measurement. Offline fixture removal holds writer
ownership, occurs with no readers, and syncs the directory; this setup and final
fixture destruction are outside the clock. Reopen includes writer/shared-epoch
acquisition, directory scan/stat validation, the two selected payload reads,
decode/checksum/identity validation and handle destruction. All rows are warm
filesystem-cache measurements; no transaction data or queries are involved.

```bash
scripts/run-benches.sh --bench store_control --compile-only
scripts/run-benches.sh --profile quick --bench store_control --sample-size 20 --measurement-time 1
```

#### Pre-pivot measurements (historical)

The following earlier numbers precede the owner-directed unified-writer and
selected-state-only pivot; they are not current-contract performance evidence.
Measured 2026-09-09 on Apple M5 (10 cores, 16 GiB), macOS 27.0 build 26A5425a,
internal APFS Data volume, rustc 1.97.1, mimalloc, optimized bench profile with
thin LTO. Candidate worktree base: `d0aaf40b`. Each row used 20 samples, a 100 ms
warm-up and a 1 s requested measurement window; setup is excluded as above.

| Row | Criterion time estimate [95% interval] | Sample median | Sample p95 |
|---|---:|---:|---:|
| `anchor` | 15.477 µs [15.374, 15.577] | 0.015 ms | 0.016 ms |
| `open_empty` | 67.734 µs [66.919, 68.770] | 0.069 ms | 0.073 ms |
| `create_empty` | 14.997 ms [14.854, 15.141] | 14.983 ms | 15.451 ms |
| `publish_empty` | 15.335 ms [15.069, 15.572] | 15.468 ms | 15.989 ms |

Sample medians/p95 were obtained with `scripts/criterion-summary.sh
persist_store_control/anchor persist_store_control/open_empty
persist_store_control/create_empty persist_store_control/publish_empty`.
An earlier run estimated 15.600 µs, 79.482 µs, 14.157 ms and 14.423 ms respectively;
its open row had four high-severe outliers. The final repeat reported create and
publication approximately 5.93% and 6.32% slower than that run, while open was
faster. This was not an isolated optimization A/B, so no cause or speedup is
claimed. These are startup/control costs, not a regression threshold; neither
run measures queries, cold-cache startup, long manifest history, or power loss.

#### Unified writer proof and selected-state-only reopen

Re-run with the commands above on the same native M5/APFS host and Rust 1.97.1,
after the owner-directed pivot. The optimized/mimalloc configuration, 20 samples,
100 ms warm-up and requested 1 s measurement window are unchanged.

| Row | Criterion time estimate [95% interval] | Sample median | Sample p95 |
|---|---:|---:|---:|
| `anchor` | 16.567 µs [16.480, 16.636] | 0.017 ms | 0.017 ms |
| `open_empty` | 76.368 µs [73.965, 79.226] | 0.073 ms | 0.085 ms |
| `create_empty` | 13.129 ms [12.933, 13.321] | 13.201 ms | 13.626 ms |
| `publish_empty` | 14.951 ms [14.272, 15.727] | 14.923 ms | 18.085 ms |

| History row | Criterion time estimate [95% interval] | Sample median | Sample p95 |
|---|---:|---:|---:|
| `retained/1` | 74.696 µs [73.941, 75.346] | 0.074 ms | 0.076 ms |
| `pruned/1` | 75.785 µs [74.970, 76.490] | 0.076 ms | 0.077 ms |
| `retained/32` | 120.55 µs [115.52, 125.55] | 0.115 ms | 0.130 ms |
| `pruned/32` | 77.301 µs [75.104, 80.164] | 0.078 ms | 0.084 ms |
| `retained/256` | 371.02 µs [365.51, 378.01] | 0.373 ms | 0.396 ms |
| `pruned/256` | 74.341 µs [72.850, 75.769] | 0.073 ms | 0.077 ms |

Per-capability unit instrumentation proves exactly two control payload opens at
these generations, including after history removal and for ordinary publication
base validation. **Overall reopen is not constant-time**: retained-history
directory enumeration/stat work and its entry-name vector remain O(entries),
as the retained rows demonstrate. Removing history leaves the same selected
state and avoids those extra entry scans; no production history-prune API or
epoch-reset protocol is introduced here.

Criterion compared cached earlier results and reported anchor/open estimates
about 6.51%/9.55% higher, create about 12.46% lower, and no significant publication
change. These runs are not an isolated optimization A/B and no causal speedup
is claimed. Publication had three high outliers. No sync barrier was removed to
obtain these numbers. Native process/fault evidence is not power-loss proof.

### §4a WAL

_§4a re-measured 2026-08-16 (same M5 box, rustc 1.97.1) via
`scripts/run-benches.sh --profile full --bench wal`. The file-level
2026-06-01 stamp still governs every other section._

`scale` = WAL entries, not graph nodes. `_no_fsync` rows use
`SyncPolicy::OnFlushOnly` (append/threshold/drop fsync suppressed; a caller
`flush()` would still sync).

| Bench | 10k | 50k | 100k | 100k Δ | Notes |
|---|---:|---:|---:|---:|---|
| `persist_wal_append_single` | 58.49 ms | 295.5 ms | 604.7 ms | −4.2% | Single-entry loop, `EveryN(1000)`. |
| `persist_wal_append_single_no_fsync` | 12.4 ms | 59.03 ms | 120.2 ms | **+8.1%** | Donor-parity diagnostic, no append fsync. |
| `persist_wal_append_batch_1000` | 6.295 ms | 8.585 ms | 10.28 ms | −18.3% | 1000-change entries — **~59× faster than per-entry at 100k**. |
| `persist_wal_append_batch_1000_no_fsync` | 2.239 ms | 3.997 ms | 6.092 ms | −26.4% | Batched, no flush in timed body. |
| `persist_wal_replay` | 3.882 ms | 15.15 ms | 22.83 ms | −29.3% | Fixed-layout header + xxh3 + BufReader. |
| `persist_wal_open_scan` | 233.79 µs | 1.21 ms | 2.228 ms | **+45.5%** | Writer reopen validation scan. |

**The two regressions are the priced-in cost of WAL 3.0.** The window between
the two measurements contains `bfb904c4` (#1108, "bring WAL framing under
integrity protection"), which widened the entry prefix to 40 bytes and put a
checksum on it. `append_single_no_fsync` pays that on every write and
`open_scan` verifies it on every entry — at 100k, +0.7 ms over the scan is
~7 ns/entry, the right order for an xxh3 over 40 bytes. Both are consistent
with that single cause; neither was bisected, so treat the attribution as
the leading explanation rather than a measured one.

Replay moving the other way (−29.3%) is the same change paying for itself: a
prefix checksum lets replay reject a bad entry without decoding its body.

#### `persist_wal_open_scan` — writer reopen validation (B16)

Measures `WalWriter::open` over an existing WAL with `scale` single-change
entries. The timed body covers file open/lock, file header read, entry-header
scan, payload checksum validation, and final committed-offset positioning; the
WAL fixture is created outside the timed body.

Commands:

```bash
scripts/run-benches.sh --profile full --bench wal --filter persist_wal_open_scan --save-baseline b16_pre
scripts/run-benches.sh --profile full --bench wal --filter persist_wal_open_scan --baseline b16_pre
```

| Bench | Before | After | Signal |
|---|---:|---:|---|
| `persist_wal_open_scan/10000` | 7.9089 ms | 161.75 µs | Buffered sequential scan avoids per-entry seek and reuses the payload buffer; Criterion reported −97.533%. |
| `persist_wal_open_scan/50000` | 43.344 ms | 760.79 µs | Criterion reported −98.083%. |
| `persist_wal_open_scan/100000` | 87.278 ms | 1.5317 ms | Criterion reported −98.083%. |

#### `persist_wal_body_size_no_fsync` — entry-body packing (PERSIST-04)

Fixed total changes (100k), swept changes-per-entry packing — isolates the
per-byte serialize+write cost from the per-entry overhead the count sweeps
cover. Per-entry overhead dominates at small bodies; the minimum is ~10k
changes/entry, after which large-`Vec` build/alloc creeps back in. This was the
PERSIST-04 measurement surface; the stable manual `write_vectored` candidate was
measured-rejected on 2026-06-01 because it regressed the WAL append hot path, so
the contiguous `Vec` + `write_all` path remains the baseline.

| Bench | per-entry=100 | =1000 | =10000 | =50000 | Notes |
|---|---:|---:|---:|---:|---|
| `persist_wal_body_size_no_fsync` | 12.5 ms | 8.42 ms | 7.22 ms | 13.1 ms | Equal total work; U-shaped in packing; vectored write rejected. |

PR-local quick WAL record-buffer reuse A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench wal --filter persist_wal_append_single_no_fsync`;
`scripts/run-benches.sh --profile quick --bench wal --filter persist_wal_body_size_no_fsync`.

| Bench | Before | After | Signal |
|---|---:|---:|---|
| `persist_wal_append_single_no_fsync/1000` | 3.1835 ms | 1.8447 ms | Writer-owned record buffer keeps the contiguous `write_all` record shape while avoiding per-append `Vec` allocation; median is ~42% below the local pre-change baseline. |
| `persist_wal_body_size_no_fsync/100` | 2.9168 ms | 1.9143 ms | Same allocation reuse on the 100-change packing; the 4 MiB retention cap keeps ordinary hot buffers reusable without pinning pathological max-entry allocations. |
| `persist_wal_body_size_no_fsync/1000` | 2.4978 ms | 1.4411 ms | Same allocation reuse on the 1000-change packing; median is ~42% below the local pre-change baseline while preserving the contiguous `write_all` path. |

#### `persist_wal_payload_shape_*` — scalar / JSON / vector payloads

These rows keep the WAL format unchanged and isolate payload shape for the
future WAL/compression overhaul. Quick profile writes/replays 1k changes as ten
100-change entries with `SyncPolicy::OnFlushOnly`; setup is outside the replay
timed body. The JSON fixture models an agent-memory metadata document, and the
vector fixtures use 128-dim and 768-dim first-class `Value::Vector` payloads.

Command:

```bash
scripts/run-benches.sh --profile quick --bench wal --filter payload_shape
```

| Bench | scalar i64 | JSON metadata | vector128 | vector768 | Notes |
|---|---:|---:|---:|---:|---|
| `persist_wal_payload_shape_no_fsync` | 1.084 ms | 1.677 ms | 1.826 ms | 2.185 ms | Append path only; no fsync in timed body. |
| `persist_wal_payload_shape_replay` | 1.433 ms | 2.870 ms | 2.514 ms | 2.908 ms | Reader open + checksum + optional decompression + postcard decode. |

#### `persist_wal_payload_compression_sweep` — compression threshold pressure

Benchmark-only codec sweep for the future WAL rewrite. The fixture serializes
`ChangeSet` payloads with `postcard` outside the timed body, then the timed body
applies a candidate compression threshold, optionally runs zstd level 1, and
computes the xxh3 checksum over the bytes that would be framed in the WAL. This
does not write to disk and does not measure fsync.

Command:

```bash
scripts/run-benches.sh --profile quick --bench wal --filter persist_wal_payload_compression_sweep
```

Quick profile on 2026-06-06 before the default threshold was raised to 4096
bytes. `current128` was the production `COMPRESS_THRESHOLD` at measurement
time; `never` is checksum-only. `always` mostly confirms the cost of
compressing sub-threshold bodies, so the table keeps representative threshold
rows and calls out sub-threshold compression in the notes.

| Payload | batch | current128 | 512 | 4096 | never | Notes |
|---|---:|---:|---:|---:|---:|---|
| scalar i64 | 1 | 2.23 ns | 2.23 ns | 2.23 ns | 2.23 ns | `always` compressed the tiny body at 1.19 us. |
| scalar i64 | 10 | 1.19 us | 10.1 ns | 10.1 ns | 10.1 ns | 512 avoids compression that current128 takes. |
| scalar i64 | 100 | 5.99 us | 6.01 us | 6.00 us | 96.4 ns | Crosses all swept thresholds below `never`. |
| JSON metadata | 1 | 3.87 us | 6.63 ns | 6.63 ns | 6.62 ns | Single JSON record crosses current128 only. |
| JSON metadata | 10 | 4.91 us | 4.91 us | 51.0 ns | 51.0 ns | 4096 avoids compression for this batch. |
| JSON metadata | 100 | 15.1 us | 14.9 us | 14.8 us | 552 ns | Crosses all swept thresholds below `never`. |
| vector128 | 1 | 4.92 us | 4.91 us | 11.3 ns | 11.3 ns | 4096 avoids single-vector compression. |
| vector128 | 10 | 9.86 us | 9.87 us | 9.89 us | 113 ns | Crosses all swept thresholds below `never`. |
| vector128 | 100 | 21.4 us | 21.4 us | 21.5 us | 1.18 us | Checksum-only cost rises with body size. |
| vector768 | 1 | 7.48 us | 7.50 us | 62.3 ns | 62.3 ns | 4096 avoids single-vector compression. |
| vector768 | 10 | 13.8 us | 13.8 us | 13.8 us | 654 ns | Crosses all swept thresholds below `never`. |
| vector768 | 100 | 35.6 us | 35.7 us | 35.7 us | 6.59 us | Large vector batches dominate checksum too. |

Decision signal: the existing 128-byte threshold is aggressive for scalar,
single JSON, and single-vector entries. Raising the threshold would avoid
microsecond-scale zstd work on many small writes, but this benchmark is only a
codec threshold surface; an actual WAL format or policy change still needs
end-to-end append/replay and recovery evidence.

#### `persist_wal_payload_compression_policy_*` — real writer policy sweep

End-to-end companion to the codec threshold sweep. These rows use the real
`WalWriter::open_with_compression` path added after the codec-only baseline.
Append rows include postcard serialization, policy selection, optional zstd,
header framing, checksum, and file writes with `SyncPolicy::OnFlushOnly`.
Flush rows add one explicit `WalWriter::flush()` durability barrier at the end
of the append cycle and include deterministic WAL file sizes in the row IDs.
Replay rows build a WAL file with the selected policy in setup, then time
reader iteration, checksum, optional decompression, and postcard decode.

Command:

```bash
scripts/run-benches.sh --profile quick --bench wal --filter compression_policy
scripts/run-benches.sh --profile quick --bench wal --filter compression_policy_flush
```

Quick profile on 2026-06-06 before the default threshold was raised to 4096
bytes. `total=1000` changes. `batch` is changes per WAL entry. `current128`
was the production default at measurement time, `threshold4096` avoids
single-record JSON/vector compression, and `disabled` leaves every payload
uncompressed.

| Payload / batch | append current128 | append 4096 | append disabled | replay current128 | replay 4096 | replay disabled | Signal |
|---|---:|---:|---:|---:|---:|---:|---|
| scalar i64 / b10 | 1.70 ms | 1.33 ms | 1.51 ms | 1.36 ms | 1.21 ms | 1.17 ms | Raised threshold avoids compressing small scalar batches. |
| JSON metadata / b1 | 6.53 ms | 2.77 ms | 2.59 ms | 3.17 ms | 1.51 ms | 1.35 ms | Single JSON records are over-compressed at current128. |
| JSON metadata / b10 | 2.38 ms | 2.04 ms | 2.02 ms | 2.21 ms | 2.28 ms | 2.24 ms | Append improves; replay is noise-level. |
| vector128 / b1 | 7.36 ms | 2.74 ms | 2.61 ms | 2.48 ms | 1.03 ms | 993 us | Single 128-dim vectors are over-compressed at current128. |
| vector128 / b100 | 1.69 ms | 1.75 ms | 1.51 ms | 2.24 ms | 2.03 ms | 1.84 ms | Large vector batches need size/latency trade-off work. |
| vector768 / b1 | 10.54 ms | 3.85 ms | 3.77 ms | 4.65 ms | 1.64 ms | 1.55 ms | Single 768-dim vectors strongly favor no compression. |
| vector768 / b100 | 1.84 ms | 2.11 ms | 2.20 ms | 2.88 ms | 2.93 ms | 2.58 ms | Current compression can help large append bytes; replay still favors no decompression. |

Flush-inclusive companion rows, with bytes-on-disk from the row IDs:

| Payload / batch | file current128 | file 4096 | file disabled | flush current128 | flush 4096 | flush disabled | Signal |
|---|---:|---:|---:|---:|---:|---:|---|
| scalar i64 / b1 | 81,016 B | 81,016 B | 81,016 B | 7.01 ms | 7.61 ms | 7.27 ms | Tiny scalar rows do not cross either compression threshold. |
| scalar i64 / b100 | 4,386 B | 4,386 B | 48,706 B | 5.20 ms | 5.22 ms | 5.23 ms | Large scalar batches compress heavily without a flush penalty. |
| JSON metadata / b1 | 239,016 B | 293,016 B | 293,016 B | 11.02 ms | 7.99 ms | 7.97 ms | Current128 saves bytes but is slower for single JSON records. |
| JSON metadata / b100 | 17,056 B | 17,056 B | 264,406 B | 6.03 ms | 6.47 ms | 6.53 ms | Large JSON batches keep the compression size win. |
| vector128 / b1 | 439,016 B | 594,016 B | 594,016 B | 11.66 ms | 7.96 ms | 7.77 ms | Current128 saves bytes but is slower for single vectors. |
| vector128 / b100 | 61,126 B | 61,126 B | 561,346 B | 6.03 ms | 6.10 ms | 5.72 ms | Compression gives a ~9x size win; latency is fsync/noise level. |
| vector768 / b1 | 1,962,016 B | 3,154,016 B | 3,154,016 B | 14.98 ms | 9.03 ms | 8.56 ms | Current128 saves bytes but strongly hurts single large vectors. |
| vector768 / b100 | 63,296 B | 63,296 B | 3,121,346 B | 6.41 ms | 6.94 ms | 7.36 ms | Compression gives a ~49x size win and remains competitive. |

PR-local WAL zstd compressor-reuse A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench wal --filter 'compression_policy_no_fsync/vector768/b1_threshold128|compression_policy_no_fsync/json_metadata/b1_threshold128|compression_policy_no_fsync/vector128/b1_threshold128'`;
`scripts/run-benches.sh --profile quick --bench wal --filter 'compression_policy_no_fsync/vector768/b100_default4096|compression_policy_no_fsync/json_metadata/b100_default4096|compression_policy_no_fsync/vector128/b100_default4096'`.

| Bench | Before | After | Signal |
|---|---:|---:|---|
| `persist_wal_payload_compression_policy_no_fsync/json_metadata/b1_threshold128` | 6.1906 ms | 5.1691 ms | Writer-owned zstd context avoids per-entry compressor setup for repeated compressed records; Criterion reported -17.542%, p=0.00. |
| `persist_wal_payload_compression_policy_no_fsync/vector128/b1_threshold128` | 7.2361 ms | 5.9544 ms | Same repeated-compression path; Criterion reported -17.593%, p=0.00. |
| `persist_wal_payload_compression_policy_no_fsync/vector768/b1_threshold128` | 11.197 ms | 9.8407 ms | Same repeated-compression path; Criterion reported -13.444%, p=0.00. |
| `persist_wal_payload_compression_policy_no_fsync/json_metadata/b100_default4096` | 845.34 us | 783.59 us | Production default threshold sanity row; no statistically significant change detected. |
| `persist_wal_payload_compression_policy_no_fsync/vector128/b100_default4096` | 592.36 us | 567.23 us | Production default threshold sanity row; no statistically significant change detected. |
| `persist_wal_payload_compression_policy_no_fsync/vector768/b100_default4096` | 1.0910 ms | 991.21 us | Production default threshold row; Criterion reported -8.6144%, p=0.02. |

Decision signal: the follow-up production policy raises the default threshold
from 128 bytes to 4096 bytes. Disabling compression entirely is still not a
clear global win because larger JSON/vector batches save substantial bytes with
similar durability-inclusive latency. Future reruns compare the old policy as
`threshold128` against the new default as `default4096`.

#### `persist_wal_sync_sweep` — sync-policy sweep

Append + explicit `flush()` across sync policies. The fsync-frequent policies
(`every1`/`every10`/`every100`) are bound by `fsync` syscall latency, not
selene-db code, and balloon to tens of seconds at 100k — they are **capped at
≤10k** so a full sweep is not dominated by one durability cell.

_The **10k column** was re-measured 2026-08-16 alongside the rest of §4a; the 1k
and 100k columns still carry the file-level 2026-06-01 stamp. #1132 measured
this sweep and published it in `docs/performance.md` without recording it here,
so the two documents disagreed by 5–10% on identical rows until this update._

| Bench | 1k | 10k | 100k | Notes |
|---|---:|---:|---:|---|
| `persist_wal_sync_sweep/every1` | 3.74 s | 35.97 s | n/a (capped) | `EveryN(1)` — fsync per entry. |
| `persist_wal_sync_sweep/every10` | 378 ms | 3.668 s | n/a (capped) | `EveryN(10)`. |
| `persist_wal_sync_sweep/every100` | 47.5 ms | 475.8 ms | n/a (capped) | `EveryN(100)`. |
| `persist_wal_sync_sweep/every1000` | 7.79 ms | 60.61 ms | 655 ms | `EveryN(1000)`. |
| `persist_wal_sync_sweep/on_flush_only` | 7.60 ms | 17.18 ms | 113 ms | `OnFlushOnly` + caller flush. |

`EveryN(1)` to `OnFlushOnly` at 10k is a **~2,090× spread**, which is the single
widest policy-choice consequence in the engine. `docs/performance.md` carries
the same table with a changes/s column.

### §4b Snapshot

`persist_snapshot_*` measure the SLSN **container** (framing + per-section zstd +
body hash) over synthetic byte payloads. The uncompressed companion rows isolate
raw framing/body-hash cost with `SectionCompression::None`. `scale` drives section
bytes.

_All five rows in the table below were re-measured 2026-08-16 (same M5 box,
rustc 1.97.1) via `scripts/run-benches.sh --profile full --bench snapshot`, at
the profile's default sample size of 30. Deltas quoted in the Notes column are
point-estimate deltas against the prior medians, not Criterion `--baseline`
comparisons, so they carry no p-value. The PR-local A/B tables further down
retain their own original commands and sample sizes._

The rows were previously refreshed/added with
`scripts/run-benches.sh --profile full --bench snapshot --filter 'persist_snapshot_(write|read|uncompressed_write|uncompressed_read)'`,
and the read rows with
`scripts/run-benches.sh --profile full --sample-size 20 --measurement-time 2 --bench snapshot --filter 'persist_snapshot_(read|uncompressed_read)'`.

| Bench | 10k | 50k | 100k | Notes |
|---|---:|---:|---:|---|
| `persist_snapshot_write` | 371.6 µs | 523.3 µs | 640.5 µs | Five independently-compressed sections over highly-compressible synthetic bytes. 100k −10.7%. |
| `persist_snapshot_read` | 297.3 µs | 469.7 µs | 659.1 µs | Snapshot read-and-apply for compressed sections. Flat (100k +0.6%). |
| `persist_snapshot_uncompressed_write` | 723.6 µs | 2.11 ms | 3.55 ms | Five uncompressed sections; exposes raw envelope write, body hash, and payload I/O cost. 100k −3.2%. |
| `persist_snapshot_uncompressed_read` | 410.5 µs | 1.67 ms | 3.26 ms | Snapshot read-and-apply for uncompressed sections. 100k −4.4%. |
| `persist_full_recovery` | 2.41 ms | 9.17 ms | 16.31 ms | Snapshot reconcile + WAL replay. **100k −21.4%** — the only row here that moved materially, and the WAL half explains it: §4a measured `persist_wal_replay` at −29.3% over the same window. The snapshot container rows are synthetic-byte framing work and are correctly insensitive to WAL 3.0. |

PR-local snapshot compression scheduling A/B:

Commands:

- `scripts/run-benches.sh --profile quick --bench snapshot --filter persist_snapshot_write --save-baseline snapshot-compression-scheduling-pre`
- `scripts/run-benches.sh --profile quick --bench snapshot --filter persist_snapshot_write --baseline snapshot-compression-scheduling-pre`
- `scripts/run-benches.sh --profile full --bench snapshot --filter persist_snapshot_write --sample-size 10 --measurement-time 1 --save-baseline snapshot-compression-scheduling-parallel-full`
- `scripts/run-benches.sh --profile full --bench snapshot --filter persist_snapshot_write --sample-size 10 --measurement-time 1 --baseline snapshot-compression-scheduling-parallel-full`

| Bench | Before | After | Change | Notes |
|---|---:|---:|---:|---|
| `persist_snapshot_write/1000` | 400.73 µs | 328.08 µs | -17.491%, p=0.00 | 64 KiB synthetic snapshot stays serial below the 1 MiB parallel-compression floor. |
| `persist_snapshot_write/10000` | 402.28 µs | 358.67 µs | -10.946%, p=0.00 | 640 KiB synthetic snapshot also avoids Rayon setup. |
| `persist_snapshot_write/50000` | 517.88 µs | 525.57 µs | no change, p=0.13 | 3.2 MiB synthetic snapshot keeps the existing parallel path. |
| `persist_snapshot_write/100000` | 653.13 µs | 658.25 µs | within Criterion noise threshold | 6.4 MiB synthetic snapshot keeps the existing parallel path. |

PR-local snapshot body-hash buffer A/B:

Commands:

- `scripts/run-benches.sh --profile full --sample-size 20 --measurement-time 2 --bench snapshot --filter 'persist_snapshot_(read|uncompressed_read)' --save-baseline snapshot-read-nozero-full-pre`
- `scripts/run-benches.sh --profile full --sample-size 20 --measurement-time 2 --bench snapshot --filter 'persist_snapshot_(read|uncompressed_read)' --baseline snapshot-read-nozero-full-pre`

| Bench | Before | After | Change | Notes |
|---|---:|---:|---:|---|
| `persist_snapshot_read/10000` | 291.80 µs | 295.27 µs | within Criterion noise threshold | Compressed sections are small after zstd, so the larger verification buffer does not materially move this row. |
| `persist_snapshot_read/50000` | 473.47 µs | 462.28 µs | within Criterion noise threshold | Same compressed-read guard row. |
| `persist_snapshot_read/100000` | 669.08 µs | 654.88 µs | within Criterion noise threshold | Same compressed-read guard row. |
| `persist_snapshot_uncompressed_read/10000` | 574.66 µs | 412.43 µs | -27.114%, p=0.00 | Body-hash verification now streams payloads with a 64 KiB buffer instead of 8 KiB, reducing read calls over raw sections. |
| `persist_snapshot_uncompressed_read/50000` | 2.4329 ms | 1.7219 ms | -28.537%, p=0.00 | Same large raw-section verification path. |
| `persist_snapshot_uncompressed_read/100000` | 4.7570 ms | 3.4118 ms | -29.538%, p=0.00 | Same large raw-section verification path. |

### §4c `graph_snapshot_roundtrip` — real rkyv graph encode/decode (D14)

Unlike the synthetic-bytes snapshot bench above, this drives the **real**
`CoreProvider` path over fixture rows: `IndexProvider::write_section` over every
`CORE/*` sub-tag (rkyv archive of `CORE/NODE`+`CORE/EDGE` positional rows, D14),
then a recovery-mode provider + `finish_recovery` (positional placement / id↔row
rebuild). Self-validating: asserts node/edge counts survive the roundtrip once
(untimed) before measuring. `scale` = fixture node count.

| Bench | 10k | 50k | 100k | Notes |
|---|---:|---:|---:|---|
| `graph_snapshot_roundtrip/encode` | 2.11 ms | 14.58 ms | 31.42 ms | rkyv encode of all `CORE/*` sections. Re-measured 2026-08-16; 100k −0.7%. |
| `graph_snapshot_roundtrip/decode` | 13.22 ms | 86.26 ms | 177.10 ms | Positional recovery + `finish_recovery`; duplicate-id validation is fused with row conversion. Re-measured 2026-08-16; 100k +1.9%. |
| `graph_snapshot_roundtrip/roundtrip` | 17.24 ms | 102.20 ms | 208.60 ms | End-to-end (≈ encode + decode). Re-measured 2026-08-16; 100k −5.1%. |

_Re-measured 2026-08-16 (same M5 box, rustc 1.97.1) via
`scripts/run-benches.sh --profile full --bench graph_snapshot_roundtrip`. **The
sample size changed:** the prior rows used `--sample-size 10
--measurement-time 1` and these use the `full` profile default of 30, so the
new rows are the better-conditioned estimate and the small deltas below should
not be read as movement. The rkyv graph encode/decode path took no format
change this cycle, and these rows agreeing to within a few percent is the
expected result._

The prior rows were refreshed with
`scripts/run-benches.sh --profile full --sample-size 10 --measurement-time 1 --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip`
(encode/roundtrip) and
`scripts/run-benches.sh --profile full --sample-size 10 --measurement-time 1 --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/decode`
(decode).

PR-local fused duplicate-id validation A/B:

Commands:

- `scripts/run-benches.sh --profile full --sample-size 10 --measurement-time 1 --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/decode --save-baseline d14-decode-fuse-full-pre`
- `scripts/run-benches.sh --profile full --sample-size 10 --measurement-time 1 --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/decode --baseline d14-decode-fuse-full-pre`

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_snapshot_roundtrip/decode/10000` | 15.31 ms | 14.55 ms | -4.3014%, p=0.00 | `CORE/NODE` and `CORE/EDGE` now validate non-tombstone id uniqueness while converting archive rows into runtime rows, avoiding a separate full traversal. |
| `graph_snapshot_roundtrip/decode/50000` | 88.63 ms | 86.57 ms | -2.3267%, p=0.00 | Same fused row-validation path. |
| `graph_snapshot_roundtrip/decode/100000` | 181.54 ms | 173.76 ms | -4.2833%, p=0.00 | Same fused row-validation path. |

PR-local snapshot row-position carrier A/B:

Command:
`scripts/run-benches.sh --profile quick --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/decode`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_snapshot_roundtrip/decode/1000` | 1.1709 ms | 1.0824 ms | -7.7946% | Recovery now carries the decoded snapshot row position beside each recovered node/edge row instead of maintaining separate `id -> position` BTreeMaps and looking them up during materialization. Criterion reports p=0.00. |

PR-local recovery row scratch-map A/B:

Command:
`scripts/run-benches.sh --profile quick --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/decode`.

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_snapshot_roundtrip/decode/1000` | 1.0696 ms | 956.20 µs | -10.732% | Recovery stores decoded snapshot rows in hash maps and carries separate positional order vectors, avoiding per-row `BTreeMap` inserts while preserving compacted-snapshot row placement and WAL-created dense append order. Criterion reports p=0.00. |

PR-local recovery bulk-liveness A/B:

Commands:

- `scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 3 --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/decode --save-baseline recovery-alive-bulk-pre`
- `scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 3 --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/decode --baseline recovery-alive-bulk-pre`

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_snapshot_roundtrip/decode/1000` | 982.93 µs | 952.37 µs | -3.1446% | Recovery now builds node/edge liveness bitmaps locally and installs each `Arc<RoaringBitmap>` once after materialization instead of calling `Arc::make_mut` per recovered row. Criterion reports p=0.00. |

PR-local direct archive-row encode A/B:

Commands:

- `scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 3 --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/encode --save-baseline snapshot-encode-direct-pre`
- `scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 3 --bench graph_snapshot_roundtrip --filter graph_snapshot_roundtrip/encode --baseline snapshot-encode-direct-pre`

| Bench | Before | After | Delta | Notes |
|---|---:|---:|---:|---|
| `graph_snapshot_roundtrip/encode/1000` | 336.17 µs | 270.58 µs | -19.184% | CORE/NODE and CORE/EDGE archive rows now encode borrowed row `PropertyMap`s directly instead of cloning them into temporary runtime rows before postcard serialization. Criterion reports p=0.00. |

## §5 selene-gql — parse / plan / execute

Bench bins: `parse`, `analyze`, `plan_optimize`, `expression_eval`, `mixed_orientation`,
`procedure_call_repeat`, `correlated_subquery`, `read_pipeline`, `write_e2e`.
The first four are scale-independent (single-query CPU).

### F01-PR04 one-hop mixed orientation runtime

`mixed_orientation` measures preplanned `execute_pattern` including the root
label scan, orientation filtering, bindings, and result allocation/drop. Parsing,
planning, fixture construction, and correctness guards are outside timing.
Degrees 8 and 1024 use respectively 9 and 1025 nodes. The directed and mixed
controls share node/edge identities and topology; half of the mixed edges are
intrinsically undirected. All rows produce exactly `degree` distinct edge IDs,
checked against the independent contiguous fixture ID sequence before timing.
There are no index-performance or pre/post speedup claims from this topology
comparison. The separate `mixed_edge_storage` target measures storage incidence,
not this GQL runtime operation.

Command: `scripts/run-benches.sh --profile full --bench mixed_orientation --sample-size 30 --measurement-time 5`.
Measured 2026-09-09 on Apple M5 (10 cores, 16 GiB), macOS 27.0 (26A5425a),
rustc 1.97.1 / LLVM 22.1.6, native aarch64. Full measurements used 30 samples,
3-second warmup, 5-second measurement, mimalloc, and the workspace bench profile
(opt-level 3, thin LTO, one codegen unit). Compilation completed before the
serialized run; no competing Cargo/benchmark run was active.

| Runtime row | Degree | Criterion time estimate | 95% interval |
|---|---:|---:|---:|
| `directed_right` | 8 | 909.18 ns | 906.45–913.36 ns |
| `directed_any` | 8 | 920.00 ns | 912.69–926.98 ns |
| `mixed_any` | 8 | 940.58 ns | 935.89–946.30 ns |
| `directed_right` | 1024 | 109.39 µs | 109.35–109.42 µs |
| `directed_any` | 1024 | 109.77 µs | 109.53–110.16 µs |
| `mixed_any` | 1024 | 110.16 µs | 110.04–110.32 µs |

The mixed/Any point estimate is 2.24% above directed/Any at degree 8 and 0.36%
above at degree 1024 (the high-degree intervals overlap). These are topology
controls, **not** before/after speedups. The new benchmark did not exist at the
supplied baseline; no baseline checkout/build was made and no historical
performance claim is inferred. This run does not measure indexed expansion.

| Bench | Median | Notes |
|---|---:|---|
| `gql_parse_corpus/m5c` | 879.6 µs | Full single-query parse-corpus latency. |
| `gql_parse_hostile/bracket_artifacts` | 566 ns | DoS-hardening: pathological `[`-backtracking input. |
| `gql_parse_hostile/recursion_chains` | 12.6 µs | DoS-hardening: deep sign/NOT/CASE recursion-guard input. |
| `gql_analyze_corpus/m5c` | 21.98 µs | Semantic analysis. |
| `gql_plan_optimize_corpus/m5c` | 48.13 µs | Planner/optimizer end-to-end. |
| `gql_plan_ir_clone/representative` | 164.0 ns | IR-clone hot path. |
| `gql_expression_eval/*` (17 cases) | 180–245 ns, plus JSON rows below | Scalar eval: predicates, scalar fns, CASE, list trimming, binary ops, and runtime-parameter JSON scalar functions. |
| `procedure_call_repeat/no_cache` | 2.958 ms | 100 short-lived sessions, parse/analyze/plan each. |
| `procedure_call_repeat/shared_cache` | 27.49 µs | Shared `Arc<CallPlanCache>` warm-hit — **99.1% lower**. |
| `procedure_call_pipeline/match_call_repeat/1000` | 254.62 µs (quick) | Warm plan-cache `MATCH` over 1k input nodes feeding regular `CALL bench.repeat()`; covers direct procedure-call row growth beyond one-row source calls. |
| `gql_profile_conformance/generated_capability_lookup` | 2.484 µs (quick) | One typed lookup for each of the 208 generated capability records. |
| `gql_profile_flagger/parse_admitted_and_rejected` | 23.02 µs (quick) | Parses one admitted direct-selected parameter query and one rejected `CREATE GRAPH ... LIKE` query (referenced-only feature GG04). |
| `procedure_feature_status/shared_cache_generated_capabilities` | 54.58 µs (quick) | Warm shared-cache `selene.feature_status()` call returning the generated inventory. |

PR-local generated-profile smoke commands:

- `scripts/run-benches.sh --profile quick --bench parse --filter gql_profile`
- `scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter procedure_feature_status`

The quick parse run measured `gql_parse_corpus/m5c` at 1.134 ms
(1.126–1.147 ms), compared with the supplied base row's 1.763 ms median
(1.716–1.844 ms). The generated lookup processed all 208 records in 2.484 µs
(2.479–2.490 µs). The feature-status command measured the ten-column, 208-row
procedure at 54.58 µs (54.40–54.82 µs).

PR-local quick procedure-call row-extension A/B:

Command:
`scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter 'procedure_call_repeat/(no_cache|shared_cache)'`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `procedure_call_repeat/no_cache` | 3.1136 ms | 2.4171 ms | CALL yield row extension now stays in `Binding` inline storage; median -21.8%. |
| `procedure_call_repeat/shared_cache` | 26.384 µs | 26.360 µs | Warm shared CALL-plan cache guard stayed neutral (`p = 0.74`). |

PR-local quick JSON expression baseline. These rows bind `Value::Json` payloads
as runtime parameters, so the timed body measures expression execution and
JSON scalar work, not JSON text parsing during `json(...)`.

| Bench | Median | Notes |
|---|---:|---|
| `gql_expression_eval/json/parse_type` | 151.83 ns (quick) | `json_type($payload)` over a prebound agent-memory metadata document. |
| `gql_expression_eval/json/nested_get_path_text` | 215.31 ns (quick) | Nested object/array path selector returning an episodic fact title. |
| `gql_expression_eval/json/nested_get_path_scalar` | 180.68 ns (quick) | Nested object path selector returning a JSON numeric leaf as a native GQL scalar. |
| `gql_expression_eval/json/has_path_miss` | 210.55 ns (quick) | Same nested selector shape returning a deterministic missing-path boolean. |
| `gql_expression_eval/json/contains_nested` | 189.99 ns (quick) | Recursive containment against a prebound candidate object/array subset. |
| `gql_expression_eval/json/construct_metadata` | 596.09 ns (quick) | Builds a nested JSON metadata document from scalar runtime parameters. |
| `gql_expression_eval/json/merge_patch_metadata` | 351.87 ns (quick) | Applies an RFC 7396 merge patch to a prebound metadata document. |
| `gql_expression_eval/json/patch_metadata` | 472.89 ns (quick) | Applies a three-operation RFC 6902 JSON Patch to a prebound metadata document. |

PR-local quick Reciprocal Rank Fusion procedure baseline:

| Bench | Median | Notes |
|---|---:|---|
| `procedure_reciprocal_rank_fusion/shared_cache_rankings2x64_k10` | 5.066 µs (quick) | Cached `CALL selene.reciprocal_rank_fusion` over two ranked node lists of width 64 with 50% neighboring-list overlap. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings2x256_k10` | 18.87 µs (quick) | Same RRF procedure row over two ranked node lists of width 256. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings2x1024_k10` | 87.81 µs (quick) | Same RRF procedure row over two ranked node lists of width 1,024. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings4x64_k10` | 9.192 µs (quick) | Cached RRF over four ranked node lists of width 64. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings4x256_k10` | 38.26 µs (quick) | Cached RRF over four ranked node lists of width 256. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings4x1024_k10` | 201.1 µs (quick) | Cached RRF over four ranked node lists of width 1,024. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings8x64_k10` | 17.99 µs (quick) | Cached RRF over eight ranked node lists of width 64. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings8x256_k10` | 81.46 µs (quick) | Cached RRF over eight ranked node lists of width 256. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings8x1024_k10` | 473.3 µs (quick) | Cached RRF over eight ranked node lists of width 1,024. |

PR-local quick RRF accumulation/top-k A/B:

Command:
`scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter procedure_reciprocal_rank_fusion --save-baseline rrf_btree`,
then the same command with `--baseline rrf_btree` after replacing tree-backed
RRF score/dedup accumulation with engine-id `FxHashMap` / `FxHashSet` and
partial top-k selection before the final deterministic output sort.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `procedure_reciprocal_rank_fusion/shared_cache_rankings2x64_k10` | 5.0839 µs | 3.2698 µs | −35.69%, p=0.00. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings2x256_k10` | 18.750 µs | 10.049 µs | −46.52%, p=0.00. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings2x1024_k10` | 83.477 µs | 36.573 µs | −56.23%, p=0.00. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings4x64_k10` | 10.159 µs | 5.5026 µs | −43.06%, p=0.00. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings4x256_k10` | 38.327 µs | 19.407 µs | −49.73%, p=0.00. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings4x1024_k10` | 207.16 µs | 70.854 µs | −66.40%, p=0.00. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings8x64_k10` | 17.994 µs | 10.396 µs | −42.49%, p=0.00. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings8x256_k10` | 81.288 µs | 38.434 µs | −52.88%, p=0.00. |
| `procedure_reciprocal_rank_fusion/shared_cache_rankings8x1024_k10` | 508.41 µs | 141.27 µs | −72.22%, p=0.00. |

PR-local quick vector procedure baseline:

| Bench | Median | Notes |
|---|---:|---|
| `procedure_vector_search/shared_cache_squared_euclidean_dim128_k10_1000` | 37.0 µs (quick) | Cached `CALL selene.vector_search_nodes` over 1,000 vector nodes; scalar exact scan. |
| `procedure_vector_search/shared_cache_flat_index_dim128_k10_1000` | 25.0 µs (quick) | Cached exact search over the flat vector index. |
| `procedure_vector_search/shared_cache_flat_index_repeated_8x_dim128_k10_1000` | 199.0 µs (quick) | Eight separate cached exact procedure calls, one short-lived session per query. |
| `procedure_vector_search/shared_cache_flat_index_batch_8x_dim128_k10_1000` | 176.3 µs (quick) | One cached `CALL selene.vector_search_nodes_batch` over eight query vectors; ~11% below repeated exact single-call latency. |
| `procedure_vector_search/shared_cache_score_nodes_64_dim128_k10_1000` | 5.15 µs (quick) | Cached `CALL selene.vector_score_nodes` over a 64-node candidate set; graph-derived candidate rerank baseline. |
| `procedure_vector_search/shared_cache_score_nodes_repeated_8x64_dim128_k10_1000` | 45.4 µs (quick) | Eight separate cached candidate-score procedure calls, one short-lived session per query. |
| `procedure_vector_search/shared_cache_score_nodes_batch_8x64_dim128_k10_1000` | 43.0 µs (quick) | One cached `CALL selene.vector_score_nodes_batch` over eight query vectors and eight 64-node candidate sets; ~5% below repeated single-call latency. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_64_dim128_k10_1000` | 3.97 µs (quick) | Cached `CALL selene.vector_score_candidate_state` over one maintained 64-node candidate set; avoids caller-side node-list construction and validates the generation-checked provider path. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_repeated_8x64_dim128_k10_1000` | 32.75 µs (quick) | Eight separate cached maintained candidate-state score calls, one short-lived session per query; faster than explicit-node repeated scoring. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_nodes_intersection_64_dim128_k10_1000` | 4.86 µs (quick) | Cached `CALL selene.vector_score_candidate_state_nodes` intersecting a maintained 64-node state with a 64-node explicit candidate list before exact rerank. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_nodes_intersection_repeated_8x64_dim128_k10_1000` | 39.65 µs (quick) | Eight separate cached maintained-state + explicit-node intersection calls, one short-lived session per query. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_nodes_union_128_dim128_k10_1000` | 8.20 µs (quick) | Cached maintained-state + explicit-node union producing 128 canonical candidates before exact rerank. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_nodes_state_difference_32_dim128_k10_1000` | 3.04 µs (quick) | Cached maintained-state minus explicit-node candidates, leaving 32 candidates before exact rerank. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_intersection_64_dim128_k10_1000` | 4.88 µs (quick) | Cached `CALL selene.vector_score_candidate_state_expanded` expanding two graph roots through `SUPPORTS`, intersecting with maintained state, and exact-reranking 64 canonical candidates. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_intersection_repeated_8x64_dim128_k10_1000` | 39.69 µs (quick) | Eight separate cached maintained-state + graph-expanded intersection calls, one short-lived session per query. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_intersection_batch_8x64_dim128_k10_1000` | 21.6 µs (quick) | One cached `CALL selene.vector_score_candidate_state_expanded_batch` over eight query/root-set pairs; repeated root sets expand and compose once, then the repeated candidate set is scored with one property-lookup pass. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_union_128_dim128_k10_1000` | 8.25 µs (quick) | Cached maintained-state + graph-expanded union producing 128 canonical candidates before exact rerank. |
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_state_difference_32_dim128_k10_1000` | 3.13 µs (quick) | Cached maintained-state minus graph-expanded candidates, leaving 32 candidates before exact rerank. |
| `procedure_vector_neighbors/shared_cache_score_neighbors_64_dim128_k10_1000` | 4.60 µs (quick) | Cached `CALL selene.vector_score_neighbors` over one 64-neighbor graph-derived candidate set. |
| `procedure_vector_neighbors/shared_cache_score_neighbors_repeated_8x64_dim128_k10_1000` | 40.3 µs (quick) | Eight separate cached graph-neighbor score calls, one short-lived session per query. |
| `procedure_vector_neighbors/shared_cache_score_neighbors_batch_8x64_dim128_k10_1000` | 25.3 µs (quick) | One cached `CALL selene.vector_score_neighbors_batch` over eight distinct anchors; the current guard row remains below repeated neighbor-call latency and stayed noise-scale for this PR. |
| `procedure_vector_expanded/shared_cache_score_expanded_2root64_dim128_k10_1000` | 4.85 µs (quick) | Cached `CALL selene.vector_score_expanded_candidates` where two root nodes expand through `SUPPORTS` to a 64-node canonical candidate set. |
| `procedure_vector_expanded/shared_cache_score_expanded_repeated_8x2root64_dim128_k10_1000` | 42.81 µs (quick) | Eight separate cached expanded-candidate score calls, one short-lived session per query. |
| `procedure_vector_expanded/shared_cache_score_expanded_batch_8x2root64_dim128_k10_1000` | 41.31 µs (quick) | One cached `CALL selene.vector_score_expanded_candidates_batch` over eight per-query root sets; ~3.5% below repeated expanded-call latency. |
| `procedure_vector_expanded/shared_cache_score_expanded_query_roots_2root64_dim128_k10_1000` | 95.94 µs (quick) | Full GQL pipeline row: `MATCH` + `WITH collect_list(root)` derives two graph roots, then calls `selene.vector_score_expanded_candidates`; root production dominates the explicit-root procedure boundary. |
| `procedure_vector_search/shared_cache_hnsw_ann_dim128_k10_1000` | 13.46 µs (quick) | Cached single-query `CALL selene.vector_search_nodes_ann` over the HNSW index; graph-level ANN hit conversion no longer re-heaps index results. |
| `procedure_vector_search/shared_cache_hnsw_ann_repeated_8x_dim128_k10_1000` | 114.4 µs (quick) | Eight separate cached ANN procedure calls, one short-lived session per query. |
| `procedure_vector_search/shared_cache_hnsw_ann_batch_8x_dim128_k10_1000` | 108.9 µs (quick) | One cached `CALL selene.vector_search_nodes_ann_batch` over eight query vectors; ~4.5% below repeated single-call latency. |
| `procedure_vector_ann_expanded/shared_cache_ann_expanded_2root64_dim128_k10_1000` | 14.83 µs (quick) | Cached `CALL selene.vector_search_expanded_candidates_ann`; HNSW supplies two `VectorSummary` roots, graph expansion walks `SUPPORTS`, and exact rerank returns final `VectorFact` candidates. |
| `procedure_vector_ann_expanded/shared_cache_ann_expanded_repeated_8x2root64_dim128_k10_1000` | 123.41 µs (quick) | Eight separate cached ANN-root graph-expansion calls, one short-lived session per query. |
| `procedure_vector_ann_expanded/shared_cache_ann_expanded_batch_8x2root64_dim128_k10_1000` | 119.18 µs (quick) | One cached `CALL selene.vector_search_expanded_candidates_ann_batch` over eight query vectors; ~3.4% below repeated ANN-expanded-call latency. |
| `procedure_vector_ann_expanded/shared_cache_ann_state_expanded_intersection_2root64_dim128_k10_1000` | 15.20 µs (quick) | Cached `CALL selene.vector_search_candidate_state_expanded_ann` using HNSW roots, graph expansion, maintained-state intersection, and exact rerank. |
| `procedure_vector_ann_expanded/shared_cache_ann_state_expanded_intersection_repeated_8x2root64_dim128_k10_1000` | 127.98 µs (quick) | Eight separate cached ANN-root graph-expansion calls intersected with maintained state before exact rerank. |

PR-local quick maintained-state expanded batch reuse A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 5 --bench procedure_call_repeat --filter procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_intersection_batch_8x64 --save-baseline state_expanded_batch_pre`
on the pre-change procedure path, then the same command with `--baseline
state_expanded_batch_pre` after repeated root sets expand and compose once.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_intersection_batch_8x64...` | 30.411 µs | 26.515 µs | The benchmark fixture sends the same two graph roots for all eight queries; the procedure now reuses one expansion/composition and keeps exact candidate-set batch scoring unchanged. |

PR-local quick repeated candidate-set scoring A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 40 --measurement-time 4 --bench procedure_call_repeat --filter procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_intersection_batch_8x64 --save-baseline repeated_candidate_scoring_pre`
on the pre-change scorer path, then the same command with `--baseline
repeated_candidate_scoring_pre` after repeated candidate sets score through one
candidate property-lookup pass. Graph-level guard:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench single_graph --filter graph_vector_candidate_set/score_candidate_sets_batch_cosine_q8_c64 --save-baseline repeated_candidate_graph_pre`,
then the same command with `--baseline repeated_candidate_graph_pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `procedure_vector_candidate_state/shared_cache_score_candidate_state_expanded_intersection_batch_8x64...` | 26.584 µs | 21.636 µs | The post-root-reuse procedure row now avoids eight repeated property lookup passes over the same 64-node maintained-state intersection before exact scoring. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q8_c64_d1024/64` | 102.04 µs | 97.528 µs | The core scorer keeps the existing query-level Rayon path for larger batches, but below threshold flips repeated candidate sets to candidate-major scoring. |

PR-local quick broad repeated candidate-set batch scoring A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench single_graph --filter graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64 --save-baseline repeated_candidate_major_pre`
on the pre-change batch scorer, then the same command with `--baseline
repeated_candidate_major_pre` after adding the broad repeated-set
candidate-major parallel path. q8 guard:
`scripts/run-benches.sh --profile quick --sample-size 30 --measurement-time 2 --bench single_graph --filter graph_vector_candidate_set/score_candidate_sets_batch_cosine_q8 --save-baseline repeated_candidate_major_q8_pre2`,
then focused 40-sample guard commands with `--baseline
repeated_candidate_major_q8_pre2`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q8_c64_d1024/64` | 97.500 µs | 97.487 µs | Below the broad repeated-set gate; unchanged. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q8_c256_d1024/256` | 383.60 µs | 381.97 µs | Below the broad repeated-set gate; unchanged. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q8_c1024_d1024/1024` | 423.33 µs | 428.48 µs | Remains on query-level Rayon; focused guard reports the change within noise threshold. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q8_c4096_d1024/4096` | 1.5236 ms | 1.1349 ms | Broad repeated candidate sets now split by candidate chunk, reuse one property lookup pass, and improve 23.65% (`p=0.00`). |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c64_d1024/64` | 182.50 µs | 177.08 µs | Small repeated sets keep the existing query-level Rayon path; Criterion reports the change within noise threshold. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c256_d1024/256` | 591.29 µs | 604.19 µs | Medium repeated sets stay on query-level Rayon; Criterion reports no change. A prototype gate at 256 candidates regressed this row by 31.28%, so the production gate is 4,096 candidates. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c1024_d1024/1024` | 2.2580 ms | 2.2701 ms | Medium-wide repeated sets stay on query-level Rayon; Criterion reports no change. The 256-candidate prototype gate regressed this row by 5.67%. |
| `graph_vector_candidate_set/score_candidate_sets_batch_cosine_q64_c4096_d1024/4096` | 9.4264 ms | 8.4606 ms | Broad repeated candidate sets now use candidate-major parallel scoring and improve 9.22% (`p=0.00`). |

PR-local quick mixed candidate-set reuse validation:

Commands:
`set -a; source .env; set +a; SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=google/gemini-embedding-2 SELENE_EMBEDDING_CORPUS=code_alias_memory SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter 'query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch'`
before and after the mixed repeated-candidate grouping path. Code-shaped
validation used:
`set -a; source .env; set +a; SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=mistralai/codestral-embed-2505 SELENE_EMBEDDING_CORPUS=code_alias_wide_memory SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter 'query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch'`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `procedure_vector_omlx_query_roots/shared_cache_query_root_expansion_batch/google_gemini-embedding-2_code_alias_memory_q8_k4_r2_c9_dim3072` | 287.57 µs | 279.60 µs | Mixed per-topic root sets now reuse duplicate canonical candidate groups inside the batch scorer; Criterion reported a 2.22% improvement (`p=0.00`). |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_intersection_batch/google_gemini-embedding-2_code_alias_memory_q8_k4_r2_c6_dim3072` | 280.12 µs | 270.47 µs | Same general-purpose 3072-dim endpoint; median moved down but Criterion classified the row as no statistically significant change (`p=0.14`). |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_provenance_state_intersection_batch/google_gemini-embedding-2_code_alias_memory_q8_k4_r2_c6_dim3072` | 282.38 µs | 269.49 µs | Provenance-gated current-state vector row improved 4.45% (`p=0.00`) while preserving the same candidate width. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_expansion_batch/mistralai_codestral-embed-2505_code_alias_wide_memory_q16_k4_r2_c11_dim1536` | n/a | 340.26 µs | Live Codestral code-shaped validation on the wider 16-query corpus; post-change row remains in the documented latency band. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_intersection_batch/mistralai_codestral-embed-2505_code_alias_wide_memory_q16_k4_r2_c8_dim1536` | n/a | 337.70 µs | Code-oriented endpoint keeps current-state vector scoring close to plain expansion while applying the current-state gate. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_provenance_state_intersection_batch/mistralai_codestral-embed-2505_code_alias_wide_memory_q16_k4_r2_c8_dim1536` | n/a | 342.65 µs | Provenance-gated Codestral row stays within a few microseconds of the current-state vector row. |

PR-local quick mixed root-set expansion reuse validation:

Commands:
`set -a; source .env; set +a; SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=mistralai/codestral-embed-2505 SELENE_EMBEDDING_CORPUS=project_source_chunk_memory SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --sample-size 40 --measurement-time 4 --bench procedure_call_repeat --filter 'query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch' --save-baseline expanded_group_pre`,
then the same command with `--baseline expanded_group_pre` after grouped root
expansion moved into the shared graph batch helper and the maintained-state GQL
batch procedure.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `procedure_vector_omlx_query_roots/shared_cache_query_root_expansion_batch/mistralai_codestral-embed-2505_project_source_chunk_memory_q16_k4_r2_c8_dim1536` | 341.15 µs | 305.78 µs | Repeated per-topic root sets now expand once per distinct group before exact vector batch scoring; Criterion reported a 9.03% improvement (`p=0.00`). |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_intersection_batch/mistralai_codestral-embed-2505_project_source_chunk_memory_q16_k4_r2_c6_dim1536` | 343.26 µs | 302.17 µs | Maintained current-state composition now uses the same grouped expansion helper, improving the live source-chunk row by 11.12% (`p=0.00`). |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_provenance_state_intersection_batch/mistralai_codestral-embed-2505_project_source_chunk_memory_q16_k4_r2_c6_dim1536` | 338.29 µs | 302.80 µs | Provenance-gated current-state vector scoring preserves the same candidate semantics while avoiding duplicate graph expansion, improving 10.32% (`p=0.00`). |

PR-local OpenRouter JSON-current composition row validation:

Command:
`set -a; source .env; set +a; SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=mistralai/codestral-embed-2505 SELENE_EMBEDDING_CORPUS=project_source_chunk_memory SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench procedure_call_repeat --filter 'query_root_json_current|query_root_current_state_text_score_batch|query_root_current_state_intersection_batch'`.
These rows use the existing candidate-scoped JSON procedures over document
`metadata` to filter root-expanded candidates to current support facts before
batch text/vector scoring. They are new absolute rows, so there is no
before/after p-value; the comparison is against maintained-state rows from the
same command.

| Bench | Median | Notes |
|---|---:|---|
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_intersection_batch/mistralai_codestral-embed-2505_project_source_chunk_memory_q16_k4_r2_c6_dim1536_basecurbp9687_curbp10000_hitbp10000` | 318.39 µs | Maintained current-state vector baseline for the JSON-current comparison command. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_json_current_vector_batch/mistralai_codestral-embed-2505_project_source_chunk_memory_q16_k4_r2_c6_dim1536_precbp10000_curbp10000_hitbp10000` | 1.1045 ms | JSON metadata filtering preserves full target/current quality, but the exact JSON procedure pass makes it ~3.5x slower than maintained current-state vector scoring on this source-chunk row. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_text_score_batch/mistralai_codestral-embed-2505_project_source_chunk_memory_q16_k4_r2_c6_dim1536_precbp9375_curbp9375_hitbp10000` | 185.29 µs | Maintained current-state BM25 baseline for the JSON-current text comparison. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_json_current_text_score_batch/mistralai_codestral-embed-2505_project_source_chunk_memory_q16_k4_r2_c6_dim1536_precbp9375_curbp9375_hitbp10000` | 364.84 µs | JSON-current filtering composes correctly with BM25 and keeps the same target-hit suffix, but is ~2.0x slower than maintained-state BM25 for this already-materialized current/support shape. Use candidate-scoped JSON as an exact metadata filter/oracle unless rows show a quality gap maintained state cannot express. |

PR-local OpenRouter maintained-state BM25 root-expansion reuse validation:

Command:
`set -a; source .env; set +a; SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=mistralai/codestral-embed-2505 SELENE_EMBEDDING_CORPUS=project_source_chunk_memory SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --sample-size 40 --measurement-time 4 --bench procedure_call_repeat --filter query_root_current_state_text_score_batch --save-baseline text_state_expand_pre`,
then the same command with `--baseline text_state_expand_pre` after
`selene.text_score_candidate_state_expanded_batch` reused the same grouped
root-expansion primitive as the vector companion.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_text_score_batch/mistralai_codestral-embed-2505_project_source_chunk_memory_q16_k4_r2_c6_dim1536_precbp9375_curbp9375_hitbp10000` | 189.96 µs | 170.90 µs | Repeated per-topic root sets expand once per distinct group before candidate-state composition and BM25 scoring. Criterion reports a 10.19% improvement (`p=0.00`, 95% mean CI -10.47% to -9.91%) while preserving the same precision/currentness/target-hit suffix. |

PR-local OpenRouter maintained-state text/vector RRF composition validation:

Command:
`set -a; source .env; set +a; SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=mistralai/codestral-embed-2505 SELENE_EMBEDDING_CORPUS=project_code_alias_memory SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench procedure_call_repeat --filter 'query_root_current_state_text_score_batch|query_root_current_state_intersection_batch|query_root_current_state_text_vector_rrf_batch'`,
then the same command with
`SELENE_EMBEDDING_CORPUS=project_source_chunk_memory`.
No before/after implementation p-value is claimed for this section; the useful
signal is the same-run latency/quality comparison between existing producers
and the new benchmark-only RRF composition row.

| Corpus / Bench | Median | Notes |
|---|---:|---|
| `project_code_alias_memory` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c8...precbp9218_curbp9218_hitbp9375` | 182.37 µs | Maintained BM25/current-state remains the fastest alias-heavy source row but misses one expected target. One high mild outlier. |
| `project_code_alias_memory` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c8...basecurbp9687_curbp10000_hitbp10000` | 327.32 µs | Maintained current-state vector scoring restores all targets and current precision. |
| `project_code_alias_memory` `shared_cache_query_root_current_state_text_vector_rrf_batch/...q16_k4_r2_c8...precbp10000_curbp10000_hitbp10000` | 534.14 µs | RRF over the maintained BM25/current-state and vector/current-state rankings also reaches full quality, but it is slower than the vector quality path and roughly costs the two producers plus small fusion overhead. |
| `project_source_chunk_memory` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c6...precbp9375_curbp9375_hitbp10000` | 167.40 µs | Target-complete BM25/current-state guard; one broad/current miss remains. Two outliers, one low mild and one high mild. |
| `project_source_chunk_memory` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c6...basecurbp9687_curbp10000_hitbp10000` | 309.41 µs | Maintained current-state vector scoring restores full current precision. |
| `project_source_chunk_memory` `shared_cache_query_root_current_state_text_vector_rrf_batch/...q16_k4_r2_c6...precbp10000_curbp10000_hitbp10000` | 487.45 µs | RRF again reaches full quality but is slower than using the best single maintained-state primitive for the needed quality target. No default fused policy is justified by these rows; keep RRF as a compositional A/B tool. Two outliers, including one high severe. |

### §5a `gql_correlated_subquery` — correlated EXISTS / aggregate VALUE execution (GQLRT-05/B3)

The only read-query **execution** bench in the suite (`expression_eval` is
scalar-only; `write_e2e` is write-only). A correlated subquery is re-evaluated
per outer row and its pattern schema is rebuilt per row (`schema_for_pattern`); a
memoization win (GQLRT-05) would otherwise be invisible. The `count` arm uses a
correlated `VALUE { ... RETURN count(*) }` aggregate because `COUNT { MATCH ... }`
is intentionally rejected by the current parser. In-memory graph (no WAL) so the
timed body is read execution, not durability. Uses a **small scale envelope**
(2.5k/5k/10k fixture rows, ~scale/3 `Person` rows) — correlated
re-evaluation is O(rows × subquery), so the cost grows super-linearly when the
inner scan cannot reuse the already-bound outer entity.

_Refreshed 2026-06-11 for B3 (development post-#705 vs the feature branch on this
M5, profile `full`, mimalloc), so these run ahead of the `3a864ac` header until
the next clean re-sweep. Seed-bound scans short-circuit the inner scan when the
scan binding is already a live `NodeRef`/`EdgeRef` in the outer row, while still
reapplying liveness, labels, value constraints, binding equality, and residual
predicates. Development baselines were: exists 58.951 ms / 237.45 ms / 932.70 ms
and count 59.387 ms / 235.91 ms / 933.92 ms at 2.5k / 5k / 10k. The B3 medians
below are ~90x / 190x / 339x faster for EXISTS and ~85x / 179x / 349x faster for
the aggregate count arm._

| Bench | 2.5k | 5k | 10k | Notes |
|---|---:|---:|---:|---|
| `gql_correlated_subquery/exists` | 654 µs | 1.251 ms | 2.752 ms | `FILTER EXISTS { (p)-[:KNOWS]->(:Person) }`; B3 seeded inner scan. |
| `gql_correlated_subquery/count` | 698 µs | 1.318 ms | 2.676 ms | `VALUE { MATCH (p)-[:KNOWS]->(:Person) RETURN count(*) }`; B3 seeded inner scan. |

PR-local aggregate value syntax repair:

Command:
`scripts/run-benches.sh --profile quick --bench correlated_subquery`.

| Bench | 1k quick | Notes |
|---|---:|---|
| `gql_correlated_subquery/exists/1000` | 190.27 µs | Existing correlated `EXISTS` guard still runs. |
| `gql_correlated_subquery/count/1000` | 284.74 µs | The aggregate count guard now uses supported `VALUE { MATCH ... RETURN count(*) }` syntax instead of rejected `COUNT { MATCH ... }` syntax. |

_Refreshed again 2026-06-11 for B5 (development post-#706 vs the feature
branch on this M5, profile `full`, mimalloc). Moving the four immutable maps
keyed by engine-assigned `NodeId`/`EdgeId` values to `FxBuildHasher`
(`node_id_to_row`, `edge_id_to_row`, `adjacency_out`, `adjacency_in`) trimmed the
post-B3 residual lookup cost without changing user-keyed label/property maps._

| Bench | Development post-B3 | B5 | Notes |
|---|---:|---:|---|
| `gql_correlated_subquery/exists/2500` | 654.05 µs | 572.31 µs | −12.3% median; seeded source + outgoing adjacency lookup. |
| `gql_correlated_subquery/count/2500` | 688.13 µs | 605.21 µs | −11.8% median. |
| `gql_correlated_subquery/exists/5000` | 1.2261 ms | 1.0636 ms | −13.1% median. |
| `gql_correlated_subquery/count/5000` | 1.3050 ms | 1.1513 ms | −12.7% median. |
| `gql_correlated_subquery/exists/10000` | 2.5415 ms | 2.1501 ms | −15.3% median. |
| `gql_correlated_subquery/count/10000` | 2.6529 ms | 2.2669 ms | −14.5% median. |

_Refreshed again 2026-06-11 for B18/B20 (development post-#707 vs the feature
branch, same M5, profile `full`, mimalloc). B18 resolves scan, expand/repeat,
join-key, subplan-projection, and evaluator variable names without per-row
`DbString` clones where the runtime operator can safely hoist the column
indexes. B20 makes per-group aggregate slots borrow the immutable plan
`Aggregate` descriptor instead of cloning it for every group._

| Bench | Development post-B5 | B18/B20 | Notes |
|---|---:|---:|---|
| `gql_correlated_subquery/exists/2500` | 574.04 µs | 545.43 µs | −4.8% median; remaining correlated runtime binding/index resolution. |
| `gql_correlated_subquery/count/2500` | 609.85 µs | 583.21 µs | −4.7% median. |
| `gql_correlated_subquery/exists/5000` | 1.0773 ms | 1.0052 ms | −5.3% median. |
| `gql_correlated_subquery/count/5000` | 1.1229 ms | 1.0571 ms | −5.9% median. |
| `gql_correlated_subquery/exists/10000` | 2.1321 ms | 2.0191 ms | −5.5% median. |
| `gql_correlated_subquery/count/10000` | 2.2679 ms | 2.1505 ms | −5.2% median. |

### §5b `write_e2e` — GQL write end-to-end

Two families. The **in-memory CPU** family runs on a no-WAL `SharedGraph` to
isolate parse/plan/execute + in-memory commit CPU. The **durable** family
(`*_with_flush`, `direct_*`) keeps a real WAL on `OnFlushOnly` /
`CommitBatching::Off`. The `match_*` / `insert_node_with_edge` arms scan the
fixture and so scale with N; the single-node arms are flat.

| Bench | 10k | 50k | 100k | Notes |
|---|---:|---:|---:|---|
| `write_e2e/gql_insert_single_node_per_iter_plan` | 317 µs | 237 µs | 394 µs | Parse/plan/execute per iter (in-memory). |
| `write_e2e/gql_insert_single_node_preplanned` | 279 µs | 190 µs | 342 µs | Preplanned single-node insert. |
| `write_e2e/gql_insert_single_node_cached` | 129 µs | 115 µs | 151 µs | Plan-cache warm hit. |
| `write_e2e/gql_insert_single_node_cached_with_schema_churn` | 152 µs | 165 µs | 289 µs | Cache hit under schema churn. |
| `write_e2e/gql_insert_node_with_edge_preplanned` | 1.67 ms | 11.75 ms | 24.86 ms | Preplanned insert + matched source + edge (scans). |
| `write_e2e/gql_match_set_preplanned` | 1.76 ms | 11.74 ms | 24.12 ms | Indexed match + property update (scans). |
| `write_e2e/gql_match_delete_preplanned` | 1.68 ms | 12.25 ms | 24.97 ms | Fresh fixture per iter (target deleted). |
| `write_e2e/gql_cached_point_read_set_r60w40` | 8.717 ms | 6.781 ms | 9.318 ms | One warm-plan-cache in-memory cycle: 60 indexed `bench_id` point reads plus 40 indexed `SET score = $score` writes over two parameterized source strings. |
| `write_e2e/gql_multi_statement_txn_preplanned` | 280 µs | 191 µs | 350 µs | START, three INSERTs, COMMIT. |
| `write_e2e/explicit_txn_3_inserts_rust_api` | 275 µs | 223 µs | 363 µs | Three inserts via the Rust txn API. |
| `write_e2e/explicit_txn_3_inserts_rollback` | 279 µs | 198 µs | 355 µs | Same, rolled back. |
| `write_e2e/gql_insert_single_node_preplanned_with_flush` | 4.22 ms | 4.27 ms | 3.95 ms | Durable: preplanned insert + WAL flush. |
| `write_e2e/direct_insert_single_node_with_wal_flush` | 4.20 ms | 4.30 ms | 4.17 ms | Direct mutation + one WAL flush. |
| `write_e2e/direct_insert_single_node_with_wal_flush_every10` | 30.5 ms | 32.2 ms | 32.4 ms | Ten direct inserts over one flush. |

PR-local B18/B20 write-control guard (`scripts/run-benches.sh --profile full
--bench write_e2e`, followed by an isolated rerun of the noisy direct-WAL 50k
row):

| Bench | Development post-B5 | B18/B20 | Notes |
|---|---:|---:|---|
| `write_e2e/gql_cached_point_read_set_r60w40/100000` | 5.0627 ms | 5.0207 ms | No statistically significant change; mixed indexed reads + writes guard. |
| `write_e2e/gql_cached_json_read_patch_r60w40/100000` | 5.3239 ms | 5.2953 ms | No statistically significant change; JSON read/patch guard. |
| `write_e2e/gql_match_set_preplanned/100000` | 11.020 ms | 10.500 ms | −4.7% median; scan/set row benefits from runtime binding-index hoisting. |
| `write_e2e/gql_match_delete_preplanned/100000` | 12.167 ms | 11.566 ms | −4.9% median. |
| `write_e2e/direct_insert_single_node_with_wal_flush/50000` | 4.2367 ms | 4.1545 ms | Isolated rerun after one transient 14.791 ms sample; no reproducible direct-WAL regression. |

PR-local quick JSON mixed row:

| Bench | Median | Notes |
|---|---:|---|
| `write_e2e/gql_cached_json_read_patch_r60w40/1000` | 4.486 ms (quick) | One warm-plan-cache in-memory cycle over property-backed JSON payloads: 60 indexed `bench_id` point reads extracting nested JSON metadata and 40 indexed point updates applying an idempotent three-op `json_patch` over `payload`. Payload seeding runs outside the timed body. |

PR-local quick B2 shared source-plan cache row
(`scripts/run-benches.sh --profile quick --bench write_e2e --filter gql_insert_single_node`):

| Bench | Median | Notes |
|---|---:|---|
| `write_e2e/gql_insert_single_node_per_iter_plan/1000` | 98.559 µs (quick) | Fresh parse/analyze/plan plus execute. |
| `write_e2e/gql_insert_single_node_cached/1000` | 53.470 µs (quick) | Session-local warm source-plan cache. |
| `write_e2e/gql_insert_single_node_shared_cache/1000` | 53.509 µs (quick) | Fresh `Session` per request reusing shared non-CALL source-plan cache. |
| `write_e2e/gql_insert_single_node_cached_with_schema_churn/1000` | 61.537 µs (quick) | Session-local cache under periodic schema-version invalidation. |

PR-local quick mutation row-extension guard
(`scripts/run-benches.sh --profile quick --bench write_e2e --filter 'gql_insert_single_node_cached|gql_insert_node_with_edge_preplanned|gql_multi_statement_txn_preplanned'`):

| Bench | Development baseline | Inline Binding row extension | Notes |
|---|---:|---:|---|
| `write_e2e/gql_insert_single_node_cached/1000` | 52.698 µs | 52.297 µs | No statistically significant change. |
| `write_e2e/gql_insert_single_node_cached_with_schema_churn/1000` | 61.294 µs | 61.207 µs | No statistically significant change. |
| `write_e2e/gql_insert_node_with_edge_preplanned/1000` | 164.49 µs | 161.12 µs | −3.8% median; p < 0.01. |
| `write_e2e/gql_multi_statement_txn_preplanned/1000` | 71.661 µs | 70.956 µs | −1.5% median; within noise threshold. |

### §5c `read_pipeline` — read-query pipeline execution

Read-execution coverage for the declared 60%-read workload: label scan +
indexed range filter, two-leg hash join, ORDER BY top-K, high-cardinality
GROUP BY, DISTINCT dedup, indexed `IN` bitmap union, inline `CALL {}`
table-subquery row extension, correlated `NEXT` row expansion, non-leading
`OPTIONAL MATCH` null-extension, post-RETURN bare `LIMIT 10`, pre-RETURN bare
`LIMIT 10`, and maintained composite-index equality lookup. Warm-plan-cache rows run over
`BenchFixture` on an in-memory `SharedGraph` (no WAL), so the timed body is
pure execution + index access — not parse/plan/optimize, not durability. Cold
and shared-cache companions on the cheapest row rebuild a fresh session per
iteration to isolate short-lived-session cache strategy. The join targets
`Person→Sensor→Device` deliberately: every fixture
`KNOWS` offset is ≡1 mod 3, so a `Person→Person` join would be empty.

_Measured 2026-06-11 on the B3 feature branch (profile `full`, mimalloc), so
these run ahead of the `3a864ac` header until the next clean re-sweep. The
same-session development guard stayed within noise for ordinary read rows; the
50k hashjoin sample printed wide variance (`p = 0.14`, no statistically
significant change), and the seeded-scan branch does not show a persistent
no-seed scan tax._

Historical baseline signals worth reading directly from the table: warm
`match_limit10` is **scale-linear** (784 µs → 13.39 ms for ten output rows) —
the scan does not short-circuit on LIMIT, which is the B19 baseline this row
exists to expose; and `match_limit10/cold` ≈ warm at these scales because the
~30–45 µs fixed compile cost (clearly visible at the 1k quick scale: 111 µs
cold vs 81 µs warm) amortizes under the linear scan.

| Bench | 10k | 50k | 100k | Notes |
|---|---:|---:|---:|---|
| `read_pipeline/match_filter_project` | 646 µs | 3.53 ms | 9.13 ms | Warm label scan + `n.age >= 40` filter + projection (age-indexed range path). |
| `read_pipeline/match_expand_hashjoin` | 14.90 ms | 105.62 ms | 219.10 ms | Two-leg `(:Person)-[:KNOWS]->(s:Sensor), (s)-[:KNOWS]->(d:Device)`; hash-join build/probe. |
| `read_pipeline/order_by_topk` | 1.40 ms | 7.82 ms | 16.36 ms | Full Person scan → `ORDER BY n.score DESC LIMIT 10` top-K (`score` non-indexed). |
| `read_pipeline/group_by_highcard` | 1.04 ms | 5.29 ms | 11.21 ms | `GROUP BY n.score` + `count(*)`, ~1024 groups; hash-aggregate build (B20 target). |
| `read_pipeline/distinct_dedup` | 877 µs | 5.93 ms | 13.61 ms | `RETURN DISTINCT n.name` over 256 distinct values; distinct hash-set. |
| `read_pipeline/match_limit10` | 784 µs | 5.93 ms | 13.39 ms | Warm bare `LIMIT 10` — scale-linear: no scan short-circuit (B19 baseline). |
| `read_pipeline/match_limit10/cold` | 815 µs | 5.95 ms | 13.54 ms | Same query, fresh uncached session per iter: full parse/analyze/plan/optimize/execute. |

PR-local scan direct-binding output A/B
(`scripts/run-benches.sh --profile quick --bench read_pipeline --filter match_filter_project --save-baseline gql_scan_direct_bindings_pre`;
rerun with `--baseline gql_scan_direct_bindings_pre` after the implementation):

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/match_filter_project/1000` | 50.738 µs | 46.297 µs | Scan execution now returns `Binding` rows directly instead of collecting transient `(entity, binding)` pairs that `scan_pattern` immediately discarded. The quick indexed-range row improves 8.8043% (`p=0.00`). |

PR-local scan output capacity reserve A/B
(`scripts/run-benches.sh --profile quick --bench read_pipeline --filter match_filter_project --save-baseline gql_scan_output_reserve_pre`;
rerun with `--baseline gql_scan_output_reserve_pre` after the implementation):

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/match_filter_project/1000` | 45.691 µs | 44.279 µs | Scan execution now reserves accepted `Binding` row capacity from the already-materialized candidate-row count. The quick indexed-range row improves 3.1557% (`p=0.00`). |

PR-local non-leading `OPTIONAL MATCH` coverage row
(`scripts/run-benches.sh --profile quick --bench read_pipeline --filter optional_match_null_extend`):

| Bench | Quick | Notes |
|---|---:|---|
| `read_pipeline/optional_match_null_extend/1000` | 150.39 µs | `MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(missing:Nope) RETURN missing`; preserves every Person row with a null binding. A trial output `Vec` preallocation measured 150.45 µs (`p = 0.16`), so no runtime change was kept. |

PR-local quick inline `CALL {}` row-extension guard
(`scripts/run-benches.sh --profile quick --bench read_pipeline --filter call_subquery`):

| Bench | Baseline | Inline Binding append | Notes |
|---|---:|---:|---|
| `read_pipeline/call_subquery_yield/1000` | 155.18 µs | 151.19 µs | −2.6% median; p < 0.05. |
| `read_pipeline/optional_call_subquery_null_yield/1000` | 241.50 µs | 240.79 µs | No statistically significant change. |

PR-local inline `CALL {}` seed-row A/B:

Commands: `scripts/run-benches.sh --profile quick --bench read_pipeline --filter call_subquery`;
then temporarily rerun the old seed-row path and rerun after the change with
`scripts/run-benches.sh --profile full --bench read_pipeline --filter call_subquery`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/call_subquery_yield/1000` | 149.93 µs | 142.39 µs | Table-subquery seed rows now build directly in `Binding` inline storage instead of allocating a `Vec<Null>` per outer row; Criterion reports -7.6108%, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/1000` | 246.78 µs | 229.23 µs | Optional null-yield row improves -3.6571%, p=0.01. |
| `read_pipeline/call_subquery_yield/10000` | 1.4961 ms | 1.4547 ms | Full-profile yielded row improves -2.4932%, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/10000` | 2.3365 ms | 2.2512 ms | Full-profile optional row improves -3.2234%, p=0.00. |
| `read_pipeline/call_subquery_yield/50000` | 8.3335 ms | 8.2273 ms | Larger yielded row trends lower but stays within Criterion's noise threshold. |
| `read_pipeline/optional_call_subquery_null_yield/50000` | 15.263 ms | 14.866 ms | Optional row improves -2.5990%, p=0.00. |
| `read_pipeline/call_subquery_yield/100000` | 17.326 ms | 17.233 ms | Largest yielded row stays neutral (p=0.30). |
| `read_pipeline/optional_call_subquery_null_yield/100000` | 35.482 ms | 34.021 ms | Largest optional row improves -4.1173%, p=0.00. |

PR-local inline `CALL {}` target-schema hoist A/B:

Commands: `scripts/run-benches.sh --profile quick --bench read_pipeline --filter call_subquery`;
then temporarily rerun the old per-row target-schema path and rerun after the
change with
`scripts/run-benches.sh --profile full --bench read_pipeline --filter call_subquery`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/call_subquery_yield/1000` | 142.68 µs | 139.56 µs | `CALL {}` now computes its target seed schema once per operator and clones it per seed table, avoiding per-outer-row pattern/outer-binding schema reconstruction; Criterion reports -2.2856%, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/1000` | 234.85 µs | 225.12 µs | Quick optional row improves -5.2429%, p=0.00. |
| `read_pipeline/call_subquery_yield/10000` | 1.4458 ms | 1.4158 ms | Full-profile yielded row improves -2.4242%, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/10000` | 2.2682 ms | 2.1683 ms | Full-profile optional row improves -4.3935%, p=0.00. |
| `read_pipeline/call_subquery_yield/50000` | 8.1792 ms | 7.9357 ms | Full-profile yielded row improves -2.9773%, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/50000` | 14.706 ms | 13.985 ms | Full-profile optional row improves -4.9020%, p=0.00. |
| `read_pipeline/call_subquery_yield/100000` | 16.978 ms | 16.483 ms | Full-profile yielded row improves -2.9145%, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/100000` | 34.631 ms | 33.133 ms | Full-profile optional row improves -4.3268%, p=0.00. |

PR-local inline `CALL {}` output-reserve A/B:

Commands: temporarily rerun the old empty output-vector path, then rerun after
the change with
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter call_subquery`;
repeat with
`scripts/run-benches.sh --profile full --bench read_pipeline --filter call_subquery`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/call_subquery_yield/1000` | 140.37 µs | 138.17 µs | `CALL {}` now reserves output row capacity from the outer input row count; old-path quick rerun reported +1.6926% slower than the patched sample, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/1000` | 223.04 µs | 218.22 µs | Quick optional row stayed within Criterion's noise threshold (p=0.09). |
| `read_pipeline/call_subquery_yield/10000` | 1.4203 ms | 1.4018 ms | Full-profile yielded row stayed within Criterion's noise threshold. |
| `read_pipeline/optional_call_subquery_null_yield/10000` | 2.1750 ms | 2.1620 ms | Full-profile optional row stayed within Criterion's noise threshold. |
| `read_pipeline/call_subquery_yield/50000` | 7.9749 ms | 7.8265 ms | Full-profile yielded row improves -1.8607%, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/50000` | 14.773 ms | 14.342 ms | Full-profile optional row improves -2.9148%, p=0.00. |
| `read_pipeline/call_subquery_yield/100000` | 16.947 ms | 16.639 ms | Full-profile yielded row improves -1.8188%, p=0.00. |
| `read_pipeline/optional_call_subquery_null_yield/100000` | 34.471 ms | 34.234 ms | Largest optional row stayed neutral (p=0.13). |

PR-local correlated `NEXT` output-reserve A/B:

Commands: add the `read_pipeline/correlated_next_expand` row, save the quick
baseline with
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter 'read_pipeline/(correlated_next_expand|call_subquery_yield|optional_match_null_extend|for_expand_triple)' --save-baseline gql-correlated-next-pre`,
then rerun with `--baseline gql-correlated-next-pre`; repeat the target row at
full scale with
`scripts/run-benches.sh --profile full --bench read_pipeline --filter correlated_next_expand`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/correlated_next_expand/1000` | 169.70 µs | 168.40 µs | `CorrelatedChain` now reserves output capacity from the outer input row count. Quick row trends lower but stays within Criterion's noise threshold. |
| `read_pipeline/correlated_next_expand/10000` | 1.8139 ms | 1.7496 ms | Full-profile 10k row improves -2.3967%, p=0.00. |
| `read_pipeline/correlated_next_expand/50000` | 10.647 ms | 10.471 ms | Full-profile 50k row trends lower but stays within Criterion's noise threshold. |
| `read_pipeline/correlated_next_expand/100000` | 22.237 ms | 22.346 ms | Full-profile 100k row stayed neutral. |

PR-local composite lookup guard:

Command:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter match_composite_lookup`.

| Bench | 1k quick | Notes |
|---|---:|---|
| `read_pipeline/match_composite_lookup/1000` | 461.85 ns | Warm cached `CompositeLookup` for `Person(age, name)` equality over a maintained composite property index. A follow-up inline scratch-storage experiment measured 465.29 ns and stayed within Criterion's noise threshold, so no runtime change was kept. |

PR-local short-lived-session source-plan cache A/B:

Command:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter match_limit10`.

| Bench | 1k quick | Notes |
|---|---:|---|
| `read_pipeline/match_limit10/1000` | 67.839 µs | Warm same-session cache-hit baseline. |
| `read_pipeline/match_limit10/cold/1000` | 132.41 µs | Fresh uncached `Session` per iter: parse/analyze/plan/optimize/execute. |
| `read_pipeline/match_limit10/shared_cache/1000` | 66.491 µs | Fresh `Session` per iter over a warmed caller-owned `SharedPlanCache`; cache hit bypasses parse/analyze/plan/optimize and measures session churn + cached execute. Median is −49.8% vs uncached fresh sessions and within noise of the same-session warm row. |

PR-local B19 pre-RETURN LIMIT cap A/B:

Command:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter limit10`.

| Bench | 1k quick | Notes |
|---|---:|---|
| `read_pipeline/match_limit10/1000` | 65.732 µs | Post-RETURN `MATCH (n:Person) RETURN n.name AS name LIMIT 10`; projection precedes LIMIT, so pattern materialization stays uncapped. |
| `read_pipeline/match_prereturn_limit10/1000` | 29.948 µs | Pre-RETURN `MATCH (n:Person) LIMIT 10 RETURN n.name AS name`; leading literal LIMIT caps accepted pattern rows before projection. Median is −54.4% vs the post-RETURN baseline. |
| `read_pipeline/match_limit10/cold/1000` | 111.59 µs | Fresh uncached session companion from the same run. |
| `read_pipeline/match_limit10/shared_cache/1000` | 62.931 µs | Fresh session over warmed shared source-plan cache companion from the same run. |

PR-local passive post-RETURN LIMIT cap A/B:

Command:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter match_limit10`.

| Bench | 1k quick | Notes |
|---|---:|---|
| `read_pipeline/match_limit10/1000` | 30.062 µs | Post-RETURN `MATCH (n:Person) RETURN n.name AS name LIMIT 10`; direct pattern-property projection plus literal LIMIT now caps accepted pattern rows before projection. Median is -54.3% vs the prior post-RETURN quick row (`65.732 µs`) and matches the pre-RETURN cap envelope. |
| `read_pipeline/match_limit10/cold/1000` | 74.914 µs | Fresh uncached session companion; parse/analyze/plan/optimize cost is still visible, but execution no longer scans the full 1k fixture. |
| `read_pipeline/match_limit10/shared_cache/1000` | 30.388 µs | Fresh session over warmed shared source-plan cache; session churn plus capped cached execute stays within noise of the same-session warm row. |

PR-local edge-index sprint A/B:

Command:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter edge_property_filter`.

| Bench | 1k quick | Notes |
|---|---:|---|
| `read_pipeline/edge_property_filter_no_index/1000` | 476.73 µs | Warm unanchored edge-property query over `CONNECTED_TO` edges with no edge-property index; scans expand adjacency and evaluates `e.from_port = 'port_17'` as a residual predicate. |
| `read_pipeline/edge_property_filter_indexed/1000` | 115.87 µs | Same query with a built-in `CONNECTED_TO(from_port)` edge-property index; optimizer emits edge `TypedIndexRange` and expand drives from the selective indexed edge-row set. Median is −75.7% vs no-index. |

PR-local edge-label borrow A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench read_pipeline --filter edge_property_filter --save-baseline gql-edge-label-borrow-before`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench read_pipeline --filter edge_property_filter --baseline gql-edge-label-borrow-before`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/edge_property_filter_no_index/1000` | 463.21 µs | 451.77 µs | Edge-label predicate matching now borrows the graph-owned `DbString` through recursive label-expression evaluation instead of cloning it at call sites and for compound predicates. Criterion reports -2.4228%, p=0.00. |
| `read_pipeline/edge_property_filter_indexed/1000` | 89.251 µs | 89.179 µs | Indexed edge-property guard stayed neutral (p=0.56). |

PR-local scan-key borrow A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench read_pipeline --filter 'read_pipeline/(match_filter_project|match_name_in|match_composite_lookup|edge_property_filter)' --save-baseline gql-scan-label-borrow-before`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench read_pipeline --filter 'read_pipeline/(match_filter_project|match_name_in|match_composite_lookup|edge_property_filter)' --baseline gql-scan-label-borrow-before`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/match_filter_project/1000` | 44.718 µs | 44.203 µs | Indexed scan setup now borrows single label/property keys when selecting label, typed-range, bitmap-union, and composite lookup candidate rows. Criterion reports -1.4356%, p=0.00. |
| `read_pipeline/match_name_in/1000` | 6.0401 µs | 6.0229 µs | Bitmap-union row stayed within Criterion's noise threshold. |
| `read_pipeline/match_composite_lookup/1000` | 446.32 ns | 437.56 ns | Composite lookup row improves -1.7612%, p=0.00. |
| `read_pipeline/edge_property_filter_no_index/1000` | 450.63 µs | 447.86 µs | Edge-property no-index guard stayed within Criterion's noise threshold. |
| `read_pipeline/edge_property_filter_indexed/1000` | 88.110 µs | 88.218 µs | Indexed edge-property guard stayed neutral (p=0.75). |

PR-local label-index residual label skip A/B:

Commands:
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench read_pipeline --filter 'read_pipeline/(match_limit10|match_prereturn_limit10|distinct_dedup|group_by_highcard|order_by_topk)' --save-baseline gql-label-index-skip-pre`;
`scripts/run-benches.sh --profile quick --sample-size 50 --measurement-time 4 --bench read_pipeline --filter 'read_pipeline/(match_limit10|match_prereturn_limit10|distinct_dedup|group_by_highcard|order_by_topk)' --baseline gql-label-index-skip-pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/order_by_topk/1000` | 104.18 µs | 98.397 µs | `LabelIndex` candidate rows already satisfy the single-label predicate, so scan collection now skips the redundant per-row label lookup. Criterion reports -5.4958%, p=0.00. |
| `read_pipeline/group_by_highcard/1000` | 107.11 µs | 101.20 µs | Same label-scan source feeding hash aggregate; Criterion reports -5.6090%, p=0.00. |
| `read_pipeline/distinct_dedup/1000` | 64.669 µs | 58.599 µs | DISTINCT over label-indexed Person rows improves -9.4288%, p=0.00. |
| `read_pipeline/match_limit10/1000` | 22.883 µs | 17.063 µs | Warm cached post-RETURN LIMIT row improves -25.544%, p=0.00. |
| `read_pipeline/match_prereturn_limit10/1000` | 22.883 µs | 17.091 µs | Pre-RETURN LIMIT row improves -25.397%, p=0.00. |
| `read_pipeline/match_limit10/cold/1000` | 66.048 µs | 59.763 µs | Fresh uncached session companion improves -9.5230%, p=0.00. |
| `read_pipeline/match_limit10/shared_cache/1000` | 22.980 µs | 17.096 µs | Fresh session over warmed shared source-plan cache improves -25.651%, p=0.00. |

PR-local bitmap-union row-filter A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter match_name_in`
and
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter edge_property_filter`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/match_name_in/1000` | 7.3209 µs | 6.8430 µs | 16-key `Person.name IN [...]` bitmap-union row over the maintained `Person(name)` index. Direct roaring bitmap union is −7.84% median, p=0.00. |
| `read_pipeline/edge_property_filter_indexed/1000` | 115.83 µs | 117.14 µs | Existing indexed edge-property guard stayed neutral after edge-row filters moved from `BTreeSet` membership to roaring bitmap membership (p=0.20). |

PR-local DISTINCT/GROUP BY hash-table reserve A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter distinct_dedup`
and
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter group_by_highcard`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/distinct_dedup/1000` | 80.366 µs | 77.178 µs | DISTINCT now reserves its runtime equality-key set from the input row count. Median is -4.0%. |
| `read_pipeline/group_by_highcard/1000` | 128.66 µs | 119.63 µs | GROUP BY now reserves its group vector and runtime equality-key index from the input row count capped by the configured group-key limit. Median is -7.0%; the initial A/B showed p=0.00, and the trimmed-patch rerun median is shown here. Full-profile sanity medians: 10k 889.49 µs, 50k 4.5333 ms, 100k 9.6804 ms. |

PR-local GROUP BY finalization inline-row A/B:

Commands: `scripts/run-benches.sh --profile quick --bench read_pipeline --filter group_by_highcard`;
then temporarily rerun the old finalization path and rerun after the change with
`scripts/run-benches.sh --profile full --bench read_pipeline --filter group_by_highcard`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/group_by_highcard/1000` | 122.33 µs | 117.85 µs | GROUP BY finalization now pushes aggregate results into `Binding` inline storage instead of allocating a representative-row `Vec` and one-value aggregate `Vec` per group. Criterion reports -3.8239%, p=0.00; a same-patch rerun measured 115.39 µs. |
| `read_pipeline/group_by_highcard/10000` | 864.60 µs | 872.37 µs | Larger-scale guard stayed neutral (p=0.28), so the win is scoped to small-group/finalization-sensitive rows. |
| `read_pipeline/group_by_highcard/50000` | 4.4961 ms | 4.5041 ms | Full-profile row stayed neutral (p=0.31). |
| `read_pipeline/group_by_highcard/100000` | 9.7185 ms | 9.6600 ms | Full-profile row stayed neutral (p=0.33). |

PR-local expansion inline-row A/B:

Commands:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter match --save-baseline gql-expand-inline-row-fresh-pre`;
same command with `--baseline gql-expand-inline-row-fresh-pre` after the
change; then
`scripts/run-benches.sh --profile full --bench read_pipeline --filter match_expand_hashjoin --save-baseline gql-expand-inline-row-full-pre`
and the same command with `--baseline gql-expand-inline-row-full-pre`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/match_expand_hashjoin/10000` | 12.187 ms | 11.786 ms | Expansion and hash-join merge rows now reuse `Binding` inline storage instead of round-tripping through a temporary heap `Vec`; Criterion reports -3.2915%, p=0.00. |
| `read_pipeline/match_expand_hashjoin/50000` | 84.071 ms | 82.471 ms | Larger join row trends lower (-1.9034%, p=0.02) but remains within Criterion's noise threshold. |
| `read_pipeline/match_expand_hashjoin/100000` | 173.35 ms | 169.23 ms | Largest full-profile row improves -2.3757%, p=0.00. Fresh quick guards over `match_filter_project`, `match_name_in`, `match_limit10`, `match_limit10/cold`, `match_limit10/shared_cache`, and `match_composite_lookup` reported no performance change. |

PR-local projection inline-row A/B:

Commands: temporarily rerun the old projection path, then rerun after the
change with
`scripts/run-benches.sh --profile full --bench read_pipeline --filter match_filter_project`.
Quick short-query guard:
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter match_limit10`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/match_filter_project/10000` | 572.55 µs | 553.02 µs | Projection now collects directly into `Binding` inline storage instead of allocating a temporary heap `Vec`; Criterion reports -3.8132%, p=0.00. |
| `read_pipeline/match_filter_project/50000` | 3.3006 ms | 3.2575 ms | Larger row trends lower but remains within Criterion's noise threshold (p=0.73). |
| `read_pipeline/match_filter_project/100000` | 7.7260 ms | 7.6296 ms | Largest row trends lower but remains within Criterion's noise threshold (p=0.10). Quick `match_limit10` guards stayed at the prior absolute envelope: ~30.24 µs warm, ~63.56 µs cold, ~30.07 µs shared-cache. |

PR-local single-item `LET` inline-row A/B:

Commands: add the `read_pipeline/let_single_extend` row, then compare the old
executor path and the single-item fast path with
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter let_single_extend`;
repeat with
`scripts/run-benches.sh --profile full --bench read_pipeline --filter let_single_extend`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/let_single_extend/1000` | 94.344 µs | 87.460 µs | Single-binding `LET` now evaluates against the input row directly and emits inline `Binding` storage, avoiding the temporary prefix row and prefix schema vector; the old-path quick rerun reported +7.8612% slower than the patched sample, p=0.00. |
| `read_pipeline/let_single_extend/10000` | 934.46 µs | 863.22 µs | Full-profile row improves -7.6837%, p=0.00. |
| `read_pipeline/let_single_extend/50000` | 5.4078 ms | 5.1450 ms | Full-profile row improves -5.2620%, p=0.00. |
| `read_pipeline/let_single_extend/100000` | 11.860 ms | 11.346 ms | Full-profile row improves -4.3361%, p=0.00. |

PR-local `FOR` row-expansion inline-row A/B:

Commands: add the `read_pipeline/for_expand_triple` row, then compare the old
executor path and the inline-storage row-expansion path with
`scripts/run-benches.sh --profile quick --bench read_pipeline --filter for_expand_triple`;
repeat with
`scripts/run-benches.sh --profile full --bench read_pipeline --filter for_expand_triple`.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `read_pipeline/for_expand_triple/1000` | 158.42 µs | 151.87 µs | `FOR` row expansion now clones into `Binding` inline storage and reserves the expanded-row output for each source list; the old-path quick rerun reported +4.3854% slower than the patched sample, p=0.00. |
| `read_pipeline/for_expand_triple/10000` | 1.6296 ms | 1.5397 ms | Full-profile row improves -5.7265%, p=0.00. |
| `read_pipeline/for_expand_triple/50000` | 9.5314 ms | 9.0549 ms | Full-profile row improves -4.9989%, p=0.00. |
| `read_pipeline/for_expand_triple/100000` | 20.399 ms | 19.526 ms | Full-profile row improves -4.2810%, p=0.00. |

PR-local B18/B20 same-session A/B (`scripts/run-benches.sh --profile full
--bench read_pipeline`) against development post-#707:

| Bench | Scale | Development post-B5 | B18/B20 | Notes |
|---|---:|---:|---:|---|
| `read_pipeline/match_filter_project` | 10k | 596.73 µs | 554.25 µs | −6.9% median. |
| `read_pipeline/match_filter_project` | 100k | 8.1311 ms | 7.6072 ms | −6.4% median; 50k row was neutral (`p = 0.19`). |
| `read_pipeline/match_expand_hashjoin` | 10k | 13.180 ms | 12.240 ms | −7.1% median; resolved hash-join keys + expand slots. |
| `read_pipeline/match_expand_hashjoin` | 100k | 180.94 ms | 169.10 ms | −6.5% median. |
| `read_pipeline/group_by_highcard` | 10k | 1.0024 ms | 935.37 µs | −7.7% median; aggregate slots borrow plan descriptors. |
| `read_pipeline/group_by_highcard` | 100k | 10.251 ms | 9.6415 ms | −5.9% median. |
| `read_pipeline/match_limit10` | 50k | 5.0124 ms | 4.5696 ms | −11.7% median; still scale-linear, B19 remains. |
| `read_pipeline/match_limit10/cold` | 50k | 5.2420 ms | 4.7762 ms | −8.3% median. |

## §6 selene-algorithms

Bench bins: `algo_bench`, `projection`, `vector_graph_retrieval`. Fixture:
`BenchFixture::build(N)` (≈3N edges) for pagerank/betweenness/apsp and
projection; `planted_community_graph(N)` (≈6N edges, ~N/64 communities) for
triangle_count, WCC/SCC, articulation_points, label_propagation, and louvain;
`dag_graph(N)` (≈3N edges) for pagerank_orientation, dijkstra, SSSP, and
topological_sort.
`vector_graph_retrieval` is the
first native graph+vector agent-memory research fixture: it stores topic-summary
vectors plus support, temporal-validity, and supersession edges to evidence
nodes, then compares vector-only ANN against PageRank rerank, graph expansion,
validity-aware expansion, supersession-aware expansion, and an exact
graph/vector oracle. Row IDs encode total graph coverage as
`covbp{basis points}`, current-valid coverage as `curbp{basis points}`, and
topic precision as `precbp{basis points}`.

### §6a Algorithm baselines (Sequential vs Auto)

| Bench | Scale | Sequential | Auto | Notes |
|---|---:|---:|---:|---|
| `algo/pagerank` | 10k | 89.01 µs | 88.99 µs | Auto now uses the sequential PageRank kernel after Rayon overhead lost at every measured scale. |
| `algo/pagerank` | 50k | 499.88 µs | 499.36 µs | Explicit `Threads(n)` still opts into the parallel kernel for caller-forced experiments. |
| `algo/pagerank` | 100k | 1.058 ms | 1.050 ms | Auto tracks the fastest measured policy on this sparse fixture. |
| `algo/pagerank_orientation/reverse` | 1k | 67.23 µs | n/a | Sequential-only quick DAG control row for non-natural orientation setup. |
| `algo/pagerank_orientation/undirected` | 1k | 48.03 µs | n/a | Sequential-only quick DAG row; undirected orientation setup now builds the out+in union from dense projection rows. |
| `algo/betweenness` | 10k | 25.52 ms | 7.73 ms | **3.3× Auto** — endpoint-aware sampling. |
| `algo/betweenness` | 50k | 135.3 ms | 44.95 ms | **3.0× Auto** — per-source SSSP parallelizes. |
| `algo/betweenness` | 100k | 266.1 ms | 101.7 ms | **2.6× Auto** — headline rayon win. |
| `algo/triangle_count` | 10k | 604.91 µs | 602.05 µs | Auto now stays sequential below the sparse-row threshold unless max degree trips the dense escape hatch. |
| `algo/triangle_count` | 50k | 3.066 ms | 2.472 ms | Auto still uses Rayon once the row count clears the threshold. |
| `algo/triangle_count` | 100k | 6.345 ms | 4.888 ms | Thresholded Auto preserves the large-row parallel win. |
| `algo/wcc` | 10k | 113.35 µs | n/a | Sequential-only; union-find and component-min tracking use dense projection rows. |
| `algo/wcc` | 50k | 575.8 µs | n/a | |
| `algo/wcc` | 100k | 1.167 ms | n/a | |
| `algo/wcc_count` | 10k | 101.06 µs | n/a | Sequential-only; count-only path shares the dense union-find state. |
| `algo/wcc_count` | 50k | 522.2 µs | n/a | |
| `algo/wcc_count` | 100k | 976.7 µs | n/a | |
| `algo/scc` | 10k | 161.71 µs | n/a | Sequential-only; Tarjan reads cached dense CSR neighbors directly. |
| `algo/scc` | 50k | 838.2 µs | n/a | |
| `algo/scc` | 100k | 1.681 ms | n/a | |
| `algo/scc_count` | 10k | 138.18 µs | n/a | Sequential-only; count-only path shares the dense Tarjan traversal state. |
| `algo/scc_count` | 50k | 730.4 µs | n/a | |
| `algo/scc_count` | 100k | 1.468 ms | n/a | |
| `algo/articulation_points` | 1k | 43.99 µs | n/a | Sequential-only; shared lowlink pass now builds undirected neighbor caches from dense projection rows and indexes the cache by dense row. |
| `algo/apsp` | 200 | 621.8 µs | 306.5 µs | All-pairs SSSP; scale = source count. |
| `algo/apsp` | 500 | 4.091 ms | 1.457 ms | 2.8× Auto. |
| `algo/apsp` | 1k | 17.17 ms | 5.576 ms | **3.1× Auto** — strong scaling at 10 cores. |
| `algo/dijkstra` | 1k | 6.923 µs | n/a | Sequential-only; DAG first-node to last-node shortest path now relaxes dense projection neighbor rows. |
| `algo/sssp` | 1k | 6.710 µs | n/a | Sequential-only; direct single-source pathfinding row over the DAG fixture. Command: `scripts/run-benches.sh --profile quick --bench algo_bench --filter sssp`. |
| `algo/topological_sort` | 10k | 89.09 µs | n/a | Sequential-only; in-degree accounting uses dense projection rows. |
| `algo/topological_sort` | 50k | 455.4 µs | n/a | |
| `algo/topological_sort` | 100k | 913.1 µs | n/a | |
| `algo/label_propagation` | 10k | 426.44 µs | n/a | Sequential-only; labels/counts use dense row IDs and dense scratch storage. |
| `algo/label_propagation` | 50k | 2.400 ms | n/a | |
| `algo/label_propagation` | 100k | 5.027 ms | n/a | |
| `algo/louvain` | 10k | 1.652 ms | n/a | Sequential-only; community degree sums now use dense vector storage. |
| `algo/louvain` | 50k | 9.015 ms | n/a | |
| `algo/louvain` | 100k | 18.57 ms | n/a | |

PR-local PageRank undirected dense-orientation A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter pagerank_orientation --save-baseline pagerank-orientation-dense-before`;
rerun with `--baseline pagerank-orientation-dense-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/pagerank_orientation/reverse/1k` | 74.453 µs | 67.231 µs | Control row: reverse keeps the existing public-neighbor path after the broader dense rewrite regressed this row, so this measured improvement is not attributed to the production change. |
| `algo/pagerank_orientation/undirected/1k` | 54.732 µs | 48.034 µs | Undirected orientation setup now reads dense out/in CSR rows directly before sort/dedup. The quick DAG row improves 13.47% (`p=0.00`). |

PR-local lowlink dense-indexed cache A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter articulation_points --save-baseline lowlink-cache-vector-before`;
rerun with `--baseline lowlink-cache-vector-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/articulation_points/1k` | 56.921 µs | 43.990 µs | The shared articulation/bridges lowlink pass now stores per-DFS neighbor caches in a dense `Vec<Option<Vec<u32>>>` instead of an integer hash map. The quick planted-community row improves 22.79% (`p=0.00`). |

PR-local lowlink dense-neighbor cache A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter articulation_points --save-baseline lowlink-dense-neighbors-before`;
rerun with `--baseline lowlink-dense-neighbors-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/articulation_points/1k` | 77.922 µs | 59.865 µs | The shared articulation/bridges lowlink pass now builds sorted-with-multiplicity undirected neighbor caches from dense out/in CSR rows instead of resolving dense row through `NodeId`. The quick planted-community row improves 24.28% (`p=0.00`). |

PR-local Dijkstra dense-neighbor relaxation A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter dijkstra --save-baseline dijkstra-dense-relax-before`;
rerun with `--baseline dijkstra-dense-relax-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/dijkstra/1k` | 9.6963 µs | 6.9229 µs | Dijkstra now relaxes projection CSR out-neighbor slices by dense row and materializes the source `NodeId` only on invalid-weight error paths. The quick DAG first-to-last row improves 28.56% (`p=0.00`). |

PR-local Louvain dense-neighbor scan A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter louvain --save-baseline louvain-dense-neighbors-before`;
rerun with `--baseline louvain-dense-neighbors-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/louvain/1k` | 132.70 µs | 107.10 µs | Louvain now scans projection CSR out- and in-neighbor slices by dense row in total-weight, weighted-degree, and community-neighbor loops instead of resolving dense row through `NodeId`. The quick planted-community row improves 19.65% (`p=0.00`). |

PR-local betweenness dense-adjacency build A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter 'betweenness/sequential' --save-baseline betweenness-dense-adjacency-before`;
rerun with `--baseline betweenness-dense-adjacency-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/betweenness/sequential/1k` | 2.3808 ms | 2.3430 ms | Betweenness dense adjacency build now reads projection CSR out-neighbor slices by dense row instead of converting dense row to `NodeId` and resolving back to dense. The quick sparse row improves 3.04% (`p=0.00`). |

PR-local SSSP dense-neighbor APSP A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter 'apsp/sequential' --save-baseline sssp-dense-neighbor-before`;
rerun with `--baseline sssp-dense-neighbor-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/apsp/sequential/200` | 1.0434 ms | 553.24 µs | SSSP now relaxes projection CSR out-neighbor slices by dense row and materializes the source `NodeId` only on invalid-weight error paths. The quick sparse row improves 32.65% (`p=0.00`). |
| `algo/apsp/sequential/500` | 8.1596 ms | 3.7973 ms | Same SSSP hot-loop change; improves 57.03% (`p=0.00`). |
| `algo/apsp/sequential/1k` | 17.457 ms | 15.311 ms | Same SSSP hot-loop change; improves 8.88% (`p=0.00`). |

PR-local label-propagation dense-neighbor A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter label_propagation --save-baseline label-propagation-dense-neighbors-before`;
rerun with `--baseline label-propagation-dense-neighbors-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/label_propagation/1k` | 51.168 µs | 26.706 µs | Label propagation now stays in dense projection row space for both out- and in-neighbor scans, avoiding a dense-row-to-`NodeId` lookup followed by a `NodeId`-to-dense lookup in the hot loop. The quick planted-community row improves 42.06% (`p=0.00`). |

PR-local sequential PageRank natural-CSR borrow A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter 'pagerank/sequential' --save-baseline pagerank-natural-borrow-before`;
rerun with `--baseline pagerank-natural-borrow-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/pagerank/sequential/1k` | 10.293 µs | 2.8465 µs | Natural-orientation sequential PageRank now borrows projection CSR out-neighbor slices instead of cloning dense neighbor IDs into an owned adjacency. Reverse and undirected orientations keep the existing owned-adjacency path. The quick sparse row improves 73.19% (`p=0.00`). |

PR-local SCC count-only materialization A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter scc_count --save-baseline scc-count-skip-components-before`;
rerun with `--baseline scc-count-skip-components-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/scc_count/1k` | 13.908 µs | 12.956 µs | Count-only Tarjan now skips completed-component `Vec` materialization while keeping the same traversal and stack cleanup. The quick planted-community row improves 6.40% (`p=0.00`). |

PR-local full topological-sort dense in-degree A/B:

Command: `scripts/run-benches.sh --profile full --bench algo_bench --filter 'topological_sort'`

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/topological_sort/10k` | 285.59 µs | 89.09 µs | Replaces `HashMap<NodeId, u32>` in-degree accounting and per-edge projection membership checks with dense `Vec<u32>` rows. |
| `algo/topological_sort/50k` | 1.5615 ms | 455.41 µs | The projection CSR already stores only projected neighbors and caches each neighbor's dense index. |
| `algo/topological_sort/100k` | 3.5344 ms | 913.05 µs | Preserves ASC-by-NodeId tie-breaking via `RowIndex` dense order. |

PR-local topological-sort ready-buffer reuse A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter topological_sort --save-baseline topo-reuse-ready-before`;
rerun with `--baseline topo-reuse-ready-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/topological_sort/1k` | 9.8913 µs | 6.3636 µs | Kahn's algorithm now reuses the next-batch scratch vector across levels and swaps it with the drained ready buffer after sorting. The quick DAG row improves 37.73% (`p=0.00`) while preserving per-batch dense-order tie-breaks. |

PR-local full label-propagation dense-count A/B:

Command: `scripts/run-benches.sh --profile full --bench algo_bench --filter 'algo/label_propagation'`

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/label_propagation/10k` | 893.73 µs | 426.44 µs | Replaces per-node label-count hash maps with dense label/count scratch arrays. |
| `algo/label_propagation/50k` | 4.7386 ms | 2.3997 ms | Dense labels preserve the smallest-NodeId tie-break because `RowIndex` dense order is ASC by NodeId. |
| `algo/label_propagation/100k` | 10.544 ms | 5.0273 ms | Same asynchronous-deterministic schedule and result ordering. |

PR-local full Louvain dense-accumulator A/B:

Command: `scripts/run-benches.sh --profile full --bench algo_bench --filter 'algo/louvain'`

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/louvain/10k` | 1.723 ms | 1.6519 ms | Replaces dense-key community-degree hash lookups with vector indexing. |
| `algo/louvain/50k` | 9.294 ms | 9.0146 ms | Same deterministic single-level Louvain semantics. |
| `algo/louvain/100k` | 19.31 ms | 18.569 ms | Modest improvement; Louvain remains sequential-only. |

PR-local Louvain inline community-weight A/B:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench algo_bench --filter louvain --save-baseline louvain-inline-weights-before`;
rerun with `--baseline louvain-inline-weights-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/louvain/1k` | 153.99 µs | 106.78 µs | Neighbor community weights now aggregate in an inline `SmallVec` for low-cardinality neighborhoods and spill to the existing `FxHashMap` only past the inline cap. The planted-community quick row improves 30.49% (`p=0.00`). |

PR-local full component dense-state A/B:

Command: `scripts/run-benches.sh --profile full --bench algo_bench --filter 'wcc|scc'`

| Bench | Before 10k | After 10k | Before 50k | After 50k | Before 100k | After 100k | Notes |
|---|---:|---:|---:|---:|---:|---:|---|
| `algo/wcc` | 168.19 µs | 113.35 µs | 1.0093 ms | 575.77 µs | 2.0974 ms | 1.1666 ms | Replaces component-min `HashMap` state and per-edge projection lookups with dense rows and cached dense neighbors. |
| `algo/wcc_count` | 146.63 µs | 101.06 µs | 912.03 µs | 522.18 µs | 1.9045 ms | 976.67 µs | Count-only path benefits from the same dense union pass. |
| `algo/scc` | 438.31 µs | 161.71 µs | 2.2798 ms | 838.20 µs | 4.8065 ms | 1.6811 ms | Removes the Tarjan neighbor cache and reads projection CSR neighbor slices directly. |
| `algo/scc_count` | 413.41 µs | 138.18 µs | 2.1570 ms | 730.39 µs | 4.5427 ms | 1.4676 ms | Count-only path shares the dense Tarjan traversal. |

### §6b `projection` — CSR foundation (ALGO-01/02/05)

Every algorithm runs *over* a projection, but `algo_bench` builds it in untimed
setup. This isolates the build (graph scan + CSR construction) and the per-edge
neighbor walk — exactly the two numbers the CSR dense-`u32` reshape
(ALGO-01/02/05) changes.

_§6a/§6b medians below are refreshed post-ALGO-01/02/05 (an A/B of development
HEAD vs the feature branch on this M5, profile `full`), so they run ahead of the
`3a864ac` north-star header until the next clean re-sweep. The dense-`u32` cache
trades a one-time **+4–7% `projection_build`** (one extra `u32` write per
neighbor, `ProjNeighbor` 24→32 B) for **−6 to −52%** across every algorithm that
resolved the dense index per edge (pagerank/louvain/apsp/triangle); even raw
`neighbor_iter` dropped −4 to −6%._

| Bench | 10k | 50k | 100k | Notes |
|---|---:|---:|---:|---|
| `algo/projection_build` | 1.338 ms | 16.10 ms | 41.50 ms | Full `GraphProjection::build`; +4–7% (the `dense:u32` write). |
| `algo/projection_neighbor_iter` | 20.7 µs | 128.0 µs | 291.9 µs | Sweep every node's out-neighbor slice. |

PR-local B13 projection-build transpose A/B:

Command:
`scripts/run-benches.sh --profile full --bench projection --filter projection_build --save-baseline b13_pre`;
rerun with `--baseline b13_pre` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/projection_build/10k` | 933.19 µs | 611.61 µs | Incoming CSR is derived by transposing the outgoing CSR, avoiding the second graph-adjacency scan and improving 32.9%. |
| `algo/projection_build/50k` | 11.057 ms | 4.5021 ms | Larger projections benefit most from removing the second count/fill pass and per-edge membership probes; this row improves 59.1%. |
| `algo/projection_build/100k` | 29.202 ms | 14.412 ms | 100k projection build improves 50.6% while preserving the out/in CSR transpose invariant. |

PR-local projection sorted-bucket guard:

Command:
`scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench projection --filter projection_build --save-baseline csr-skip-sorted-before`;
rerun with `--baseline csr-skip-sorted-before` after the implementation.

| Bench | Before | After | Notes |
|---|---:|---:|---|
| `algo/projection_build/1k` | 50.804 µs | 48.671 µs | CSR bucket ordering now checks whether each bucket is already sorted by neighbor `NodeId` and keeps the stable sort only for unsorted buckets. The common one-label projection row improves 4.16% (`p=0.00`). |

### §6c Graph-Augmented Vector Retrieval Research

Quick rows below are local research fixtures, not production API claims. The
fixture intentionally makes top-k vector retrieval semantically redundant:
nearest summaries are high-precision but low-coverage, while one-hop `SUPPORTS`
expansion can recover evidence facts from the graph. Half of evidence nodes are
stale by construction; `VALID_AT` edges identify current evidence, while
`SUPERSEDED_BY` edges link stale evidence to a current replacement. This lets
the fixture separate raw coverage from current-valid coverage, then compare
filtering against graph repair. Uniform and personalized PageRank scores plus
WCC component candidates are computed in fixture setup through real
`GraphProjection` paths; timed rows measure retrieval only. Expanded graph
candidates are exact-scored through the native candidate scoring primitive
before fact-diverse selection. The exact graph oracle uses exact vector search
plus validity-aware graph expansion to bound achievable fixture quality. The
companion `graph_vector_component_pressure` group widens graph-derived component
pools before the same exact candidate scorer, exposing when graph-bounded
scoring stops being cheap enough to beat ANN or compressed pre-scoring.

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_retrieval/vector_only/...covbp2500_curbp2500_precbp10000` | 209.93 µs | 501.11 µs (`covbp1367_curbp1347_precbp9843`) | Baseline ANN top-k: high topic precision, poor evidence coverage because summaries dominate nearest neighbors. |
| `graph_vector_retrieval/pagerank_prior/...covbp2500_curbp2500_precbp10000` | 233.87 µs | 528.91 µs (`covbp1445_curbp1425_precbp9843`) | Uniform PageRank reranking is still a weak prior: small 10k coverage gain over vector-only, no graph repair, and extra cost. |
| `graph_vector_retrieval/personalized_pagerank_prior/...covbp3790_curbp3790_precbp10000` | 231.87 µs | 530.71 µs (`covbp1425_curbp1347_precbp9843`) | Query-anchor personalized PageRank improves ANN-only coverage at 1k, but does not beat uniform PageRank or graph repair at 10k. Keep it as a benchmark-only prior until stronger rows justify an API. |
| `graph_vector_retrieval/graph_expand/...covbp10000_curbp5080_precbp10000` | 323.79 µs | 733.75 µs (`covbp8632_curbp4062_precbp9140`) | Raw one-hop expansion improves total coverage but often selects stale evidence; native candidate scoring trims rerank overhead. |
| `graph_vector_retrieval/graph_expand_valid/...covbp10000_curbp10000_precbp10000` | 300.94 µs | 672.65 µs (`covbp8222_curbp8222_precbp8652`) | Validity-aware expansion prunes stale candidates, making current-valid coverage match total coverage while running faster than raw expansion. |
| `graph_vector_retrieval/graph_expand_superseded/...covbp10000_curbp10000_precbp10000` | 293.97 µs | 755.50 µs (`covbp8632_curbp8632_precbp8886`) | Supersession-aware expansion repairs stale candidates through `SUPERSEDED_BY`; native candidate scoring makes this cheaper than validity filtering at 1k and improves current coverage at 10k. |
| `graph_vector_retrieval/graph_expand_valid_wide/...covbp10000_curbp10000_precbp10000` | 332.23 µs | 1.2304 ms (`covbp8769_curbp8769_precbp9765`) | Wider 16-hit ANN seeding improves current-valid coverage and precision, but the larger candidate fanout costs substantially more than narrow validity-aware expansion at 10k. |
| `graph_vector_retrieval/graph_expand_superseded_wide/...covbp10000_curbp10000_precbp10000` | 324.86 µs | 1.2516 ms (`covbp8769_curbp8769_precbp9765`) | Wide supersession matches wide validity quality on this fixture; supersession repair has no extra coverage upside once the wider seed already reaches current evidence. |
| `graph_vector_retrieval/graph_component_filter/...covbp10000_curbp10000_precbp10000` | 54.014 µs | 115.96 µs (`covbp10000_curbp10000_precbp10000`) | WCC-derived component filtering exact-scores only the query anchor's graph component, reaching oracle quality while avoiding global exact scan and ANN fanout. This remains the strongest positive graph-acceleration row. |
| `graph_vector_retrieval/graph_expand_pagerank/...covbp10000_curbp5080_precbp10000` | 385.72 µs | 847.47 µs (`covbp8632_curbp4062_precbp9140`) | Uniform PageRank on top of raw expansion adds cost without current-valid uplift; keep as a guardrail before promoting algorithm-prior policies. |
| `graph_vector_retrieval/graph_expand_personalized_pagerank/...covbp10000_curbp10000_precbp10000` | 391.32 µs | 850.10 µs (`covbp8632_curbp1406_precbp9140`) | Personalized PageRank can repair current coverage on the 1k fixture, but it badly underperforms explicit validity/supersession repair at 10k. This is negative evidence for PageRank-only currentness policy. |
| `graph_vector_retrieval/exact_graph_oracle/...covbp10000_curbp10000_precbp10000` | 1.4716 ms | 22.548 ms | Exact vector search plus validity-aware expansion reaches the fixture oracle, but it is far slower at 10k and only suitable as a research bound. |

Opt-in embedding rows are disabled unless `SELENE_EMBEDDING_BENCH=1` (or the
legacy `SELENE_OMLX_EMBEDDING_BENCH=1`) is set. New
`SELENE_EMBEDDING_BENCH=1` runs default to OpenRouter; set
`SELENE_EMBEDDING_PROVIDER=omlx` or use the legacy enable flag for the local
oMLX OpenAI-compatible endpoint. These rows are **not** expected to run in CI.
Use ignored `.env` keys only through the shell environment; do not commit or
print them:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_CORPUS=code_alias_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter query_root_current_state_intersection_batch
```

Local oMLX rows remain available explicitly:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=omlx \
SELENE_EMBEDDING_CORPUS=tiny \
SELENE_EMBEDDING_MODELS=Qwen3-Embedding-0.6B-4bit-DWQ,Qwen3-Embedding-4B-4bit-DWQ \
scripts/run-benches.sh --profile quick --bench vector_graph_retrieval --filter graph_vector_omlx_embedding_pressure --vector-scales 1000
```

OpenRouter Codestral Embed rows use the same corpora and benchmark surfaces:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=code_alias_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter query_root_current_state_intersection_batch
```

The reverse-order Codestral text/vector row compares vector-expanded candidates
followed by BM25 over those explicit candidates:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=code_alias_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat \
  --filter 'query_root_vector_text_batch|query_root_current_state_text_score_batch|query_root_current_state_intersection_batch|query_root_expansion_batch'
```

In that comparison run, the companion medians were 146.35 us for
current-state BM25, 222.94 us for current-state vector scoring, and 224.65 us
for plain graph-expanded vector scoring.

The wider 16-query code/alias profile uses the same benchmark row family:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=code_alias_wide_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat \
  --filter 'query_root_current_state_text_score_batch|query_root_current_state_text_vector_batch|query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch|query_root_vector_text_batch'
```

The project-code profile uses curated source-shaped snippets with real
selene-db module and symbol names:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=project_code_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat \
  --filter 'query_root_current_state_text_score_batch|query_root_current_state_text_vector_batch|query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch|query_root_vector_text_batch'
```

The project-code alias profile keeps real module/symbol names but adds
natural-language decoys around the same concepts:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=project_code_alias_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat \
  --filter 'query_root_current_state_text_score_batch|query_root_current_state_text_vector_batch|query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch|query_root_vector_text_batch'
```

The project source-code profile uses short real selene-db source excerpts as
target documents, plus source-shaped distractors and currentness decoys:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=project_source_code_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat \
  --filter 'query_root_current_state_text_score_batch|query_root_current_state_text_vector_batch|query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch|query_root_vector_text_batch'
```

The project source-chunk profile uses target-aware implementation snippets from
current selene-db modules. The first two documents per topic are non-target
graph-root hints, so `SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2` keeps targets in the
expanded support set:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=project_source_chunk_memory \
SELENE_EMBEDDING_BATCH_SIZE=4 \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat \
  --filter 'query_root_current_state_text_score_batch|query_root_current_state_text_vector_batch|query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch|query_root_vector_text_batch'
```

The project source-file profile embeds selected real selene-db files as
file-level documents. Use a smaller setup batch size for external providers so
full-file inputs are sent in conservative chunks:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=project_source_file_memory \
SELENE_EMBEDDING_BATCH_SIZE=4 \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat \
  --filter 'query_root_current_state_text_score_batch|query_root_current_state_text_vector_batch|query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch|query_root_vector_text_batch'
```

The project migration profile uses archived/prototype terminology as query
aliases, current first-class engine surfaces as target documents, and stale
decoy documents that maintained current-state should exclude:

```bash
set -a; source .env; set +a
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_PROVIDER=openrouter \
SELENE_EMBEDDING_CORPUS=project_migration_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat \
  --filter 'query_root_current_state_text_score_batch|query_root_current_state_text_vector_batch|query_root_current_state_intersection_batch|query_root_provenance_state_intersection_batch|query_root_expansion_batch|query_root_vector_text_batch'
```

The GQL query-root rows use the same local oMLX corpus but exercise the full
query pipeline. The materialization rows isolate `MATCH` plus
`WITH collect_list(root)` root production before any vector procedure runs; the
shape-pressure rows split that further into anchor lookup, root-row traversal,
and root-list aggregation. The reused-session rows compare root-only and
full-scoring shapes against a long-lived session with and without the existing
source-string `PlanCache`: plan caching collapses repeated root-only query
production, while full scoring is effectively neutral because vector rerank is
the dominant work. The scoring row derives `OmlxDependsOn` graph-hint roots,
passes them into `selene.vector_score_expanded_candidates`, and the procedure
expands roots through `OmlxSupports` before exact scoring. Its maintained-state
companion uses the same GQL-produced roots with
`selene.vector_score_candidate_state_expanded`, intersecting expanded roots
with the provider-maintained `omlx_support_facts` set. The negative-evidence
state row uses the same procedure over `omlx_current_support_facts`, where
documents containing stale/superseded/contradictory wording have an outgoing
`OmlxNegativeEvidence` edge and are excluded from current support state. The
provenance-required row uses `omlx_provenance_current_support_facts`, adding a
required incoming `OmlxSupports` edge and required outgoing `OmlxGroundedBy`
edge before the same exact rerank. The text-score rows use the same
GQL-produced roots, expand them through `OmlxSupports` in GQL, then call
`selene.text_score_nodes` over explicit candidate nodes using a maintained BM25
text index. The batched rows store each query vector and query text on the query
anchor and let GQL aggregate per-query root/candidate sets. The text batch row
calls `selene.text_score_nodes_batch` once for the full 16-query profile after
GQL expands graph roots to explicit candidates. The current-state text batch row
calls `selene.text_score_candidate_state_expanded_batch` once for the same
profile, keeping maintained-state composition, graph expansion, and BM25
scoring inside one procedure boundary. The text/vector fusion row uses the same
current-state BM25 batch row as a candidate producer, regroups its hits, and
then calls `selene.vector_score_nodes_batch` for exact vector rerank. The pure
expansion row calls `selene.vector_score_expanded_candidates_batch` once for the
full 16-query profile; the current/provenance vector rows call
`selene.vector_score_candidate_state_expanded_batch` so maintained state,
graph-expanded roots, and exact rerank stay inside one procedure boundary:

```bash
SELENE_EMBEDDING_BENCH=1 \
SELENE_EMBEDDING_CORPUS=scaled_ambiguous_memory \
SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 \
scripts/run-benches.sh --profile quick --bench procedure_call_repeat --filter procedure_vector_omlx_query_roots
```

Embedding requests are chunked by `SELENE_EMBEDDING_BATCH_SIZE` (or legacy
`SELENE_OMLX_EMBEDDING_BATCH_SIZE`; default: 64). Profiles above that size
preserve input order across multiple POSTs and fail if any response chunk does
not return exactly one vector per input.
`SELENE_EMBEDDING_CORPUS_REPEAT=N` (or legacy
`SELENE_OMLX_CORPUS_REPEAT=N`) repeats document inputs while keeping one copy
of each query. Duplicate documents keep their topic but clear target keys and
append a short duplicate marker to the submitted text, so target-hit rows keep a
unique original target document while index-pressure rows can grow a source
corpus beyond the default TurboQuant `c512` envelope.
`SELENE_GRAPH_HINT_DOCS_PER_TOPIC=N` caps graph-authored topic labels and
`OmlxDependsOn` edges to the first `N` same-topic documents per topic; unset
means every same-topic document receives graph hints. The partial-hint fixture
also adds `OmlxSupports` edges from graph-hint documents to same-topic support
facts so rows can compare direct partial hints, graph-expanded hints, and ANN
union against the same endpoint embeddings.
The fixture also registers a maintained candidate-state provider named
`omlx_support_facts` over an explicit `OmlxSupportFact` label, plus
`omlx_current_support_facts` over the same label with outgoing
`OmlxNegativeEvidence` edges as exclusions, and
`omlx_provenance_current_support_facts` over the same current-state rules plus
required incoming support and outgoing provenance edges. With partial graph
hints, graph-hint roots are not support facts; with uncapped hints, every
document remains a support fact. That models provider-maintained support facts
separately from graph-hint root documents without making the default uncapped
profile degenerate to an empty state. `curbp{basis points}` records current
support precision, while `basecurbp` records the direct graph-expanded row's
current precision before negative-evidence filtering. Target-aware local
profiles also add `hitbp{basis points}`, which records how many queries had
their expected target document in the returned top-k set.

The first local corpus is intentionally tiny (16 documents + 4 queries across
GQL, vector-index, agent-memory, and Rust-code topics). It validates that real
endpoint embeddings round-trip through `Value::Vector`, graph HNSW indexing,
exact cosine search, ANN search, and graph-label candidate-set scoring before
larger local corpus work. `SELENE_OMLX_CORPUS=agent_memory` expands that to
32 documents + 8 queries; `SELENE_OMLX_CORPUS=ambiguous_memory` keeps the same
shape but deliberately overlaps vocabulary across topics to stress vector-only
retrieval. `SELENE_OMLX_CORPUS=scaled_ambiguous_memory` combines both 40-input
profiles into 64 documents + 16 queries, crossing the default batch size as a
64+16 request pair. `SELENE_OMLX_CORPUS=code_alias_memory` adds a smaller
target-aware code/alias profile; `SELENE_OMLX_CORPUS=code_alias_wide_memory`
extends that shape to 16 target queries.
`SELENE_OMLX_CORPUS=project_code_memory`,
`project_code_alias_memory`, `project_source_code_memory`,
`project_source_chunk_memory`, and `project_source_file_memory` use current
selene-db module names, implementation snippets, or selected full files to
stress code/source retrieval. `project_workspace_source_memory` reads the
current checkout at benchmark setup time and extracts small line-numbered
windows around named source symbols, so it can feed live project code into
local embedding rows without committing more stale snippets.
`project_migration_memory` uses stale archived/prototype wording as query and
decoy pressure while targeting current engine files and procedures. These
target-aware profiles keep topic/current precision metrics but also record
`hitbp` so rows can show whether the expected symbol/fact/file was retrieved,
not only whether the result was in the right broad topic:

Current vector-index and retrieval policy matrix from the evidence above:

| Workload shape | Prefer | Evidence and caveats |
|---|---|---|
| Tiny or small full-label vector search, especially when `k`/search width covers the index | Exact scan or flat/exact index; HNSW only when approximate quality is acceptable | On live Codestral source chunks, the 32-document row has exact at 149.51 µs, HNSW at 137.90 µs, and TurboQuant after exact-covered bypass at 150.88 µs. At 544 documents, exact remains faster than TurboQuant (2.4743 ms vs 3.6629 ms) while preserving the same `precbp9218`. |
| Broad high-dimensional cosine search where topic-level quality matters and corpus size is comfortably beyond c512 | IVF when the live/clustered quality suffix holds; `TurboQuantCosine` when compressed exact-rerank state is the reason to build the index | The same-run 2k live Codestral source-chunk row has default IVF p2 at 393.62 µs with `precbp9218`, versus TurboQuant c512 at 6.4123 ms and exact at 9.4456 ms with the same topic suffix. Target-hit is much weaker (`hitbp5000` for IVF/HNSW, `hitbp5625` for exact/TurboQuant), so broad index-only search is not target-complete on this repeated target-aware corpus. `selene.create_vector_index(label, property, dimension)` therefore defaults to `ivf_cosine` for omitted kind; explicit `turbo_quant` remains the compressed exact-rerank path. |
| Lowest-latency approximate vector lookup where recall/precision loss is acceptable or can be repaired by a later exact/state stage | HNSW | The 2k Codestral row has HNSW ef64 at 918.94 µs, much faster than exact/TurboQuant, but with lower `precbp8593`. HNSW remains the latency-oriented ANN primitive; use exact rerank, graph/state gating, or wider/tuned settings when quality matters. |
| Coarse partitioning, cheap rebuild/recommended maintenance, clustered source corpora, or explicit list-count experiments | IVF | IVF rebuild and recommended-rebuild rows are cheap relative to HNSW construction (`ivf_cos_dim128` 2.124 ms at 1k, recommended 12.63 ms at 10k). The 2k live Codestral row now also gives IVF p2 the best latency while preserving the exact topic-precision suffix. This promotes IVF for the omitted native vector-index kind and clustered/source-shaped broad candidate generation, but its `hitbp5000` result means target-specific retrieval should still prefer graph/state/BM25 roots when available. |
| Tight graph-derived, maintained-state, BM25, JSON, or explicit candidate sets | Exact graph-candidate scoring or maintained candidate-state scoring | Candidate/state rows are microsecond-scale for c32-c128, and live source/code rows show graph-expanded current-state vector scoring around 300 µs with full target/current precision where applicable. This remains the default for agent-memory and source-shaped workflows when graph or text can produce a meaningful candidate set. |
| Lexical/currentness-rooted retrieval where BM25 already finds the target set | Maintained BM25/current-state, optionally followed by exact vector rerank or RRF only when it closes a measured quality gap | Live OpenRouter source/code rows repeatedly show BM25/current-state as fastest (roughly 170-195 µs on q16 source-shaped profiles) and often target-complete. Text/vector fusion usually preserves quality while adding millisecond-scale vector cost; vector-first BM25 often lowers current precision. RRF can restore full precision/currentness on alias-heavy rows, but the measured composition is slower than the best single maintained-state quality primitive, so it remains an A/B tool rather than a default fused policy. Snapshot-shared text-index state cuts body-update-heavy mixed rows by more than half at 10k/100k; bulk builder append recovers part of the rebuild/regeneration tax. Keep rebuild/recovery scheduling evidence in scope before broadening maintained text/JSON surfaces. |
| Partial graph roots/hints without support expansion or maintained state | Expand/intersect roots before final scoring; do not use raw partial hints as final retrieval | Partial graph-hint rows with `*_GRAPH_HINT_DOCS_PER_TOPIC=2` yield only `c2` and `precbp5000` for raw topic-label candidate scoring. Existing graph-expanded and maintained-state rows recover full topic/current precision by expanding roots through support edges and intersecting state before exact rerank. |

Current hybrid retrieval and maintenance stock-take (2026-06-13):

Confirmed defaults:

- Omitted native vector-index creation now defaults to `ivf_cosine`; exact scan or
  flat/exact remains the small/full-label oracle, HNSW remains the lowest-latency
  approximate path when recall loss is acceptable, and explicit
  `TurboQuantCosine` remains compressed derived state with exact primary-vector
  rerank rather than a blanket default.
- Target-aware source and memory retrieval should start from graph-derived roots,
  maintained candidate state, or maintained BM25 when those producers are
  target-complete. Live OpenRouter Codestral rows keep maintained BM25/current
  state around 170-195 µs on q16 source-shaped profiles, while maintained
  current-state vector scoring is the quality path when lexical candidates miss
  alias-heavy targets.
- JSON exact candidate procedures are correctness oracles and exact metadata
  filters. On the OpenRouter source-chunk row, JSON-current vector scoring kept
  full target/current quality but cost 1.1045 ms versus 318.39 µs for maintained
  current-state vector scoring; JSON-current BM25 cost 364.84 µs versus
  185.29 µs for maintained-state BM25.
- RRF over maintained BM25/current-state and maintained vector/current-state is
  a benchmarked A/B repair tool, not a default fused API. It restored full
  quality on the alias and source-chunk rows, but cost 534.14 µs and 487.45 µs
  respectively, slower than choosing the best single maintained-state primitive.
- Maintained BM25 read paths remain production-relevant, but maintenance
  scheduling matters. Snapshot-shared text-index state cut 100k mixed
  read/update cycles from 764.43 ms to 335.88 ms and write-only text updates
  from 495.04 ms to 62.795 ms; append-based bulk rebuild then cut 100k
  registration from 59.798 ms to 49.751 ms and 100k delete/compact rebuild from
  78.225 ms to 69.793 ms.

Benchmark-backed recommendations:

- Prefer maintained graph state plus the scoring primitive that wins the corpus:
  BM25/current-state for target-complete lexical/source rows, current-state
  vector scoring for alias or stale-decoy rows where lexical candidates miss,
  and exact graph-candidate scoring for tight root sets.
- Keep vector-first BM25, BM25-then-vector rerank, and RRF as benchmark
  compositions until a row shows a quality win that is not available by choosing
  the better existing primitive.
- Use candidate-scoped JSON filters when metadata expresses a required
  predicate or acts as an oracle; do not replace maintained current/support
  state with exact JSON scans when the maintained state already expresses the
  same set.
- Treat text-body churn and rebuild/recovery as the next maintenance pressure
  area before adding broader maintained text or JSON surfaces. The query path is
  cheap (`prebuilt_topic_query/n1000_k10` stayed at 36.528 µs), while 100k
  rebuild/compaction rows remain tens of milliseconds.
- Use OpenRouter live embedding endpoints for future real-validation rows:
  `mistralai/codestral-embed-2505` for code/source-shaped corpora and
  `google/gemini-embedding-2` for general memory/retrieval rows. Local oMLX rows
  are legacy comparison or explicit opt-in rows.

Rejected paths:

- Do not promote `TurboQuantCosine` as the default omitted index kind; the 2k
  OpenRouter source-chunk row made IVF faster at the same topic precision, while
  target-hit quality still required graph/state/text composition.
- Do not add a fused vector-first text API from the current evidence.
  Vector-first BM25 often lowered `precbp`/`curbp` and stayed slower than either
  BM25/current-state or current-state vector scoring.
- Do not make BM25-then-vector rerank a default repair path when the BM25
  candidate producer already missed the target; reranking cannot recover
  documents that never entered the candidate set and adds millisecond-scale
  exact-vector cost on 1536-dimensional OpenRouter rows.
- Do not use JSON-current scans as the default currentness mechanism when
  maintained current/support state is available and equivalent.
- Do not revive raw partial graph hints, ANN unions over precise graph-expanded
  sets, PageRank/Louvain-only candidate roots, postings-only text-index sharing,
  or `Arc<Vec<String>>` document-term sharing as defaults; each has measured
  quality or maintenance regressions in the rows above.

Remaining uncertainty:

- BM25 analyzer quality and sparse lexical-hit behavior still decide whether
  BM25 is merely the fastest root or a target-complete root on harder alias
  corpora.
- Durable JSON/path indexes are not justified yet; they need a focused design
  with recovery, migration, invalidation, and benchmark evidence showing exact
  JSON candidate scans are the bottleneck or quality-critical root.
- Text-index rebuild/recovery under real WAL/snapshot rotation is not fully
  characterized. The current rows isolate graph-side rebuild/compaction cost, not
  an end-to-end durable maintenance cycle.
- Mixed 60/40 maintenance with WAL durability plus maintained BM25, vector
  indexes, candidate states, and JSON metadata still needs a product-shaped row
  before broader storage or scheduling changes.

Next specific benchmark/design input:

- Add a WAL-backed 60/40 maintenance benchmark that seeds OpenRouter-derived
  source/memory corpora, registers maintained BM25 plus candidate states and a
  vector index, exercises JSON metadata filters, then measures query quality,
  update latency, rebuild/recovery, and compaction scheduling in one durable
  cycle. If that row shows exact JSON filters dominate or provide unique quality,
  follow with a maintained JSON/path index design; otherwise keep JSON exact and
  keep optimizing the text/vector/state maintenance path.

PR-local OpenRouter Codestral source-chunk vector-index guard:

Command:
`SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=mistralai/codestral-embed-2505 SELENE_EMBEDDING_CORPUS=project_source_chunk_memory SELENE_EMBEDDING_BATCH_SIZE=4 SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --sample-size 40 --measurement-time 4 --bench vector_graph_retrieval --filter "exact_graph_search|hnsw_graph_search|turbo_quant_graph_search"`.

Use `SELENE_EMBEDDING_CORPUS_REPEAT=17` or higher with the same row family to
push the 32-document source-chunk profile above the `TurboQuantCosine` default
`c512` search width without duplicating target queries.

| Row | Median | Notes |
|---|---:|---|
| `graph_vector_omlx_embedding_pressure/exact_graph_search/mistralai_codestral-embed-2505_32_q16_k4_dim1536_precbp8125` | 149.51 µs | Exact cosine over the 32-document source-chunk profile is the small-corpus oracle for the live Codestral embedding distribution. |
| `graph_vector_omlx_embedding_pressure/hnsw_graph_search/mistralai_codestral-embed-2505_32_q16_k4_ef64_dim1536_precbp8125` | 137.90 µs | HNSW mirrors exact topic precision and is slightly faster at this small project-source scale. |
| `graph_vector_omlx_embedding_pressure/turbo_quant_graph_search/mistralai_codestral-embed-2505_32_q16_k4_c512_dim1536_precbp8125` | 613.47 µs | Production `TurboQuantCosine` now has a live real-embedding row. The default `c512` envelope preserves the same precision but is intentionally oversized for 32 documents, so graph-scoped exact scoring remains the better tiny-corpus primitive. |

PR-local OpenRouter Codestral repeated source-chunk index-pressure guard:

Command:
`SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=mistralai/codestral-embed-2505 SELENE_EMBEDDING_CORPUS=project_source_chunk_memory SELENE_EMBEDDING_CORPUS_REPEAT=17 SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_graph_retrieval --filter "exact_graph_search|hnsw_graph_search|turbo_quant_graph_search"`.

| Row | Median | 95% CI | Notes |
|---|---:|---:|---|
| `graph_vector_omlx_embedding_pressure/exact_graph_search/mistralai_codestral-embed-2505_544_q16_k4_dim1536_precbp9218` | 2.4743 ms | 2.4736-2.4754 ms | Exact scan is still feasible at 544 source-chunk documents and remains the topic-precision oracle for this repeated live corpus. |
| `graph_vector_omlx_embedding_pressure/hnsw_graph_search/mistralai_codestral-embed-2505_544_q16_k4_ef64_dim1536_precbp8593` | 951.10 µs | 949.81-951.53 µs | HNSW is the latency winner but loses topic precision on this repeated source-shaped corpus. |
| `graph_vector_omlx_embedding_pressure/turbo_quant_graph_search/mistralai_codestral-embed-2505_544_q16_k4_c512_dim1536_precbp9218` | 3.6629 ms | 3.6598-3.6653 ms | TurboQuant c512 preserves the exact precision suffix after compressed preselection plus exact rerank, but at just above the c512 envelope it is slower than exact scan. Criterion reported no baseline p-value for this new row; outliers were 35% / 15% / 10% for exact / HNSW / TurboQuant. |

PR-local OpenRouter Codestral 2k repeated source-chunk policy guard:

Command:
`SELENE_EMBEDDING_BENCH=1 SELENE_EMBEDDING_PROVIDER=openrouter SELENE_EMBEDDING_MODELS=mistralai/codestral-embed-2505 SELENE_EMBEDDING_CORPUS=project_source_chunk_memory SELENE_EMBEDDING_CORPUS_REPEAT=64 SELENE_GRAPH_HINT_DOCS_PER_TOPIC=2 scripts/run-benches.sh --profile quick --sample-size 20 --measurement-time 2 --bench vector_graph_retrieval --filter "exact_graph_search|hnsw_graph_search|ivf_graph_search|turbo_quant_graph_search"`.

| Row | Median | 95% CI | Notes |
|---|---:|---:|---|
| `graph_vector_omlx_embedding_pressure/exact_graph_search/mistralai_codestral-embed-2505_2k_q16_k4_dim1536_precbp9218_hitbp5625` | 9.4456 ms | 9.4113-9.4622 ms | Exact scan remains the topic-precision oracle for the 2,048-document repeated source-chunk corpus, but exact vector similarity finds only 9 of 16 original target documents once duplicate same-topic documents are present. |
| `graph_vector_omlx_embedding_pressure/hnsw_graph_search/mistralai_codestral-embed-2505_2k_q16_k4_ef64_dim1536_precbp8593_hitbp5000` | 923.01 µs | 921.41-924.49 µs | HNSW is much faster than exact/TurboQuant, but loses topic precision and finds 8 of 16 expected target documents. |
| `graph_vector_omlx_embedding_pressure/ivf_graph_search/mistralai_codestral-embed-2505_2k_q16_k4_p2_dim1536_precbp9218_hitbp5000` | 393.62 µs | 393.47-394.60 µs | Default IVF search width p2 is the fastest same-run broad index row and preserves the exact topic-precision suffix, but finds only 8 of 16 original targets. |
| `graph_vector_omlx_embedding_pressure/turbo_quant_graph_search/mistralai_codestral-embed-2505_2k_q16_k4_c512_dim1536_precbp9218_hitbp5625` | 6.4123 ms | 6.3890-6.4494 ms | TurboQuant c512 beats exact scan while preserving the exact topic-precision suffix through compressed preselection plus exact rerank, but it is not the latency winner once IVF is included and still finds only 9 of 16 original targets. Criterion reported no baseline p-value for these new row IDs; outliers were 10% / 5% / 10% / 10% for exact / HNSW / TurboQuant / IVF. |

PR-local TurboQuant exact-covered fallback A/B:

Command: same live OpenRouter Codestral source-chunk guard as above, after
routing TurboQuant searches whose `search_width.max(k)` covers every indexed or
allowed row straight to exact primary-vector scoring.

| Row | Before | After | Notes |
|---|---:|---:|---|
| `graph_vector_omlx_embedding_pressure/exact_graph_search/...dim1536_precbp8125` | 149.51 µs | 149.65 µs | Exact oracle is unchanged on the same 32-document corpus. |
| `graph_vector_omlx_embedding_pressure/hnsw_graph_search/...ef64_dim1536_precbp8125` | 137.90 µs | 138.18 µs | HNSW guard is unchanged; the fallback is TurboQuant-only. |
| `graph_vector_omlx_embedding_pressure/turbo_quant_graph_search/...c512_dim1536_precbp8125` | 613.47 µs | 150.88 µs | Full-index TurboQuant no longer pays compressed preselection before exact rerank when the default c512 envelope already covers the whole tiny index. Precision remains 8125 bp. |

| oMLX row | Qwen3 0.6B / 1024 dim | Qwen3 4B / 2560 dim | Notes |
|---|---:|---:|---|
| `graph_vector_omlx_embedding_pressure/embed_batch/...docs20_batch64` | 39.23 ms | 208.8 ms | End-to-end localhost embedding request for 20 texts. |
| `graph_vector_omlx_embedding_pressure/exact_graph_search/...precbp6875` | 13.58 µs | 31.81 µs | Exact cosine over 16 stored endpoint vectors and 4 query vectors. |
| `graph_vector_omlx_embedding_pressure/hnsw_graph_search/...precbp6875` | 16.07 µs | 34.29 µs | HNSW cosine over the same vectors (`k=4`, `ef=64`). |
| `graph_vector_omlx_embedding_pressure/topic_label_candidate_score/...c4...precbp10000` | 4.11 µs | 9.46 µs | Candidate sets are derived from graph topic labels and batch-scored exactly. |
| `graph_vector_omlx_embedding_pressure/topic_neighbor_score/...c4...precbp10000` | 4.04 µs | 9.41 µs | Query-anchor `OmlxDependsOn` edges derive same-topic candidates through one-hop graph-neighbor scoring. |
| `graph_vector_omlx_embedding_pressure/topic_neighbor_batch_score/...c4...precbp10000` | 4.07 µs | 9.46 µs | Batched one-hop neighbor scoring over the same tiny profile. |
| `SELENE_OMLX_CORPUS=agent_memory` `topic_label_candidate_score/...c8...precbp10000` | 15.88 µs | 34.96 µs | Expanded 32-document / 8-query agent-memory profile; graph labels still restore full precision. |
| `SELENE_OMLX_CORPUS=agent_memory` `topic_neighbor_score/...c8...precbp10000` | 15.51 µs | 34.80 µs | Explicit graph-neighbor candidate derivation stays full-precision on the expanded profile. |
| `SELENE_OMLX_CORPUS=agent_memory` `topic_neighbor_batch_score/...c8...precbp10000` | 15.61 µs | 34.74 µs | Batched one-hop neighbor scoring over the expanded profile. |
| `SELENE_OMLX_CORPUS=ambiguous_memory` `exact_graph_search/...` | 51.72 µs | 122.08 µs | Vector-only exact scan drops to `precbp6250` / `precbp4375` under cross-topic vocabulary overlap. |
| `SELENE_OMLX_CORPUS=ambiguous_memory` `hnsw_graph_search/...ef64...` | 74.46 µs | 152.15 µs | HNSW mirrors exact precision on this profile, but is slower at this tiny scale. |
| `SELENE_OMLX_CORPUS=ambiguous_memory` `topic_label_candidate_score/...c8...precbp10000` | 15.63 µs | 35.11 µs | Graph-label candidate sets restore full precision despite semantic cross-talk. |
| `SELENE_OMLX_CORPUS=ambiguous_memory` `topic_neighbor_score/...c8...precbp10000` | 15.47 µs | 34.93 µs | Explicit graph-neighbor candidates restore full precision with similar latency. |
| `SELENE_OMLX_CORPUS=ambiguous_memory` `topic_neighbor_batch_score/...c8...precbp10000` | 15.57 µs | 35.05 µs | Batched one-hop neighbor scoring over the ambiguity-stress profile. |
| `SELENE_OMLX_CORPUS=scaled_ambiguous_memory` `embed_batch/...docs80_batch64` | 132.97 ms | 822.68 ms | Two local embedding POSTs (64 + 16 inputs) over the scaled 80-input profile. |
| `SELENE_OMLX_CORPUS=scaled_ambiguous_memory` `exact_graph_search/...` | 199.12 µs | 478.63 µs | Vector-only exact scan drops to `precbp5625` / `precbp4843` at 64 documents + 16 queries. |
| `SELENE_OMLX_CORPUS=scaled_ambiguous_memory` `hnsw_graph_search/...ef64...` | 295.10 µs | 583.72 µs | HNSW mirrors exact precision on the scaled profile, remaining slower at this local size. |
| `SELENE_OMLX_CORPUS=scaled_ambiguous_memory` `topic_label_candidate_score/...c16...precbp10000` | 59.93 µs | 133.58 µs | Graph-label candidate sets restore full precision over 16 same-topic documents per query. |
| `SELENE_OMLX_CORPUS=scaled_ambiguous_memory` `topic_neighbor_score/...c16...precbp10000` | 59.69 µs | 133.92 µs | Explicit graph-neighbor candidates preserve full precision with equivalent candidate width. |
| `SELENE_OMLX_CORPUS=scaled_ambiguous_memory` `topic_neighbor_batch_score/...c16...precbp10000` | 59.90 µs | 134.19 µs | Batched one-hop neighbor scoring over the scaled 80-input profile. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_label_candidate_score/...c2...precbp5000` | 9.65 µs | 22.46 µs | Partial graph labels cap graph-only precision at 2 of 4 hits per query while staying cheap. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_neighbor_score/...c2...precbp5000` | 9.63 µs | 22.31 µs | Partial graph-neighbor hints have the same c2 quality/cost shape as labels. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_neighbor_batch_score/...c2...precbp5000` | 9.64 µs | 22.40 µs | Batched partial neighbor hints are neutral at this tiny candidate width. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_label_ann_union_score/...precbp5625/4843...ann8` | 332.30 µs | 668.43 µs | Small ANN union raises the 1024-dim model only to vector-only precision and lowers the 2560-dim model below c2 graph-only precision. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_neighbor_ann_union_score/...precbp5625/4843...ann8` | 331.61 µs | 668.26 µs | Same negative fallback result through explicit graph-neighbor candidates. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_hint_expansion_score/...c16...precbp10000` | 61.74 µs | 135.29 µs | Two direct graph hints per topic expand through `OmlxSupports` to the full same-topic support set, restoring full precision with the same width as complete graph labels. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_hint_expansion_cached_score/...c16...precbp10000` | 58.32 µs | 132.14 µs | Precomputing the same expanded support candidate sets trims query-time graph traversal overhead while preserving full precision. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_hint_expansion_state_score/...c14...precbp10000` | 55.15 µs | 119.96 µs | Intersects graph-expanded hints with maintained `omlx_support_facts`, filtering root hint docs while preserving full precision. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_hint_expansion_refresh_sets/...q16_c16_totalc256` | 2.97 µs | 3.05 µs | Recomputes every cached support candidate set from graph topology and asserts it matches cached state; refresh cost is small at this hot-scope size. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_hint_expansion_cached_r60w40/...r60w40_totalc256` | 3.64 ms | 8.05 ms | Conservative mixed cycle with 60 cached candidate-set scoring reads plus 40 full graph-topology refreshes via the production candidate-expansion API; vector rerank dominates refresh work on this local profile. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `topic_hint_expansion_ann_union_score/...precbp5625/4843...ann8` | 371.82 µs (`c22`) | 754.71 µs (`c21`) | ANN union after full graph expansion hurts precision and adds hundreds of microseconds; avoid widening precise graph-expanded candidate sets with ANN by default. |
| `SELENE_OMLX_GRAPH_HINT_DOCS_PER_TOPIC=2` `ann_hint_expansion_state_score/...ann8` | 392.51 µs (`precbp5312`, `c44`) | 699.14 µs (`precbp4531`, `c42`) | ANN roots expanded through support edges and intersected with maintained support-fact state still miss too many target facts; avoid adding a batched ANN/state procedure until a workload shows better quality. |
| `procedure_vector_omlx_query_roots/shared_cache_query_anchor_lookup/...q16_anchors16...` | 703.69 µs | 691.54 µs | Repeated per-query lookup of one `OmlxQueryAnchor` by `query_index`, creating a fresh `Session` for each query. |
| `procedure_vector_omlx_query_roots/shared_session_query_anchor_lookup/...q16_anchors16...` | 697.91 µs | 684.55 µs | Reuses one session but does not enable `PlanCache`; this is effectively neutral versus fresh sessions. |
| `procedure_vector_omlx_query_roots/shared_session_plan_cache_query_anchor_lookup/...q16_anchors16...` | 38.46 µs | 38.95 µs | Reuses one hot source-string `PlanCache` session; parse/analyze/plan are skipped for repeated parameterized root-shape queries. |
| `procedure_vector_omlx_query_roots/shared_cache_query_anchor_lookup_batch/...q16_anchors16...` | 54.73 µs | 54.58 µs | Single GQL statement returns all 16 query anchors ordered by `query_index`; this is the lower bound for batched root-shape execution. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_rows/...q16_r2_totalr32...` | 789.15 µs | 786.71 µs | Repeated per-query `OmlxDependsOn` traversal returns two root rows per query without `collect_list` aggregation. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_rows_batch/...q16_r2_totalr32...` | 67.73 µs | 67.55 µs | Single statement returns all 32 root rows; edge traversal adds ~13 µs over batched anchor lookup on this fixture. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_materialize/...q16_r2_totalr32...` | 918.45 µs | 918.35 µs | Repeated per-query GQL root production: `MATCH` + `collect_list(root)` materializes two roots for each of 16 query anchors, without vector procedure dispatch or scoring. |
| `procedure_vector_omlx_query_roots/shared_session_query_root_materialize/...q16_r2_totalr32...` | 912.40 µs | 917.28 µs | Reuses one session for the same materialization source, again showing little benefit without `PlanCache`. |
| `procedure_vector_omlx_query_roots/shared_session_plan_cache_query_root_materialize/...q16_r2_totalr32...` | 52.96 µs | 53.70 µs | Reuses one hot `PlanCache` session for the materialization source, making repeated root production competitive with the batched root-row shape. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_materialize_batch/...q16_r2_totalr32...` | 78.85 µs | 77.95 µs | Single GQL statement materializes all 16 root sets; aggregation adds ~11 µs over batched root-row traversal before vector scoring enters the path. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_expansion/...q16_k4_r2_c16...precbp10000` | 1.45 ms | 1.52 ms | Full GQL row over the scaled partial-hint corpus: `MATCH` + `collect_list(root)` derives two roots per query, graph expansion restores the 16-document same-topic set, and all 64 returned top-k hits are on-topic; current-fact precision is lower (`basecurbp8593/8281`) because stale same-topic facts remain eligible. |
| `procedure_vector_omlx_query_roots/shared_session_plan_cache_query_root_expansion/...q16_k4_r2_c16...precbp10000` | 1.45 ms | 1.52 ms | Same full scoring statement through a warmed source-string `PlanCache` session; plan caching is neutral once graph expansion and vector rerank dominate. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_text_score/...q16_k4_r2_c16...precbp10000_curbp9218` | 2.16 ms | 2.22 ms | Full GQL row over the same graph roots and support expansion, but reranks expanded candidates with maintained BM25 via `selene.text_score_nodes`; topic precision is full, while current-fact precision is partial because BM25 still admits some stale same-topic facts. |
| `procedure_vector_omlx_query_roots/shared_session_plan_cache_query_root_text_score/...q16_k4_r2_c16...precbp10000_curbp9218` | 2.19 ms | 2.23 ms | Warmed source-string `PlanCache` session for the same text-score row; plan caching is effectively neutral because repeated GQL candidate production dominates. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_text_score_batch/...q16_k4_r2_c16...precbp10000_curbp9218` | 472.70 µs | 474.70 µs | Single GQL statement builds all 16 query texts and graph-expanded candidate sets, then calls `selene.text_score_nodes_batch` once; preserves full topic precision while exposing the same currentness gap as the repeated text scorer. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c13...precbp10000_curbp10000` | 180.19 µs | 182.71 µs | Single GQL statement builds all 16 query texts and root sets, then calls `selene.text_score_candidate_state_expanded_batch`; maintained current-state composition restores full current precision and avoids explicit candidate materialization. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_text_vector_batch/...q16_k4_r2_c13...precbp10000_curbp10000` | 2.3434 ms | 5.3866 ms | Current-state BM25 batch produces top-k candidates, then `selene.vector_score_nodes_batch` reranks them. Quality stays full, but the extra vector pass is much slower on this fixture; do not recommend text/vector fusion here without a quality gap. |
| `SELENE_OMLX_CORPUS=code_alias_memory` `shared_cache_query_root_current_state_text_score_batch/...q8_k4_r2_c6...precbp5000_curbp5000_hitbp8750` | 144.57 µs | 145.39 µs | Target-aware code/alias profile. Sparse BM25 emits fewer than `k=4` rows per query, but seven of eight expected target facts appear in top-k; broad topic/current precision alone would miss this target-level signal. |
| `SELENE_OMLX_CORPUS=code_alias_memory` `shared_cache_query_root_current_state_text_vector_batch/...q8_k4_r2_c6...precbp5000_curbp5000_hitbp8750` | 553.96 µs | 1.0504 ms | Exact vector rerank after the same BM25/state candidate producer does not improve target hits on this profile and adds dimension-sensitive cost. |
| `SELENE_OMLX_CORPUS=code_alias_memory` `shared_cache_query_root_expansion_batch/...q8_k4_r2_c9...precbp10000_hitbp8750` | 178.74 µs | 257.01 µs | Plain graph-expanded vector scoring is faster than maintained-state vector scoring, but it still misses one expected target and does not apply the current-state gate. |
| `SELENE_OMLX_CORPUS=code_alias_memory` `shared_cache_query_root_current_state_intersection_batch/...q8_k4_r2_c6...basecurbp8125_curbp10000_hitbp10000` | 210.17 µs | 287.61 µs | Batched graph-root vector scoring through maintained current state recovers all expected code/alias targets. This is slower than BM25/state but fixes the missing-target case without adding a fusion API. |
| `SELENE_EMBEDDING_PROVIDER=openrouter` `mistralai/codestral-embed-2505` `shared_cache_query_root_expansion_batch/...q8_k4_r2_c9...dim1536_precbp10000_hitbp8750` | 211.05 µs | - | OpenRouter Codestral Embed 2505 through the same code-alias corpus. Plain graph-expanded vector scoring again misses one expected target. |
| `SELENE_EMBEDDING_PROVIDER=openrouter` `mistralai/codestral-embed-2505` `shared_cache_query_root_current_state_intersection_batch/...q8_k4_r2_c6...dim1536_basecurbp7812_curbp10000_hitbp10000` | 211.95 µs | - | Maintained current-state vector scoring recovers all expected code-alias targets with effectively the same latency as plain expansion on this profile. |
| `SELENE_EMBEDDING_PROVIDER=openrouter` `mistralai/codestral-embed-2505` `shared_cache_query_root_provenance_state_intersection_batch/...q8_k4_r2_c6...dim1536_basecurbp7812_curbp10000_hitbp10000` | 212.38 µs | - | Provenance-gated current-state vector scoring preserves the same target quality as the current-state vector batch with negligible extra latency on this code-alias fixture. |
| `SELENE_EMBEDDING_PROVIDER=openrouter` `mistralai/codestral-embed-2505` `shared_cache_query_root_current_state_text_score_batch/...q8_k4_r2_c6...dim1536_precbp5000_curbp5000_hitbp8750` | 136.79 µs | - | Maintained current-state BM25 remains the fastest Codestral-backed code-alias row, but sparse lexical matches still miss one expected target. |
| `SELENE_EMBEDDING_PROVIDER=openrouter` `mistralai/codestral-embed-2505` `shared_cache_query_root_current_state_text_vector_batch/...q8_k4_r2_c6...dim1536_precbp5000_curbp5000_hitbp8750` | 706.33 µs | - | Vector rerank after the BM25/current-state candidate pass keeps the same seven-of-eight target hit shape and adds substantial dimension-sensitive cost. |
| `SELENE_EMBEDDING_PROVIDER=openrouter` `mistralai/codestral-embed-2505` `shared_cache_query_root_vector_text_batch/...q8_k4_r2_c4...dim1536_precbp6562_curbp5312_hitbp8750` | 327.96 µs | - | Reverse-order fusion: vector-expanded top-k candidates feed `selene.text_score_nodes_batch`. It is slower than maintained BM25 and current-state vector baselines on the same run, while keeping the same seven-of-eight target hit shape and lowering topic/current precision. |
| `SELENE_EMBEDDING_CORPUS=code_alias_wide_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c8...dim1536_precbp6250_curbp6250_hitbp8750` | 171.51 µs | - | Wider 16-query Codestral code/alias profile. BM25/current-state remains fastest but finds 14 of 16 expected target facts. |
| `SELENE_EMBEDDING_CORPUS=code_alias_wide_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_vector_batch/...q16_k4_r2_c8...dim1536_precbp6250_curbp6250_hitbp8750` | 2.6202 ms | - | Vector rerank after BM25/current-state keeps the same target-hit and precision shape while adding substantial cost. |
| `SELENE_EMBEDDING_CORPUS=code_alias_wide_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_expansion_batch/...q16_k4_r2_c11...dim1536_precbp10000_hitbp9375` | 344.60 µs | - | Plain graph-expanded vector scoring finds 15 of 16 expected targets and keeps full broad-topic precision, but has no current-state gate. |
| `SELENE_EMBEDDING_CORPUS=code_alias_wide_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c8...dim1536_basecurbp7968_curbp10000_hitbp8750` | 331.99 µs | - | Maintained current-state vector scoring restores current precision but drops to the same 14-of-16 target-hit shape as BM25/current-state on this wider profile. |
| `SELENE_EMBEDDING_CORPUS=code_alias_wide_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_provenance_state_intersection_batch/...q16_k4_r2_c8...dim1536_basecurbp7968_curbp10000_hitbp8750` | 346.01 µs | - | Provenance-required current state preserves the same quality shape as exclusion-only current state with modest extra cost. |
| `SELENE_EMBEDDING_CORPUS=code_alias_wide_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_vector_text_batch/...q16_k4_r2_c4...dim1536_precbp7031_curbp5781_hitbp9375` | 522.62 µs | - | Vector-first BM25 finds 15 of 16 expected targets like plain vector scoring, but loses broad/current precision and costs more than current-state vector scoring. |
| `SELENE_EMBEDDING_CORPUS=project_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c6...dim1536_precbp9531_curbp9531_hitbp10000` | 170.50 µs | - | Curated project-code profile using real selene-db module and symbol names. Maintained BM25/current-state is fastest and finds all expected source-shaped targets, with one off-topic/current miss across 64 returned rows. |
| `SELENE_EMBEDDING_CORPUS=project_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_vector_batch/...q16_k4_r2_c6...dim1536_precbp9531_curbp9531_hitbp10000` | 3.1734 ms | - | Vector rerank after BM25/current-state preserves target hits and precision but adds a large exact-vector pass. |
| `SELENE_EMBEDDING_CORPUS=project_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_expansion_batch/...q16_k4_r2_c8...dim1536_precbp10000_hitbp10000` | 310.13 µs | - | Plain graph-expanded vector scoring reaches full topic precision and all 16 targets on the source-shaped corpus, but does not apply maintained current-state semantics. |
| `SELENE_EMBEDDING_CORPUS=project_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c6...dim1536_basecurbp10000_curbp10000_hitbp10000` | 306.32 µs | - | Maintained current-state vector scoring keeps full current precision and all expected targets, about 1.8x slower than BM25/current-state on this profile. |
| `SELENE_EMBEDDING_CORPUS=project_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_provenance_state_intersection_batch/...q16_k4_r2_c6...dim1536_basecurbp10000_curbp10000_hitbp10000` | 307.69 µs | - | Provenance-required current state preserves the same quality shape as current state with negligible extra cost. |
| `SELENE_EMBEDDING_CORPUS=project_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_vector_text_batch/...q16_k4_r2_c4...dim1536_precbp8750_curbp8750_hitbp10000` | 486.06 µs | - | Vector-first BM25 finds all expected targets after pruning to top-k vector candidates, but lowers broad/current precision and remains slower than either BM25/current-state or current-state vector scoring. |
| `SELENE_EMBEDDING_CORPUS=project_code_alias_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c8...dim1536_precbp9218_curbp9218_hitbp9375` | 170.28 µs | - | Harder source-shaped alias profile with lexical decoys. BM25/current-state remains fastest but misses one expected target. |
| `SELENE_EMBEDDING_CORPUS=project_code_alias_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_vector_batch/...q16_k4_r2_c8...dim1536_precbp9218_curbp9218_hitbp9375` | 3.1401 ms | - | Vector rerank after BM25/current-state keeps the same 15-of-16 target-hit shape because the missing target is not in the lexical candidate set. |
| `SELENE_EMBEDDING_CORPUS=project_code_alias_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_expansion_batch/...q16_k4_r2_c10...dim1536_precbp10000_hitbp8750` | 318.59 µs | - | Plain graph-expanded vector scoring keeps full broad-topic precision but finds only 14 of 16 expected alias targets. |
| `SELENE_EMBEDDING_CORPUS=project_code_alias_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c8...dim1536_basecurbp9687_curbp10000_hitbp10000` | 314.32 µs | - | Maintained current-state vector scoring recovers all expected alias targets and restores full current precision. |
| `SELENE_EMBEDDING_CORPUS=project_code_alias_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_provenance_state_intersection_batch/...q16_k4_r2_c8...dim1536_basecurbp9687_curbp10000_hitbp10000` | 314.61 µs | - | Provenance-required current state preserves the current-state vector quality with negligible extra cost. |
| `SELENE_EMBEDDING_CORPUS=project_code_alias_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_vector_text_batch/...q16_k4_r2_c4...dim1536_precbp8906_curbp8593_hitbp8750` | 495.39 µs | - | Vector-first BM25 is again negative: it misses two targets, lowers current precision, and is slower than both BM25/current-state and current-state vector scoring. |
| `SELENE_EMBEDDING_CORPUS=project_source_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c6...dim1536_precbp9843_curbp9843_hitbp10000` | 195.52 µs | - | Real source-excerpt profile. Maintained BM25/current-state is target-complete and fastest, with one broad/current miss across 64 returned rows. |
| `SELENE_EMBEDDING_CORPUS=project_source_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_vector_batch/...q16_k4_r2_c6...dim1536_precbp9843_curbp9843_hitbp10000` | 3.2922 ms | - | Vector rerank after target-complete BM25/current-state keeps the same quality shape and adds a large exact-vector pass. |
| `SELENE_EMBEDDING_CORPUS=project_source_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_expansion_batch/...q16_k4_r2_c10...dim1536_precbp10000_hitbp9375` | 344.62 µs | - | Plain graph-expanded vector scoring keeps full broad-topic precision but misses one expected source target and has no current-state gate. |
| `SELENE_EMBEDDING_CORPUS=project_source_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c6...dim1536_basecurbp8906_curbp10000_hitbp10000` | 328.21 µs | - | Maintained current-state vector scoring restores full current precision and remains target-complete on real source excerpts. |
| `SELENE_EMBEDDING_CORPUS=project_source_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_provenance_state_intersection_batch/...q16_k4_r2_c6...dim1536_basecurbp8906_curbp10000_hitbp10000` | 330.03 µs | - | Provenance-required current state preserves the current-state vector quality with negligible extra cost. |
| `SELENE_EMBEDDING_CORPUS=project_source_code_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_vector_text_batch/...q16_k4_r2_c4...dim1536_precbp9531_curbp8593_hitbp9375` | 527.43 µs | - | Vector-first BM25 misses one target, lowers current precision, and remains slower than both BM25/current-state and current-state vector scoring. |
| `SELENE_EMBEDDING_CORPUS=project_source_chunk_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c6...dim1536_precbp9375_curbp9375_hitbp10000` | 193.15 µs | - | Source-chunk profile with target-aware implementation snippets. Maintained BM25/current-state is fastest and target-complete, but has one broad/current miss across 64 returned rows. |
| `SELENE_EMBEDDING_CORPUS=project_source_chunk_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_vector_batch/...q16_k4_r2_c6...dim1536_precbp9375_curbp9375_hitbp10000` | 3.2712 ms | - | Vector rerank after BM25/current-state keeps the same target/precision shape and adds a large exact-vector pass. |
| `SELENE_EMBEDDING_CORPUS=project_source_chunk_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_expansion_batch/...q16_k4_r2_c8...dim1536_precbp10000_hitbp10000` | 305.43 µs | - | Live Codestral rerun after the d3072 TurboQuant guard work. Plain graph-expanded vector scoring reaches full target and broad-topic precision, but does not apply maintained current-state semantics. |
| `SELENE_EMBEDDING_CORPUS=project_source_chunk_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c6...dim1536_basecurbp9687_curbp10000_hitbp10000` | 301.28 µs | - | Maintained current-state vector scoring is target-complete and restores full current precision while staying slightly faster than plain expansion on this source-chunk run. |
| `SELENE_EMBEDDING_CORPUS=project_source_chunk_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_provenance_state_intersection_batch/...q16_k4_r2_c6...dim1536_basecurbp9687_curbp10000_hitbp10000` | 300.93 µs | - | Provenance-required current state preserves the current-state vector quality with effectively the same latency as exclusion-only current state. |
| `SELENE_EMBEDDING_CORPUS=project_source_chunk_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_vector_text_batch/...q16_k4_r2_c4...dim1536_precbp9843_curbp9531_hitbp10000` | 523.56 µs | - | Vector-first BM25 is target-complete and has better broad precision than BM25/current-state on this chunk profile, but it remains slower than current-state vector and less current-precise. |
| `SELENE_EMBEDDING_CORPUS=project_source_file_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_score_batch/...q8_k4_r2_c4...dim1536_precbp9375_curbp9375_hitbp10000` | 142.50 µs | - | File-level source corpus with selected real selene-db files. Maintained BM25/current-state is target-complete and fastest, with one broad/current miss across 32 returned rows. |
| `SELENE_EMBEDDING_CORPUS=project_source_file_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_vector_batch/...q8_k4_r2_c4...dim1536_precbp9375_curbp9375_hitbp10000` | 1.0473 ms | - | Vector rerank after BM25/current-state preserves quality but adds a large exact-vector pass even at q8/c4. |
| `SELENE_EMBEDDING_CORPUS=project_source_file_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_expansion_batch/...q8_k4_r2_c6...dim1536_precbp10000_hitbp10000` | 210.97 µs | - | Plain graph-expanded vector scoring is target-complete and full topic precision, but has no current-state gate. |
| `SELENE_EMBEDDING_CORPUS=project_source_file_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_intersection_batch/...q8_k4_r2_c4...dim1536_basecurbp10000_curbp10000_hitbp10000` | 212.75 µs | - | Maintained current-state vector scoring keeps full target/current precision, about 1.5x slower than BM25/current-state. |
| `SELENE_EMBEDDING_CORPUS=project_source_file_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_provenance_state_intersection_batch/...q8_k4_r2_c4...dim1536_basecurbp10000_curbp10000_hitbp10000` | 213.11 µs | - | Provenance-required current state preserves the current-state vector quality with negligible extra cost. |
| `SELENE_EMBEDDING_CORPUS=project_source_file_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_vector_text_batch/...q8_k4_r2_c4...dim1536_precbp10000_curbp10000_hitbp10000` | 318.20 µs | - | Vector-first BM25 is finally full-quality on this file-level corpus, but it is still slower than current-state vector scoring and much slower than BM25/current-state. |
| `SELENE_EMBEDDING_CORPUS=project_workspace_source_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c4...dim1536_precbp9375_curbp9375_hitbp10000` | 171.90 µs | - | Live workspace-source corpus with line-numbered source windows from the current checkout. Maintained BM25/current-state is fastest and target-complete, with one broad/current miss across 64 returned rows. |
| `SELENE_EMBEDDING_CORPUS=project_workspace_source_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_vector_batch/...q16_k4_r2_c4...dim1536_precbp9375_curbp9375_hitbp10000` | 3.2837 ms | - | Vector rerank after BM25/current-state preserves the same quality shape because the lexical candidate producer was already target-complete, but adds a large exact-vector pass. |
| `SELENE_EMBEDDING_CORPUS=project_workspace_source_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_expansion_batch/...q16_k4_r2_c6...dim1536_precbp10000_hitbp10000` | 304.22 µs | - | Plain graph-expanded vector scoring is target-complete and full topic precision, but has no maintained current-state gate. |
| `SELENE_EMBEDDING_CORPUS=project_workspace_source_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c4...dim1536_basecurbp10000_curbp10000_hitbp10000` | 301.24 µs | - | Maintained current-state vector scoring keeps full target/current precision, about 1.75x slower than BM25/current-state on this live-source profile. |
| `SELENE_EMBEDDING_CORPUS=project_workspace_source_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_provenance_state_intersection_batch/...q16_k4_r2_c4...dim1536_basecurbp10000_curbp10000_hitbp10000` | 302.39 µs | - | Provenance-required current state preserves the current-state vector quality with negligible extra cost. |
| `SELENE_EMBEDDING_CORPUS=project_workspace_source_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_vector_text_batch/...q16_k4_r2_c4...dim1536_precbp10000_curbp10000_hitbp10000` | 492.16 µs | - | Vector-first BM25 is full-quality on live source windows, but still slower than current-state vector scoring and much slower than BM25/current-state. |
| `SELENE_EMBEDDING_CORPUS=project_migration_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_score_batch/...q16_k4_r2_c4...dim1536_precbp9531_curbp9531_hitbp10000` | 170.21 µs | - | Legacy-alias migration profile with stale archived decoys. BM25/current-state is fastest and target-complete, but still has one broad/current miss across 64 returned rows. |
| `SELENE_EMBEDDING_CORPUS=project_migration_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_text_vector_batch/...q16_k4_r2_c4...dim1536_precbp9531_curbp9531_hitbp10000` | 3.1982 ms | - | Vector rerank after BM25/current-state preserves the same quality shape and adds a large exact-vector pass. |
| `SELENE_EMBEDDING_CORPUS=project_migration_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_expansion_batch/...q16_k4_r2_c8...dim1536_precbp10000_hitbp10000` | 307.72 µs | - | Plain graph-expanded vector scoring is target-complete and full broad-topic precision, but has no maintained current-state gate. |
| `SELENE_EMBEDDING_CORPUS=project_migration_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c4...dim1536_basecurbp8125_curbp10000_hitbp10000` | 295.29 µs | - | Maintained current-state vector scoring filters stale migration decoys, restores full current precision, and keeps all expected targets. |
| `SELENE_EMBEDDING_CORPUS=project_migration_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_provenance_state_intersection_batch/...q16_k4_r2_c4...dim1536_basecurbp8125_curbp10000_hitbp10000` | 295.72 µs | - | Provenance-required current state preserves the current-state vector quality with negligible extra cost. |
| `SELENE_EMBEDDING_CORPUS=project_migration_memory` `SELENE_EMBEDDING_PROVIDER=openrouter` `shared_cache_query_root_vector_text_batch/...q16_k4_r2_c4...dim1536_precbp10000_curbp8125_hitbp10000` | 493.91 µs | - | Vector-first BM25 is target-complete but admits stale/current-invalid hits, so it is slower and less current-precise than maintained current-state vector scoring. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_state_intersection/...q16_k4_r2_c14...precbp10000` | 1.55 ms | 1.62 ms | Same GQL-produced roots, then `selene.vector_score_candidate_state_expanded` intersects graph expansion with maintained `omlx_support_facts`, filtering root hint docs while preserving topic precision. |
| `procedure_vector_omlx_query_roots/shared_session_plan_cache_query_root_state_intersection/...q16_k4_r2_c14...precbp10000` | 1.56 ms | 1.61 ms | Warmed full-plan-cache support-state scorer; unchanged within local noise versus the fresh-session row. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_intersection/...q16_k4_r2_c13...basecurbp8593/8281_curbp10000` | 1.34 ms | 1.41 ms | Intersects the same expanded roots with maintained `omlx_current_support_facts`, excluding graph-authored negative evidence and restoring full current-fact precision with one fewer first-query candidate. |
| `procedure_vector_omlx_query_roots/shared_session_plan_cache_query_root_current_state_intersection/...q16_k4_r2_c13...basecurbp8593/8281_curbp10000` | 1.35 ms | 1.41 ms | Warmed full-plan-cache current-state scorer; still dominated by graph expansion plus vector rerank. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_provenance_state_intersection/...q16_k4_r2_c13...basecurbp8593/8281_curbp10000` | 1.67 ms | 1.64 ms | Intersects expanded roots with `omlx_provenance_current_support_facts`, requiring both incoming support and outgoing provenance edges while preserving the same candidate width and full current precision. |
| `procedure_vector_omlx_query_roots/shared_session_plan_cache_query_root_provenance_state_intersection/...q16_k4_r2_c13...basecurbp8593/8281_curbp10000` | 1.60 ms | 1.63 ms | Warmed full-plan-cache provenance-state scorer; positive edge-evidence checks add modest overhead versus exclusion-only current state on this quick local pass. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_expansion_batch/...q16_k4_r2_c16...precbp10000` | 312.49 µs | 528.84 µs | Single GQL statement builds all 16 query vectors and root sets from graph rows, then calls the batched expanded scorer once; avoids repeated statement/session overhead while preserving full topic precision. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_current_state_intersection_batch/...q16_k4_r2_c13...basecurbp8593/8281_curbp10000` | 284.24 µs | 491.37 µs | Single GQL statement builds all 16 query vectors/root sets, then calls the batched maintained-state expanded scorer once; restores full current precision while avoiding repeated session/procedure overhead. |
| `procedure_vector_omlx_query_roots/shared_cache_query_root_provenance_state_intersection_batch/...q16_k4_r2_c13...basecurbp8593/8281_curbp10000` | 294.60 µs | 496.62 µs | Batched provenance-gated current-state scorer; required support/provenance edge checks add little overhead once the 16-query shape is inside one procedure call. |

The opt-in `Qwen3-Embedding-8B-4bit-DWQ` local model also works on
`/v1/embeddings` and returns 4096-dimensional vectors. With
`SELENE_OMLX_EMBEDDING_MODELS=Qwen3-Embedding-8B-4bit-DWQ`, the cached
partial-hint expansion row reaches `precbp10000`, `c16`, at 205.26 us on the
same scaled profile, the maintained `omlx_support_facts` state row reaches
`precbp10000`, `c14`, at 184.50 us, and the ANN-root maintained-state row only
reaches `precbp5625`, `c42`, at 1.113 ms. The full GQL query-root expansion row
looks up anchors at 477.65 us repeated / 38.66 us hot-plan-cache / 40.96 us
batched, returns root rows at 568.42 us repeated / 54.44 us batched,
materializes roots at 708.65 us repeated / 52.94 us hot-plan-cache / 65.45 us
batched, reaches `precbp10000`, `r2`, `c16`, at 1.39 ms repeated and 1.38 ms
hot-plan-cache but only `basecurbp8437`, the GQL maintained-state intersection
row reaches `precbp10000`, `r2`, `c14`, at 1.47 ms repeated and 1.48 ms
hot-plan-cache, the negative-evidence current-state row reaches `curbp10000`,
`r2`, `c13`, at 1.47 ms repeated and 1.46 ms hot-plan-cache, and the GQL
batched text-score row reaches `precbp10000` / `curbp9218`, `r2`, `c16`, at
476.29 us. The GQL current-state BM25 expanded batch row reaches
`precbp10000` / `curbp10000`, `r2`, `c13`, at 185.55 us. Adding vector rerank
after that BM25 current-state candidate pass keeps the same quality but costs
8.6352 ms, so the GQL batched expansion row at `precbp10000`, `r2`, `c16`, and
660.94 µs remains a better vector path. On the target-aware
`code_alias_memory` profile, current-state BM25 batch reaches `hitbp8750` at
135.41 us, while adding vector rerank keeps the same `hitbp8750` and costs
1.5738 ms. The plain graph-expanded vector batch reaches `precbp10000` and
`hitbp8750`, `r2`, `c9`, at 336.08 us; the maintained vector current-state
batch row reaches `hitbp10000`, `basecurbp8125` / `curbp10000`, `r2`, `c6`, at
345.56 us, making maintained current-state composition the better
target-quality path for that code/alias profile. The vector
batched current-state row reaches `curbp10000`, `r2`, `c13`, at 719.86 µs, and
the batched provenance-state row reaches the same
`curbp10000` / `c13` shape at 707.24 µs. The conservative cached r60/w40 mixed
cycle is 12.52 ms on the same 4096-dimensional row. It stays opt-in for now so
default local oMLX rows remain short and comparable to the earlier two-model
baseline.

OpenRouter `mistralai/codestral-embed-2505` is available through
`SELENE_EMBEDDING_PROVIDER=openrouter` and returns 1536-dimensional vectors. On
the same target-aware `code_alias_memory` profile, the plain graph-expanded
batch row reaches `precbp10000` / `hitbp8750`, `r2`, `c9`, at 211.05 us, while
the maintained current-state vector batch reaches `hitbp10000`,
`basecurbp7812` / `curbp10000`, `r2`, `c6`, at 211.95 us. Provenance-gated
current-state scoring keeps the same target quality at 212.38 us. Maintained
BM25 over the same graph-expanded current set is faster at 136.79 us but still
misses one expected code/alias target, and vector rerank after that BM25 pass
does not improve target hits while costing 706.33 us. The reverse order,
vector-expanded top-k followed by BM25 over explicit candidates, reaches only
`precbp6562` / `curbp5312` / `hitbp8750` at 327.96 us. This keeps the
current-state conclusion intact for the code-specialized embedding model:
state composition, not model choice alone, recovers the missing target; BM25 is
the fast lexical path only when its candidate producer is target-complete, and
vector-first BM25 pruning is not a win on this corpus.

On the wider `code_alias_wide_memory` profile, target-hit splits further:
current-state BM25 and current-state vector rows find 14 of 16 targets
(`hitbp8750`), while plain vector expansion and vector-first BM25 find 15 of 16
(`hitbp9375`). That extra target comes with a currentness/precision tradeoff:
vector-first BM25 falls to `precbp7031` / `curbp5781` and costs 522.62 us, so
the better follow-up is richer graph state/corpus analysis rather than a fused
procedure.

On the `project_code_memory` profile, all measured Codestral paths find all 16
expected source-shaped targets. BM25/current-state is the fastest path
(`precbp9531` / `curbp9531` / `hitbp10000`, 170.50 us), while current-state
vector scoring restores full broad/current precision at 306.32 us. Vector-first
BM25 keeps `hitbp10000`, but drops to `precbp8750` / `curbp8750` and costs
486.06 us, so it remains a negative default-fusion result even on a corpus where
vector roots are target-complete.

On the harder `project_code_alias_memory` profile, lexical decoys create the
target-quality gap the positive control lacked. BM25/current-state is still
fastest but finds 15 of 16 targets (`hitbp9375`, 170.28 us), and vector rerank
after those BM25 candidates cannot recover the missing target. Current-state
vector scoring reaches `hitbp10000` and `curbp10000` at 314.32 us, making it
the quality path for this source-alias fixture. Vector-first BM25 stays
negative: `hitbp8750`, `curbp8593`, and 495.39 us.

On the `project_source_code_memory` profile, short real source excerpts make
BM25/current-state target-complete again (`hitbp10000`, 195.52 us), so this
acts as a source-file positive control rather than another lexical-failure
case. Maintained current-state vector scoring is also target-complete and
restores `curbp10000` at 328.21 us, while plain graph-expanded vector and
vector-first BM25 both miss one target (`hitbp9375`). The conclusion remains
compositional: use maintained graph state plus the scoring primitive that wins
the corpus, and do not add a fused vector-first BM25 API from these rows.

On the `project_source_chunk_memory` profile, target-aware implementation
snippets keep all measured paths target-complete after the graph-root layout is
kept target-free. BM25/current-state is still fastest (`hitbp10000`,
193.15 us) but has one broad/current miss. Current-state vector scoring
restores `curbp10000` at 328.45 us. Vector-first BM25 is also target-complete
and improves broad precision (`precbp9843`) over BM25/current-state, but it
costs 523.56 us and remains below current-state vector on current precision.
This is useful A/B evidence for chunked code retrieval, not a reason to add a
fused vector-first text API.

On the `project_source_file_memory` profile, selected real project files are
embedded as whole file-level documents. BM25/current-state remains the fastest
target-complete path (`hitbp10000`, 142.50 us) with one broad/current miss.
Current-state vector scoring restores full broad/current precision at
212.75 us. Vector-first BM25 also reaches full target/current precision here
(`hitbp10000`, 318.20 us), which makes this the first positive quality row for
that reverse order, but it is still slower than current-state vector scoring
and much slower than BM25/current-state. Keep it as evidence for corpus-shaped
A/B testing, not production API promotion.

On the `project_migration_memory` profile, queries use archived/prototype
terminology while current target documents cite first-class engine surfaces and
stale decoys model old-code drift. BM25/current-state is still fastest and
target-complete (`hitbp10000`, 170.21 us), but has one broad/current miss.
Current-state vector scoring restores full current precision at 295.29 us, and
vector-first BM25 is target-complete but current-imprecise
(`curbp8125`, 493.91 us). This reinforces graph-maintained current state as the
right correction for old-code drift; vector-first text remains a benchmarked
composition choice, not a default API.

The locally listed `jina-code-embeddings-1.5b-mlx` model is not currently
available from `/v1/embeddings` in oMLX; the endpoint returns HTTP 400 and
reports it as an LLM model for `/v1/chat/completions`, so it is not part of
these rows until it is exposed as an embedding model.

Component-pressure rows pool the query component with additional graph
components before exact vector scoring. Quality remains perfect on this clean
fixture, so these rows isolate candidate-set size pressure rather than topology
noise:

| Component pool | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_component_pressure/component_pool_w1/...c16_covbp10000_curbp10000_precbp10000` | 53.0 µs | 116.3 µs | Scores only the query anchor's compact component; this is the same primitive direction as WCC component filtering. |
| `graph_vector_component_pressure/component_pool_w4/...c64_covbp10000_curbp10000_precbp10000` | 183.4 µs | 466.8 µs | Four pooled components still beat broad graph expansion at 10k while keeping oracle quality. |
| `graph_vector_component_pressure/component_pool_w16/...c256_covbp10000_curbp10000_precbp10000` | 685.6 µs | 1.550 ms | Candidate scoring remains linear and predictable, but it is now in the same range as wide graph expansion. |
| `graph_vector_component_pressure/component_pool_w64/...covbp10000_curbp10000_precbp10000` | 2.689 ms (`w62`, `c992`) | 5.691 ms (`c976`) | Near-global pooled scoring is still exact and high quality, but too expensive for the default graph-filtered path; this is the fallback point for ANN or compressed pre-scoring research. |

Topology-pressure rows add cross-topic `SUPPORTS` noise before WCC projection,
then compare the broadened WCC component against a hard topic/session-style
candidate filter. Quality remains perfect because exact vector scoring can
still recover the right topic, so this isolates topology noise as candidate-set
inflation:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_topology_pressure/noisy_wcc/...covbp10000_curbp10000_precbp10000` | 2.535 ms (`c992`) | 56.68 ms (`c9728`) | Cross-topic topology noise collapses WCC into a near-global candidate set; exact scoring preserves quality but is far too expensive. |
| `graph_vector_topology_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 102.7 µs (`c32`) | 1.109 ms (`c152`) | A hard topic/session candidate set restores bounded exact scoring under the same noisy graph, pointing toward query-derived subgraph filters before ANN/PQ fallback. |

Community-pressure rows compare algorithm-derived community partitions against
the same noisy topology. These rows are benchmark-only and intentionally use the
same exact vector scorer/fact-diverse selector, so they isolate partition
quality rather than rerank behavior:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_community_pressure/noisy_wcc/...covbp10000_curbp10000_precbp10000` | 2.534 ms (`c992`) | 56.11 ms (`c9728`) | Same broad WCC baseline as topology pressure; quality is perfect only because exact scoring scans nearly the whole noisy graph. |
| `graph_vector_community_pressure/louvain/...` | 7.08 µs (`c2`, `covbp2500`) | 15.59 µs (`c2`, `covbp1250`) | Single-pass Louvain over this star-like memory topology over-partitions to tiny candidate sets. It is fast, but loses too much coverage to be a useful default partition source here. |
| `graph_vector_community_pressure/label_propagation/...` | 53.34 µs (`c17`, `covbp7661`, `precbp8830`) | 112.0 µs (`c16`, `covbp7578`, `precbp8789`) | Label propagation is a useful middle row: compact and fast, but partial-recall compared with the hard topic/session filter. |
| `graph_vector_community_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 102.6 µs (`c32`) | 1.094 ms (`c152`) | The hard topic/session filter remains the full-quality reference under this noisy graph. Next research should derive comparable filters from graph/query structure rather than trusting connectivity alone. |

Query-filter rows replace the metadata-only hard topic candidate set with graph
candidate production. Each memory node links to a scope node via `IN_SCOPE`;
the query path follows the anchor's scope edge, scans incoming scope membership
edges, then exact-scores those graph-derived candidates:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_query_filter_pressure/noisy_wcc/...covbp10000_curbp10000_precbp10000` | 2.558 ms (`c992`) | 56.61 ms (`c9728`) | Repeats the noisy WCC baseline inside the query-filter fixture. |
| `graph_vector_query_filter_pressure/label_propagation/...` | 54.86 µs (`c17`, `covbp7661`, `precbp8830`) | 114.3 µs (`c16`, `covbp7578`, `precbp8789`) | Label propagation stays compact and partial-recall when compared with graph-derived scope filtering. |
| `graph_vector_query_filter_pressure/graph_scope_filter/...covbp10000_curbp10000_precbp10000` | 111.2 µs (`c32`) | 1.142 ms (`c152`) | Graph-derived scope membership matches hard-topic quality with a small traversal overhead. This is the strongest product-shaped primitive so far: graph/query candidate production plus exact vector scoring. |
| `graph_vector_query_filter_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 104.3 µs (`c32`) | 1.102 ms (`c152`) | Metadata hard-topic filtering remains the lower-bound reference for the same candidate set. |
| `graph_vector_query_filter_pressure/graph_scope_candidate_set_batch_score/...covbp10000_curbp10000_precbp10000` | 90.47 µs (`c32`) | 498.1 µs (`c152`) | Canonical `VectorCandidateSet` batch scoring over graph-query output now uses query-level batch parallelism once the batch crosses the distributed-work threshold. |
| `graph_vector_query_filter_pressure/graph_scope_unresolved_current_algebra_batch_score/...covbp10000_curbp10000_precbp10000` | 80.92 µs (`c18`) | 840.4 µs (`c89`) | Composing graph-scope output with the maintained unresolved-current candidate set cuts exact scoring work while preserving full quality. |

The candidate-set rows show the boundary tradeoff: canonical graph-query output
is not faster by itself, but it becomes valuable once it composes with another
maintained graph-derived set. That supports `VectorCandidateSet` as a Rust-side
graph/query/active-set glue primitive before adding narrower procedure surfaces.

Session-filter rows add a coarser graph-derived membership edge. Each memory
node links to an `IN_SESSION` node shared by four topics, modeling task/session
scope that is broader than an exact topic but much narrower than noisy
connectivity. These rows keep the same noisy topology and exact vector scorer.
The `*_current_filter` variants prune stale fixture metadata before scoring,
mirroring a freshness-aware graph candidate producer instead of relying on
post-score current-result selection. The `*_unsuperseded_filter` variants derive
the same current candidate set from graph topology by rejecting nodes with an
outgoing `SUPERSEDED_BY` edge before scoring. The
`*_materialized_current_filter` variants use the same graph-derived current set
materialized once during fixture setup. The `*_provenance_expand` variants then
score only graph-current provenance roots with outgoing `SUPPORTS` edges and use
graph expansion plus `SUPERSEDED_BY` repair to recover current supporting facts.
The `*_k1` variants expand only the nearest provenance root, while the
non-suffixed provenance rows expand four roots:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_session_filter_pressure/noisy_wcc/...covbp10000_curbp10000_precbp10000` | 2.818 ms (`c992`) | 60.41 ms (`c9728`) | Repeats the noisy WCC baseline with session edges present outside the WCC projection. |
| `graph_vector_session_filter_pressure/label_propagation/...` | 60.11 µs (`c17`, `covbp7661`, `precbp8830`) | 123.8 µs (`c16`, `covbp7578`, `precbp8789`) | Label propagation remains fast but partial-recall against the full-quality graph membership filters. |
| `graph_vector_session_filter_pressure/graph_session_filter/...covbp10000_curbp10000_precbp10000` | 381.2 µs (`c124`) | 3.781 ms (`c608`) | Four-topic session membership is a useful middle row: much cheaper than noisy WCC and full-quality, but about 3x the exact scope filter at 10k. |
| `graph_vector_session_filter_pressure/graph_session_current_filter/...covbp10000_curbp10000_precbp10000` | 243.3 µs (`c70`) | 2.588 ms (`c356`) | Pre-score metadata freshness pruning keeps full quality while cutting session candidate pressure by roughly half. |
| `graph_vector_session_filter_pressure/graph_session_unsuperseded_filter/...covbp10000_curbp10000_precbp10000` | 291.6 µs (`c70`) | 3.154 ms (`c356`) | Graph-derived freshness reaches the same candidate count and quality as metadata current filtering, with extra edge-check overhead. |
| `graph_vector_session_filter_pressure/graph_session_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 245.6 µs (`c70`) | 2.615 ms (`c356`) | Materialized graph-derived currentness recovers nearly all metadata-current latency while avoiding per-candidate edge scans. |
| `graph_vector_session_filter_pressure/graph_session_provenance_expand_k1/...covbp10000_curbp10000_precbp10000` | 132.3 µs (`c15`) | 1.313 ms (`c76`) | Expanding only the nearest session provenance root is enough for full grounded coverage on this fixture. |
| `graph_vector_session_filter_pressure/graph_session_provenance_expand/...covbp10000_curbp10000_precbp10000` | 151.3 µs (`c15`) | 1.347 ms (`c76`) | Four-root session provenance expansion preserves quality but adds graph expansion work over the k1 row. |
| `graph_vector_session_filter_pressure/graph_scope_filter/...covbp10000_curbp10000_precbp10000` | 120.9 µs (`c32`) | 1.236 ms (`c152`) | Exact graph scope remains the best product-shaped candidate filter when the query can identify a narrow subgraph. |
| `graph_vector_session_filter_pressure/graph_scope_current_filter/...covbp10000_curbp10000_precbp10000` | 79.50 µs (`c18`) | 824.6 µs (`c89`) | Freshness-aware scope filtering preserves full quality and lowers exact scoring work below the metadata topic baseline. |
| `graph_vector_session_filter_pressure/graph_scope_unsuperseded_filter/...covbp10000_curbp10000_precbp10000` | 92.23 µs (`c18`) | 989.6 µs (`c89`) | Graph-derived scope freshness is still faster than raw scope scoring while avoiding fixture-side current metadata. |
| `graph_vector_session_filter_pressure/graph_scope_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 80.08 µs (`c18`) | 839.1 µs (`c89`) | Materialized graph-derived scope currentness stays close to metadata current filtering and below the hard-topic lower-bound row. |
| `graph_vector_session_filter_pressure/graph_scope_provenance_expand_k1/...covbp10000_curbp10000_precbp10000` | 53.18 µs (`c4`) | 390.4 µs (`c19`) | One scope-local provenance root reaches full quality and is now the lowest full-quality graph-derived row. |
| `graph_vector_session_filter_pressure/graph_scope_provenance_expand/...covbp10000_curbp10000_precbp10000` | 71.85 µs (`c4`) | 429.6 µs (`c19`) | Four-root scope provenance expansion keeps full quality but is slower than k1 on this fixture. |
| `graph_vector_session_filter_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 114.0 µs (`c32`) | 1.190 ms (`c152`) | Metadata hard-topic filtering remains the lower-bound reference for the same narrow candidate set. |

Sparse-provenance rows use the same scope/session candidate producers, but each
summary provenance root supports only a partition of the topic facts. This
turns provenance root fanout into a measurable quality/latency knob instead of
letting a single nearest root cover the whole support set:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_sparse_provenance_pressure/graph_session_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 371.6 µs (`c113`) | 3.854 ms (`c552`) | Full-quality broad session-current baseline. |
| `graph_vector_sparse_provenance_pressure/graph_session_provenance_expand_k1/...covbp3750_curbp3750_precbp3750` | 146.4 µs (`c15`) | 1.452 ms (`c76`) | Fast, but one sparse provenance root covers only 3750 bp of the supporting facts. |
| `graph_vector_sparse_provenance_pressure/graph_session_provenance_expand/...` | 165.9 µs (`c15`) | 1.516 ms (`c76`, `covbp8144`) | Four roots recover full quality at 1k, but not at the larger session scale. |
| `graph_vector_sparse_provenance_pressure/graph_session_provenance_expand_k8/...` | 172.8 µs (`c15`) | 1.545 ms (`c76`, `covbp9453`) | Eight roots nearly close the 10k quality gap with a small latency increase. |
| `graph_vector_sparse_provenance_pressure/graph_session_provenance_expand_k16/...covbp10000_curbp10000_precbp10000` | 186.2 µs (`c15`) | 1.578 ms (`c76`) | Sixteen roots reach full quality while staying roughly 2.4x faster than materialized session-current scoring. |
| `graph_vector_sparse_provenance_pressure/graph_scope_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 116.7 µs (`c29`) | 1.243 ms (`c138`) | Full-quality graph-scope current baseline. |
| `graph_vector_sparse_provenance_pressure/graph_scope_provenance_expand_k1/...covbp3750_curbp3750_precbp3750` | 44.67 µs (`c4`) | 408.6 µs (`c19`) | The lowest-latency graph-derived row, but only partial support coverage. |
| `graph_vector_sparse_provenance_pressure/graph_scope_provenance_expand/...` | 63.41 µs (`c4`) | 452.9 µs (`c19`, `covbp8144`) | Four roots are enough at 1k, but still partial at 10k. |
| `graph_vector_sparse_provenance_pressure/graph_scope_provenance_expand_k8/...` | 63.49 µs (`c4`) | 489.6 µs (`c19`, `covbp9453`) | Eight roots improve 10k support coverage with little 1k cost. |
| `graph_vector_sparse_provenance_pressure/graph_scope_provenance_expand_k16/...covbp10000_curbp10000_precbp10000` | 64.26 µs (`c4`) | 532.6 µs (`c19`) | Sixteen roots reach full quality and stay faster than the hard-topic metadata reference. |
| `graph_vector_sparse_provenance_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 113.8 µs (`c32`) | 1.227 ms (`c152`) | Metadata hard-topic lower-bound reference for comparison. |

Noisy sparse-provenance rows add current wrong-topic `SUPPORTS` edges before the
same sparse correct support partitions. This tests whether a graph-expansion
policy can tolerate plausible cross-topic provenance noise:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_noisy_sparse_provenance_pressure/graph_session_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 374.2 µs (`c113`) | 3.735 ms (`c552`) | Full-quality broad session-current baseline under noisy support topology. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_session_provenance_expand_k1/...covbp3750_curbp3750_precbp3750` | 143.7 µs (`c15`) | 1.434 ms (`c76`) | One root is fast but now loses both coverage and precision because wrong-topic support fills the tail. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_session_provenance_expand/...` | 165.4 µs (`c15`, full) | 1.481 ms (`c76`, `covbp8144`, `precbp8906`) | Four roots recover full 1k quality but stay partial at 10k and admit noisy evidence. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_session_provenance_expand_k8/...` | 172.9 µs (`c15`, full) | 1.518 ms (`c76`, `covbp9453`, `precbp9648`) | Eight roots nearly close the 10k gap but still leave some cross-topic tail results. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_session_provenance_expand_k16/...covbp10000_curbp10000_precbp10000` | 191.3 µs (`c15`) | 1.569 ms (`c76`) | Sixteen roots restore full quality while remaining materially faster than materialized session-current scoring. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_scope_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 116.4 µs (`c29`) | 1.205 ms (`c138`) | Full-quality scope-current baseline. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_scope_provenance_expand_k1/...covbp3750_curbp3750_precbp3750` | 44.11 µs (`c4`) | 405.1 µs (`c19`) | Lowest latency, but same coverage/precision failure as the session k1 row. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_scope_provenance_expand/...` | 62.14 µs (`c4`, full) | 446.9 µs (`c19`, `covbp8144`, `precbp8906`) | Scope-local four-root expansion is faster than session expansion but has the same 10k quality loss. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_scope_provenance_expand_k8/...` | 62.49 µs (`c4`, full) | 486.3 µs (`c19`, `covbp9453`, `precbp9648`) | Eight roots improve noisy support coverage and precision, but do not fully close them at 10k. |
| `graph_vector_noisy_sparse_provenance_pressure/graph_scope_provenance_expand_k16/...covbp10000_curbp10000_precbp10000` | 61.98 µs (`c4`) | 533.4 µs (`c19`) | Sixteen roots reach full noisy-support quality and remain faster than the topic-filter reference. |
| `graph_vector_noisy_sparse_provenance_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 113.6 µs (`c32`) | 1.190 ms (`c152`) | Metadata hard-topic lower-bound reference for comparison. |

Multi-hop provenance rows route half of each summary root's support facts
through `MemoryBridge` nodes. The one-hop row intentionally misses those bridged
facts; the two-hop row follows one more `SUPPORTS` layer:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_multihop_provenance_pressure/graph_session_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 249.7 µs (`c70`) | 2.688 ms (`c356`) | Full-quality broad session-current baseline. |
| `graph_vector_multihop_provenance_pressure/graph_session_provenance_expand_k1/...covbp5000_curbp5000_precbp5000` | 128.9 µs (`c15`) | 1.390 ms (`c76`) | One-hop expansion reaches only the direct half of each root's support set. |
| `graph_vector_multihop_provenance_pressure/graph_session_provenance_expand_2hop_k1/...covbp10000_curbp10000_precbp10000` | 141.7 µs (`c15`) | 1.423 ms (`c76`) | Two-hop expansion restores full quality with a small extra traversal cost. |
| `graph_vector_multihop_provenance_pressure/graph_scope_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 80.29 µs (`c18`) | 853.0 µs (`c89`) | Full-quality scope-current baseline. |
| `graph_vector_multihop_provenance_pressure/graph_scope_provenance_expand_k1/...covbp5000_curbp5000_precbp5000` | 44.97 µs (`c4`) | 391.6 µs (`c19`) | Scope-local one-hop expansion has the same half-coverage failure. |
| `graph_vector_multihop_provenance_pressure/graph_scope_provenance_expand_2hop_k1/...covbp10000_curbp10000_precbp10000` | 58.43 µs (`c4`) | 424.9 µs (`c19`) | Two-hop scope expansion restores full quality and stays below both materialized-current and topic-filter references. |
| `graph_vector_multihop_provenance_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 113.5 µs (`c32`) | 1.218 ms (`c152`) | Metadata hard-topic lower-bound reference for comparison. |

Noisy multi-hop provenance rows add one current wrong-topic `SUPPORTS` edge
before the same bridged support pattern. This keeps the full correct support
set but tests whether bounded-depth expansion admits off-topic tail evidence:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_noisy_multihop_provenance_pressure/graph_session_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 248.4 µs (`c70`) | 2.677 ms (`c356`) | Full-quality broad session-current baseline under noisy bridged support. |
| `graph_vector_noisy_multihop_provenance_pressure/graph_session_provenance_expand_k1/...covbp5000_curbp5000_precbp5000` | 127.8 µs (`c15`) | 1.402 ms (`c76`) | One-hop expansion still reaches only the direct half; off-topic tail evidence does not improve coverage. |
| `graph_vector_noisy_multihop_provenance_pressure/graph_session_provenance_expand_2hop_k1/...covbp10000_curbp10000_precbp10000` | 141.7 µs (`c15`) | 1.433 ms (`c76`) | Two-hop expansion restores full quality before deferred off-topic support can enter the final result set. |
| `graph_vector_noisy_multihop_provenance_pressure/graph_scope_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 80.09 µs (`c18`) | 851.0 µs (`c89`) | Full-quality scope-current baseline. |
| `graph_vector_noisy_multihop_provenance_pressure/graph_scope_provenance_expand_k1/...covbp5000_curbp5000_precbp5000` | 45.76 µs (`c4`) | 400.2 µs (`c19`) | Scope-local one-hop expansion has the same half-coverage failure. |
| `graph_vector_noisy_multihop_provenance_pressure/graph_scope_provenance_expand_2hop_k1/...covbp10000_curbp10000_precbp10000` | 59.09 µs (`c4`) | 432.1 µs (`c19`) | Two-hop scope expansion keeps full quality and stays below both materialized-current and topic-filter references. |
| `graph_vector_noisy_multihop_provenance_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 113.6 µs (`c32`) | 1.209 ms (`c152`) | Metadata hard-topic lower-bound reference for comparison. |

Noisy sparse multi-hop provenance rows combine sparse correct support,
wrong-topic support inserted first, and bridged correct support facts. This is
the first fixture where provenance depth and root fanout matter together:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_session_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 373.6 µs (`c113`) | 3.828 ms (`c552`) | Full-quality broad session-current baseline. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_session_provenance_expand_k1/...covbp2500_curbp2500_precbp2500` | 145.2 µs (`c15`) | 1.475 ms (`c76`) | One-hop k1 sees only the direct part of one sparse root. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_session_provenance_expand_2hop_k1/...covbp3750_curbp3750_precbp3750` | 148.8 µs (`c15`) | 1.467 ms (`c76`) | Two-hop k1 recovers bridged facts for one root but still leaves sparse-root coverage on the floor. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_session_provenance_expand_2hop/...` | 175.4 µs (`c15`, full) | 1.521 ms (`c76`, `covbp8144`, `precbp8886`) | Four two-hop roots are enough at 1k but partial at 10k under sparse noisy support. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_session_provenance_expand_2hop_k8/...` | 193.8 µs (`c15`, full) | 1.583 ms (`c76`, `covbp9453`, `precbp9648`) | Eight two-hop roots nearly close the 10k gap. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_session_provenance_expand_k16/...` | 204.6 µs (`c15`, `covbp5927`, `precbp7500`) | 1.624 ms (`c76`, `covbp5000`, `precbp7500`) | Wide one-hop fanout improves precision but cannot see bridged correct support. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_session_provenance_expand_2hop_k16/...covbp10000_curbp10000_precbp10000` | 228.7 µs (`c15`) | 1.669 ms (`c76`) | Wide two-hop expansion restores full session quality, still below materialized-current scoring. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_scope_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 116.6 µs (`c29`) | 1.242 ms (`c138`) | Full-quality scope-current baseline. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_scope_provenance_expand_k1/...covbp2500_curbp2500_precbp2500` | 44.09 µs (`c4`) | 411.0 µs (`c19`) | Lowest latency but only the direct part of one sparse root. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_scope_provenance_expand_2hop_k1/...covbp3750_curbp3750_precbp3750` | 47.17 µs (`c4`) | 418.1 µs (`c19`) | Two-hop k1 restores bridged facts for one root. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_scope_provenance_expand_2hop/...` | 74.66 µs (`c4`, full) | 477.7 µs (`c19`, `covbp8144`, `precbp8886`) | Scope-local four-root expansion has the same 10k quality knee as the session row. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_scope_provenance_expand_2hop_k8/...` | 74.16 µs (`c4`, full) | 540.8 µs (`c19`, `covbp9453`, `precbp9648`) | Eight roots nearly close the 10k gap while staying below topic filtering. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_scope_provenance_expand_k16/...` | 60.06 µs (`c4`, `covbp5000`, `precbp7500`) | 571.0 µs (`c19`, `covbp5000`, `precbp7500`) | Wide one-hop fanout remains depth-limited. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/graph_scope_provenance_expand_2hop_k16/...covbp10000_curbp10000_precbp10000` | 74.23 µs (`c4`) | 615.2 µs (`c19`) | Wide two-hop scope expansion restores full quality and remains below materialized-current and topic-filter references. |
| `graph_vector_noisy_sparse_multihop_provenance_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 114.2 µs (`c32`) | 1.214 ms (`c152`) | Metadata hard-topic lower-bound reference for comparison. |

Active-subgraph composition rows reuse the noisy sparse multi-hop topology and
add `CONTRADICTS` edges to current duplicates. The new unresolved-provenance
rows intentionally intersect session/scope, unresolved-current active set,
provenance roots, two-hop expansion, and unresolved-current selection. That is
too strict on this fixture: it is fast, but loses recall because too few
unresolved roots remain:

| Bench | 9k/10k scale | Notes |
|---|---:|---|
| `graph_vector_active_subgraph_composition_pressure/graph_session_materialized_current_filter/9k_q64_c552_covbp10000_curbp10000_precbp10000` | 3.438 ms (quick) | Full-quality broad session-current baseline under noisy sparse multi-hop plus contradictions. |
| `graph_vector_active_subgraph_composition_pressure/graph_session_materialized_unresolved_current_filter/9k_q64_c228_covbp10000_curbp10000_precbp10000` | 1.626 ms (quick) | Active unresolved-current set keeps full quality and cuts session candidates from 552 to 228. |
| `graph_vector_active_subgraph_composition_pressure/graph_session_provenance_expand_2hop_k16/9k_q64_c76_covbp10000_curbp10000_precbp10000` | 1.663 ms (quick) | Full-quality provenance reference over current roots; similar latency to materialized unresolved-current on this topology. |
| `graph_vector_active_subgraph_composition_pressure/graph_session_unresolved_provenance_expand_2hop_k16/9k_q64_c4_covbp3750_curbp3750_precbp4042` | 662.2 µs (quick) | Strict unresolved-root provenance is a negative result: very small root set, fast, but only partial coverage/precision. |
| `graph_vector_active_subgraph_composition_pressure/graph_scope_materialized_current_filter/9k_q64_c138_covbp10000_curbp10000_precbp10000` | 1.110 ms (quick) | Scope-local current baseline. |
| `graph_vector_active_subgraph_composition_pressure/graph_scope_materialized_unresolved_current_filter/9k_q64_c57_covbp10000_curbp10000_precbp10000` | 497.3 µs (quick) | Scope-local active unresolved-current set keeps full quality and cuts candidates from 138 to 57. |
| `graph_vector_active_subgraph_composition_pressure/graph_scope_provenance_expand_2hop_k16/9k_q64_c19_covbp10000_curbp10000_precbp10000` | 611.4 µs (quick) | Full-quality scope provenance reference. |
| `graph_vector_active_subgraph_composition_pressure/graph_scope_unresolved_provenance_expand_2hop_k16/9k_q64_c1_covbp3750_curbp3750_precbp3750` | 179.4 µs (quick) | Strict scope unresolved-root provenance is also partial recall; active-set filtering needs a full-recall fallback. |
| `graph_vector_active_subgraph_composition_pressure/topic_filter/9k_q64_c152_covbp10000_curbp10000_precbp10000` | 1.095 ms (quick) | Metadata hard-topic reference. |

Active-subgraph fallback rows test that follow-up directly: run strict
unresolved-provenance first, then fill missing current facts from the
materialized unresolved-current active set. The fallback restores full quality,
but it is slower than scoring the maintained active set directly on this
fixture because it pays both the narrow provenance pass and the broad fallback
scoring pass:

| Bench | 9k/10k scale | Notes |
|---|---:|---|
| `graph_vector_active_subgraph_fallback_pressure/graph_session_materialized_unresolved_current_filter/9k_q64_c228_covbp10000_curbp10000_precbp10000` | 1.673 ms (quick) | Full-quality maintained active-set baseline. |
| `graph_vector_active_subgraph_fallback_pressure/graph_session_unresolved_provenance_expand_2hop_k16/9k_q64_c4_covbp3750_curbp3750_precbp4042` | 664.1 µs (quick) | Fast strict provenance pass, still partial recall. |
| `graph_vector_active_subgraph_fallback_pressure/graph_session_unresolved_provenance_fallback_2hop_k16/9k_q64_c232_covbp10000_curbp10000_precbp10000` | 2.443 ms (quick) | Full-quality fallback, but slower than scoring the maintained active set directly. |
| `graph_vector_active_subgraph_fallback_pressure/graph_scope_materialized_unresolved_current_filter/9k_q64_c57_covbp10000_curbp10000_precbp10000` | 523.6 µs (quick) | Scope-local maintained active-set baseline. |
| `graph_vector_active_subgraph_fallback_pressure/graph_scope_unresolved_provenance_expand_2hop_k16/9k_q64_c1_covbp3750_curbp3750_precbp3750` | 184.4 µs (quick) | Fast strict scope provenance, still partial recall. |
| `graph_vector_active_subgraph_fallback_pressure/graph_scope_unresolved_provenance_fallback_2hop_k16/9k_q64_c58_covbp10000_curbp10000_precbp10000` | 796.5 µs (quick) | Scope fallback restores full quality, but remains slower than the active-set baseline. |
| `graph_vector_active_subgraph_fallback_pressure/topic_filter/9k_q64_c152_covbp10000_curbp10000_precbp10000` | 1.131 ms (quick) | Metadata hard-topic reference. |

Active-hint rows keep the same noisy sparse multi-hop contradicted fixture, but
add graph-authored `RECENT_IN` windows and direct `DEPENDS_ON` edges. This
models a broad session query that can derive a narrower active subgraph from
task memory topology before exact vector scoring:

| Bench | 9k/10k scale | Notes |
|---|---:|---|
| `graph_vector_active_hint_pressure/graph_session_materialized_unresolved_current_filter/9k_q64_c228_covbp10000_curbp10000_precbp10000` | 1.619 ms (quick) | Broad session active-set baseline. |
| `graph_vector_active_hint_pressure/graph_session_recent_active_filter/9k_q64_c57_covbp10000_curbp10000_precbp10000` | 498.1 µs (quick) | Graph-authored recency window narrows the session to topic-sized active candidates while preserving full quality. |
| `graph_vector_active_hint_pressure/graph_session_dependency_active_filter/9k_q64_c8_covbp10000_curbp10000_precbp10000` | 73.25 µs (quick) | Direct dependency edges produce one unresolved current candidate per fact; strongest full-quality graph/vector row so far on this fixture. |
| `graph_vector_active_hint_pressure/graph_session_provenance_expand_2hop_k16/9k_q64_c76_covbp10000_curbp10000_precbp10000` | 1.798 ms (quick) | Full-quality provenance reference is slower than direct active hints here. |
| `graph_vector_active_hint_pressure/topic_filter/9k_q64_c152_covbp10000_curbp10000_precbp10000` | 1.099 ms (quick) | Metadata hard-topic reference. |

Batched active-hint candidate-scoring rows compare repeated scoring, generic
`score_vector_nodes_batch_checked`, canonical `VectorCandidateSet` batch
scoring, sorted candidate-set algebra against the maintained unresolved-current
set, and graph-neighbor batch scoring. This isolates the API-boundary impact
from the candidate producer:

| Bench | 9k/10k scale | Notes |
|---|---:|---|
| `graph_vector_active_hint_batch_pressure/graph_session_materialized_unresolved_current_filter_repeated_score/9k_q64_c228_covbp10000_curbp10000_precbp10000` | 1.382 ms (quick) | Existing repeated per-query candidate scoring over the broad active set. |
| `graph_vector_active_hint_batch_pressure/graph_session_materialized_unresolved_current_filter_batch_score/9k_q64_c228_covbp10000_curbp10000_precbp10000` | 723.9 µs (quick) | Generic explicit-node batch scoring now normalizes once and delegates to the query-level candidate-set batch scorer. |
| `graph_vector_active_hint_batch_pressure/graph_session_materialized_unresolved_current_filter_candidate_set_batch_score/9k_q64_c228_covbp10000_curbp10000_precbp10000` | 734.9 µs (quick) | Query-level candidate-set batch parallelism removes the serial 64-query loop for broad active-set scoring. |
| `graph_vector_active_hint_batch_pressure/graph_session_materialized_unresolved_current_filter_candidate_set_algebra_batch_score/9k_q64_c228_covbp10000_curbp10000_precbp10000` | 579.9 µs (quick) | Sorted intersection with the maintained unresolved-current set plus query-level batch scoring is the fastest broad active-hint row. |
| `graph_vector_active_hint_batch_pressure/graph_session_recent_active_filter_repeated_score/9k_q64_c57_covbp10000_curbp10000_precbp10000` | 432.1 µs (quick) | Existing repeated scoring over recency-window candidates. |
| `graph_vector_active_hint_batch_pressure/graph_session_recent_active_filter_batch_score/9k_q64_c57_covbp10000_curbp10000_precbp10000` | 431.3 µs (quick) | Generic batch scorer is effectively neutral for medium candidate sets and preserves full quality. |
| `graph_vector_active_hint_batch_pressure/graph_session_recent_active_filter_candidate_set_batch_score/9k_q64_c57_covbp10000_curbp10000_precbp10000` | 430.6 µs (quick) | This 64-query by 57-candidate row stays just below the 4,096 total-candidate batch threshold and remains neutral. |
| `graph_vector_active_hint_batch_pressure/graph_session_recent_active_filter_candidate_set_algebra_batch_score/9k_q64_c57_covbp10000_curbp10000_precbp10000` | 430.3 µs (quick) | Adaptive sorted intersection remains effectively neutral with the maintained recency candidate path at medium width. |
| `graph_vector_active_hint_batch_pressure/graph_session_dependency_active_filter_repeated_score/9k_q64_c8_covbp10000_curbp10000_precbp10000` | 62.16 µs (quick) | Existing repeated scoring over direct dependency candidates. |
| `graph_vector_active_hint_batch_pressure/graph_session_dependency_active_filter_batch_score/9k_q64_c8_covbp10000_curbp10000_precbp10000` | 62.87 µs (quick) | Generic batch scorer remains effectively neutral for tiny candidate sets. |
| `graph_vector_active_hint_batch_pressure/graph_session_dependency_active_filter_candidate_set_batch_score/9k_q64_c8_covbp10000_curbp10000_precbp10000` | 63.44 µs (quick) | Tiny dependency sets remain below the batch parallel threshold at 512 total candidates. |
| `graph_vector_active_hint_batch_pressure/graph_session_dependency_active_filter_candidate_set_algebra_batch_score/9k_q64_c8_covbp10000_curbp10000_precbp10000` | 62.90 µs (quick) | Adaptive intersection stays neutral with the tiny maintained dependency path. |
| `graph_vector_active_hint_batch_pressure/graph_session_dependency_active_filter_neighbor_score/9k_q64_c8_covbp10000_curbp10000_precbp10000` | 60.17 µs (quick) | Production neighbor scorer derives dependency candidates from the anchor's `DEPENDS_ON` edges and remains fastest for tiny active hints. |
| `graph_vector_active_hint_batch_pressure/graph_session_dependency_active_filter_neighbor_batch_score/9k_q64_c8_covbp10000_curbp10000_precbp10000` | 61.26 µs (quick) | Batched production neighbor scoring remains neutral for tiny dependency sets while preserving full quality. |

ANN rerank rows use the same active-hint fixture, but start from ANN/search-hit
output. They convert wide ANN hits into `VectorCandidateSet`s, optionally
compose them with graph-maintained unresolved-current or dependency candidates,
then exact-score the resulting sets in one batch:

| Bench | 9k/10k scale | Notes |
|---|---:|---|
| `graph_vector_ann_rerank_pressure/ann_wide_hit_set_batch_rerank/9k_q64_c16_covbp1562_curbp1562_precbp9765` | 621.0 µs (quick) | Wide ANN hits stay high precision but low coverage; exact rerank of the ANN hit set cannot recover facts missing from the approximate candidate producer. |
| `graph_vector_ann_rerank_pressure/ann_wide_active_intersection_batch_rerank/9k_q64_c1_covbp644_curbp644_precbp644` | 524.6 µs (quick) | Adaptive intersection trims the active-set composition cost, but recall still collapses because the ANN seed rarely contains the active fact nodes. |
| `graph_vector_ann_rerank_pressure/ann_wide_dependency_union_batch_rerank/9k_q64_c23_covbp10000_curbp10000_precbp10000` | 694.4 µs (quick) | Unioning ANN hits with direct dependency candidates restores full quality, but the ANN search dominates latency and is roughly 10x slower than the graph-only dependency row. |
| `graph_vector_ann_rerank_pressure/graph_dependency_candidate_set_batch/9k_q64_c8_covbp10000_curbp10000_precbp10000` | 70.07 µs (quick) | Direct graph dependency candidates remain the best shape for tiny active hints; adding ANN output is unnecessary when graph topology already supplies one candidate per fact. |

Broad graph-gate rows use the same active-hint fixture but deliberately start
from the expensive session-level graph candidate set. They compare direct
session scoring, direct unresolved-current session scoring, ANN-only hits,
ANN-intersection gates, and ANN-union fallbacks:

| Bench | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_ann_broad_graph_gate_pressure/graph_session_candidate_set_batch/...covbp10000_curbp10000_precbp10000` | 343.2 µs (`c124`) | 3.827 ms (`c608`) | Broad session exact-score reference; full quality but expensive at 10k. |
| `graph_vector_ann_broad_graph_gate_pressure/graph_session_unresolved_candidate_set_batch/...covbp10000_curbp10000_precbp10000` | 217.5 µs (`c74`) | 1.505 ms (`c228`) | Maintained unresolved-current graph state cuts exact scoring while preserving full quality. |
| `graph_vector_ann_broad_graph_gate_pressure/ann_wide_hit_set_batch_rerank/...` | 262.8 µs (`c16`, `covbp5000`) | 639.4 µs (`c16`, `covbp1562`) | ANN-only hits are cheaper at 10k but miss most graph-memory facts. |
| `graph_vector_ann_broad_graph_gate_pressure/ann_wide_session_intersection_batch_rerank/...` | 280.9 µs (`c16`, `covbp5000`) | 783.5 µs (`c15`, `covbp1562`) | Intersecting 16 ANN hits with the broad session set trims exact scoring but does not improve recall. |
| `graph_vector_ann_broad_graph_gate_pressure/ann_broad_session_intersection_batch_rerank/...` | 538.2 µs (`c34`, full quality) | 1.442 ms (`c62`, `covbp5019`) | A 64-hit ANN gate recovers 1k quality but only reaches half coverage at 10k, roughly tied with direct unresolved-current scoring. |
| `graph_vector_ann_broad_graph_gate_pressure/ann_wide_session_union_batch_rerank/...` | 555.9 µs (`c124`) | 4.006 ms (`c608`) | ANN+session union preserves quality but adds ANN overhead without reducing the broad graph set. |
| `graph_vector_ann_broad_graph_gate_pressure/ann_broad_session_union_batch_rerank/...` | 847.6 µs (`c154`) | 4.466 ms (`c609`) | Wider ANN union is strictly slower than direct session scoring on this fixture. |

Interpretation: broad graph candidates are expensive enough that a gate would
matter, but ANN hit sets still do not contain enough graph-memory facts. The
best native shape remains graph-maintained unresolved-current state or stronger
graph-derived active hints; ANN should not be promoted as a broad-session
candidate gate without a different candidate producer or a much stronger recall
profile.

ANN+partial-graph fallback rows use the query-filter topology where label
propagation is a compact but partial graph-derived candidate producer. They
compare ANN alone, label propagation alone, their candidate-set union, and the
full-quality graph-scope reference:

| Bench | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_ann_graph_fallback_pressure/graph_scope_candidate_set_batch/...covbp10000_curbp10000_precbp10000` | 110.4 µs (`c32`) | 1.116 ms (`c152`) | Full-quality graph-scope reference using canonical candidate-set batch scoring. |
| `graph_vector_ann_graph_fallback_pressure/label_propagation_candidate_set_batch/...` | 54.89 µs (`c17`, `covbp7661`, `precbp8830`) | 117.8 µs (`c16`, `covbp7578`, `precbp8789`) | Label propagation is fast and compact but partial recall. |
| `graph_vector_ann_graph_fallback_pressure/ann_wide_hit_set_batch_rerank/...` | 265.5 µs (`c16`, `covbp5000`, `precbp10000`) | 624.9 µs (`c16`, `covbp1562`, `precbp9765`) | ANN alone is high precision but too low coverage to repair missing graph facts. |
| `graph_vector_ann_graph_fallback_pressure/ann_wide_label_union_batch_rerank/...` | 303.1 µs (`c28`, `covbp9959`, `precbp10000`) | 730.1 µs (`c30`, `covbp7910`, `precbp9980`) | ANN+label union nearly closes 1k coverage but barely improves the 10k label row while adding substantial ANN latency. |

Interpretation: ANN can help a small partial graph partition at 1k, but this
synthetic 10k fixture is still candidate-producer limited. A cheap graph-scope
candidate set remains the full-quality path; ANN fallback needs a different
workload where graph candidates are broad enough to be expensive but not
already quality-complete.

Adaptive provenance rows use the noisy sparse multi-hop topology and a
benchmark-only quality oracle. The adaptive row scores provenance roots once,
then tries k1 one-hop, k1 two-hop, k4 two-hop, k8 two-hop, and k16 two-hop
until full quality is reached or the ladder is exhausted:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_adaptive_provenance_pressure/graph_session_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 376.3 µs (`c113`) | 4.529 ms (`c552`) | Full-quality broad session-current baseline on the adaptive run. |
| `graph_vector_adaptive_provenance_pressure/graph_session_provenance_expand_2hop_k16/...covbp10000_curbp10000_precbp10000` | 232.5 µs (`c15`) | 2.336 ms (`c76`) | Fixed wide two-hop session reference. |
| `graph_vector_adaptive_provenance_pressure/graph_session_provenance_adaptive_quality/...covbp10000_curbp10000_precbp10000` | 200.7 µs (`c15`) | 2.402 ms (`c76`) | Stops early at 1k, but at 10k pays the staged probes before reaching the same full-quality plan. |
| `graph_vector_adaptive_provenance_pressure/graph_scope_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 116.8 µs (`c29`) | 1.403 ms (`c138`) | Full-quality scope-current baseline on the adaptive run. |
| `graph_vector_adaptive_provenance_pressure/graph_scope_provenance_expand_2hop_k16/...covbp10000_curbp10000_precbp10000` | 77.61 µs (`c4`) | 802.3 µs (`c19`) | Fixed wide two-hop scope reference. |
| `graph_vector_adaptive_provenance_pressure/graph_scope_provenance_adaptive_quality/...covbp10000_curbp10000_precbp10000` | 98.00 µs (`c4`) | 875.9 µs (`c19`) | Oracle-style staged probing is slower than fixed k16 on the scope path. |
| `graph_vector_adaptive_provenance_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 114.8 µs (`c32`) | 1.195 ms (`c152`) | Metadata hard-topic lower-bound reference for comparison. |

Negative-evidence rows use a graph-authored `CONTRADICTS` edge to mark duplicate
current facts as resolved elsewhere, leaving one unresolved current node per
fact. This measures graph-side candidate pruning before exact vector scoring:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_negative_evidence_pressure/graph_session_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 228.2 µs (`c70`) | 2.424 ms (`c356`) | Baseline broad session-current scoring still sees contradicted duplicate current facts. |
| `graph_vector_negative_evidence_pressure/graph_session_unresolved_current_filter/...covbp10000_curbp10000_precbp10000` | 160.6 µs (`c31`) | 976.1 µs (`c32`) | Dynamic graph-derived unresolved-current pruning keeps full quality while cutting the 10k session row by ~2.5x. |
| `graph_vector_negative_evidence_pressure/graph_session_materialized_unresolved_current_filter/...covbp10000_curbp10000_precbp10000` | 122.0 µs (`c31`) | 556.6 µs (`c32`) | Maintaining the unresolved-current set keeps the same full-quality candidate shape while avoiding per-candidate edge scans. |
| `graph_vector_negative_evidence_pressure/graph_scope_materialized_current_filter/...covbp10000_curbp10000_precbp10000` | 74.29 µs (`c18`) | 775.4 µs (`c89`) | Topic-scope current baseline with contradicted duplicates still present. |
| `graph_vector_negative_evidence_pressure/graph_scope_unresolved_current_filter/...covbp10000_curbp10000_precbp10000` | 53.33 µs (`c8`) | 289.8 µs (`c8`) | Dynamic unresolved-current pruning keeps the strongest exact-scored graph candidate shape. |
| `graph_vector_negative_evidence_pressure/graph_scope_materialized_unresolved_current_filter/...covbp10000_curbp10000_precbp10000` | 42.94 µs (`c8`) | 173.9 µs (`c8`) | Materialized unresolved-current scope pruning is the fastest full-quality graph row in this matrix. |
| `graph_vector_negative_evidence_pressure/topic_filter/...covbp10000_curbp10000_precbp10000` | 104.7 µs (`c32`) | 1.105 ms (`c152`) | Metadata hard-topic reference remains slower than unresolved graph pruning at 10k. |

Active-set maintenance rows reuse the negative-evidence fixture and time a
60-read / 40-write cycle. The dynamic row pays graph-edge checks on every read
and no maintained-state work on writes. The materialized row pays maintained
set membership on reads and a balanced 20-remove / 20-insert active-set update
cycle on writes:

| Strategy | 1k requested / 992 actual | 10k requested / 9,728 actual | Notes |
|---|---:|---:|---|
| `graph_vector_active_set_maintenance_pressure/dynamic_edge_checks_r60w40/...covbp10000_curbp10000_precbp10000` | 9.622 ms (`c31`) | 58.35 ms (`c32`) | Baseline 60/40 cycle using dynamic contradiction-edge checks for unresolved-current reads. |
| `graph_vector_active_set_maintenance_pressure/materialized_set_r60w40/...covbp10000_curbp10000_precbp10000` | 6.904 ms (`c31`) | 26.41 ms (`c32`) | Maintained active set keeps full quality and remains faster even after 40 balanced set updates per cycle. |
| `graph_vector_active_set_maintenance_pressure/materialized_set_maintenance_w40/...` | 454.1 ns (`active248`) | 396.8 ns (`active512`) | Isolated 40-update HashSet maintenance is negligible next to exact vector rerank cost on this fixture. |

## Typed physical candidate-set lifecycle

Focused M04-PR02 Part 1 representation/algebra rows, measured 2026-09-01:

Command: `scripts/run-benches.sh --profile quick --bench single_graph --filter graph_physical_candidate_set`

| Bench | 1,024-element quick estimate | Notes |
|---|---:|---|
| `graph_physical_candidate_set/build_live_nodes/1024` | 4.3863 µs | Builds the generation/layout-bound node set from typed live rows, retains both private binding tokens, and sorts stable IDs. One high mild outlier was reported. |
| `graph_physical_candidate_set/iterate_stable_ids/1024` | 93.951 ns | Iterates deterministic stable `NodeId` values without exposing physical rows. One high mild outlier makes this directional local evidence rather than a regression threshold. |
| `graph_physical_candidate_set/union_full_overlap/1024` | 21.476 µs | Validates graph/generation/physical-layout/workspace-binding identity plus every stable-ID/typed-row/liveness pairing for both operands, then unions two fully overlapping sets. One high severe outlier was reported. |
| `graph_physical_candidate_set/intersection_full_overlap/1024` | 21.682 µs | Performs the same complete identity and trusted-entry validation before intersecting two fully overlapping sets. |
| `graph_physical_candidate_set/difference_full_overlap/1024` | 23.812 µs | Performs complete identity and trusted-entry validation before removing a fully overlapping set and producing empty output. Two high mild outliers were reported. |
| `graph_physical_candidate_set/clone_snapshot/1024` | 61.825 ns | Clones the immutable graph snapshot while retaining physical-layout ancestry and its current private workspace binding. |

Focused M04-PR02 Part 2 binding/recovery acceptance rows were measured on
2026-09-01 with the quick Criterion profile and mimalloc on an Apple M5 with
16 GiB RAM running macOS 26.7. These are bounded overhead observations, not
before/after improvement claims:

```text
scripts/run-benches.sh --profile quick --bench single_graph --filter graph_physical_candidate_set
scripts/run-benches.sh --profile quick --bench single_graph --filter graph_vector_candidate_set --vector-scales 1000
scripts/run-benches.sh --profile quick --bench text_search_bm25
scripts/run-benches.sh --profile quick --bench provider_fanout
```

| Bench | Quick estimate | Notes |
|---|---:|---|
| `graph_physical_candidate_set/bind_canonical_ids/1024` | 16.673 µs | Binds an already strictly ascending stable-ID input without another sort/dedup pass. |
| `graph_physical_candidate_set/bind_noncanonical_duplicate_ids/1024` | 22.621 µs | Canonicalizes reversed duplicate stable IDs while filtering through the same liveness-only binder. |
| `graph_physical_candidate_set/bind_vector_candidate_set/1024` | 18.232 µs | Binds a canonical stable-ID `VectorCandidateSet` without vector-property or index checks. |
| `graph_vector_candidate_set/neighbor_candidates_depends_on_k64/1000` | 62.565 ns | Representative registered 1,000-node graph/vector row from the required bounded companion run. |
| `graph_text_bm25_exact/topic_query/n1000_k10` | 257.19 µs | Exact typed-candidate scan over 1,000 text nodes. |
| `graph_text_bm25_indexed/prebuilt_topic_query_candidates_sorted/n1000_k10` | 62.088 µs | Prebuilt text-index scoring over sorted stable-ID candidates. |
| `provider_fanout/core_only` | 178.70 µs | Representative core-only provider fan-out cycle. |
| `provider_fanout/extra_k4` | 198.04 µs | Representative cycle with four additional providers. |

The repository's registered `bfs` benchmark does not call
`reachable_nodes_checked`, and no registered reachability row currently
measures the changed stable-root binding path, so a BFS timing is not treated as
relevant Part 2 evidence.

## Retrieval scoping guards

Focused local P0 row, measured 2026-06-15:

Command: `scripts/run-benches.sh --profile quick --bench single_graph --filter graph_ann_property_filter --vector-scales 1000`

| Bench | 1k quick median | Notes |
|---|---:|---|
| `graph_ann_property_filter/hnsw_cosine_namespace_sparse_d128_k10_ef64_rows4_...` | 6.0873 µs | High-cardinality `namespace` string index admits 4 of 1,000 rows before HNSW result admission; traversal stays bounded by the ANN search width instead of scanning the sparse property bucket. |

## Cluster-B regression targets

This doc is the baseline for the v1.2 cluster-B performance-uplift work
(graph node 767). Each optimization has a dedicated bench whose median should
move when it lands; refresh that row + diff against the `northstar` baseline to
confirm the win and guard the surrounding rows against regression.

| Target | Optimization | Watch bench | Current baseline |
|---|---|---|---|
| CORE-06 ✓ | Box `Value` `Path` + time variants (shrink `size_of`) | `core_value_clone/*` + `size_of::<Value>` stderr | **32 B** (was 128); vec 4.62 µs / pmap 53.8 ns |
| GRAPH-05 ✓ | In-place adjacency delete O(D²)→O(D) | `graph_hub_delete` (now linear) | **4.54 ms** @ degree 10k (was 133 ms — 30×) |
| PERSIST-04 rejected | WAL vectored write regressed append on Darwin/macOS | `persist_wal_body_size_no_fsync` (large-body arms) | measured-rejected 2026-06-01; keep contiguous `Vec` + `write_all` |
| ALGO-01/02/05 ✓ | CSR dense-`u32` cache on `ProjNeighbor` | `algo/projection_build` + `…_neighbor_iter` + algo medians | **pagerank −15..31% · louvain −23..26% · apsp −9..52% · triangle −6..11% · iter −4..6%**; build +4–7% one-time (24→32 B/neighbor) |
| GQLRT-05 ✓ | Memoize correlated-subquery target schema (per statement, by expr id) | `gql_correlated_subquery/{exists,count}` | **−2 to −7%** — memo elides the per-row `schema_for_pattern` walk |
| B3 ✓ | Short-circuit scans already bound by the correlated outer row | `gql_correlated_subquery/{exists,count}` + `read_pipeline` guard | **~339x EXISTS / ~349x COUNT @10k**; ordinary read-pipeline rows remain noise-scale |
| B5 ✓ | Use `FxBuildHasher` for immutable maps keyed only by engine-assigned ids | `graph_node_fetch` + `gql_correlated_subquery/{exists,count}` + `bulk_mutation` guard | **graph_node_fetch −22.7% @1k quick; post-B3 correlated residual −11.8..15.3%**; update-batch writes remain noisy/no claimed win |
| B18/B20 ✓ | Hoist runtime column resolution and borrow aggregate descriptors | `read_pipeline` + `gql_correlated_subquery/{exists,count}` + `write_e2e` guard | **read_pipeline −3.8..11.7% on significant rows; correlated residual −4.7..5.9%**; mixed write guards neutral, isolated WAL spike not reproduced |
| D10 (guard) | Lock-free reads stay flat under writes | `graph_read_under_write` | 24.5 ms @100k |
| D14 (guard) | Snapshot rkyv encode/positional recovery | `graph_snapshot_roundtrip/{encode,decode}` | enc 32 ms / dec 183 ms @100k |

## Update protocol

1. From a clean, synced `development` on a **quiet machine** (background load
   pollutes medians): `git checkout development && git pull --ff-only`.
2. Run the full sweep, saving the baseline:
   `scripts/run-benches.sh --profile full --save-baseline northstar`.
3. Refresh the header: `_Last measured_` date + hardware footprint (capture
   commands above) + `git rev-parse --short HEAD`.
4. Fill every `Median` / `Sequential` / `Auto` cell from criterion stdout.
5. Commit: `chore(bench): refresh BENCHMARKS.md (<short hardware string>)`.

## Out of scope

- CI bench jobs — benchmarks are local-only and sequential (CI only lints the
  invocation hygiene; it never executes benches).
- Cross-host comparison automation.
- Donor regression-target comparison (lives in gitignored
  `_design/perf-baselines.md`).
