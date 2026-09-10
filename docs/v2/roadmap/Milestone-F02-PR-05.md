---
plan_id: F02-PR05
milestone: F02
initial_status: proposed
---

# F02-PR05 — Checkpoint a coherent database and reopen the first durable slice

**Milestone:** [F02: Bring durable embedding forward](Milestone-F02.md)
**Dependencies:** [F02-PR04](Milestone-F02-PR-04.md)
**Carries forward:** M09-PR05, M09-PR07; see the [complete crosswalk](06-OLD-TO-NEW-MAP.md).
**Issue closure:** None; do not close another PR’s issue.
**Focused skills:** `rust-storage-durability`; `rust-test-design`; `rust-api-design` from `yet-more-skills/skills/<name>/SKILL.md`.

## Outcome

Create a complete format-2 checkpoint and a first facade create/open/reopen path that reconstructs catalog and graph state together.

## Start from what exists

The current DatabaseBuilder only builds in-memory databases. New fallible persistence APIs belong beside the actual database/configuration owners, not a presumed existing builder.rs. This first reopen slice precedes the comprehensive recovery campaign; it is not release certification. Source: S05.

**Observed live entry points:** `crates/selene-persist/src/snapshot_writer.rs`, `crates/selene-persist/src/snapshot_reader.rs`, `crates/selene-persist/src/snapshot_file_header.rs`, `crates/selene-db/src/database.rs`, `crates/selene-db/src/config.rs`, `crates/selene-db/src/catalog_snapshot.rs`

Paths are navigation, not a closed edit inventory. New modules are implementation choices. Keep required callers and their tests with the behavior they support. Source IDs resolve in [SOURCE-NOTES.md](SOURCE-NOTES.md).

## Implementation sequence

1. Pin one immutable outer database state and its durable transaction boundary. Serialize its catalog, graph types, all named graphs, logical values, mixed edges, active declaration metadata and required high-water marks from that same view.
2. Write a new immutable snapshot through StoreDirectory, validate section lengths/integrity, synchronize it, then publish its descriptor through the control protocol. Do not silently substitute a later graph snapshot midway through the checkpoint.
3. Add fallible facade creation/opening without changing the infallible in-memory builder merely for cosmetic consistency. Reuse the ownership root and session semantics; never return an apparently open database while background authoritative recovery is unfinished.
4. Load the selected snapshot into isolated state, replay its WAL suffix and publish once after validation. Rebuild derived indexes needed by currently active constraints before admitting writes.
5. Add a consumer-shaped restart fixture using named schemas/graphs and selected properties, plus explicit transaction commit and rollback. Document exactly which recovery cases are completed here and which F02-PR07 still owns.

F02-PR01 supplies only persistence-layer empty control (`StoreDirectory`, durable
StoreId/epoch, CURRENT and immutable manifests). It deliberately leaves the
production facade in-memory. This PR owns the first real durable facade
create/open path and must not reuse an ephemeral Database as a persistence
preview. Reopen preserves the store's durable identity while allocating a fresh
process-local DatabaseId and binding newly issued handles to that live instance.

## Acceptance and concrete regression cases

- [ ] Create multiple graphs, commit data, checkpoint, append more transactions and reopen with identical catalog, graph contents and diagnostic behavior.
- [ ] A checkpoint concurrent with a writer represents one valid publication boundary, not a mixture of old catalog and new graph data.
- [ ] Rolled-back writes remain absent after reopen and deleted published IDs are not accidentally reissued.
- [ ] Mixed edges, selected value types and graph-type restrictions survive restart with fresh process-local handle validation.
- [ ] Durable facade reopen preserves StoreId/epoch but rejects old or foreign process-local handles, without changing infallible in-memory construction.
- [ ] An incomplete staged snapshot is never selected as authoritative; corrupt required sections fail rather than becoming empty graphs.
- [ ] Required derived constraint state is complete before writes; optional accelerator rebuild status is explicit.

## Validation and performance

Run snapshot codecs, facade restart and transaction visibility tests over real temporary directories. Independently inspect reconstructed semantic content rather than comparing only serialized bytes. Use current row execution as a temporary consumer of the reopened graph until batch cutover.

Measure checkpoint bytes/time, first open, WAL-suffix replay and peak retained memory under a held reader. Record the cost of all named graphs, not just a single empty graph.

Use the shared [validation guide](05-VALIDATION-AND-RELEASE.md) for runner mechanics and required PR/RC gates. These are planned checks, not reported passes.

## Keep out of this PR

No early production-ready claim, implicit background recovery, persistence of raw row layouts or snapshot selection by newest filename.

## Bridge/deletion boundary

The first new open path must not dispatch to format 1. F02-PR07 completes corruption/lineage coverage; F02-PR08 removes obsolete paths and publishes the durable preview contract.

## Standards and reviewer focus

§4.6 atomic recovery; §4.2.5 catalog ownership; runtime handle lifetime is a Selene API contract.

**Independent review question:** Does a checkpoint encode exactly the publication and durable boundary it claims?

Use [Luna execution guidance](03-LUNA-EXECUTION.md) for material decisions and the compact handoff. A necessary adjacent caller or mechanical migration is not itself a reason to stop; an incompatible semantic, public or durable contract is.
