# Selene DB 2.0 program

This directory is the tracked program-of-record for Selene DB 2.0. The delivery
program is defined by the 2.0 Finish Plan, containing six finish milestones
(F01–F06), 35 PR-sized work items (PLAN-01 adoption plus 34 finish PRs), 22
finalized decisions, and one owner for each of the seven open issues recorded at
installation.

## Read in this order

1. [Start here](roadmap/00-START-HERE.md) and [current progress](roadmap/01-CURRENT-PROGRESS.md)
2. [Master roadmap and integration gates](roadmap/02-MASTER-ROADMAP.md)
3. [Finalized decisions](decisions/finalized.md)
4. [PLAN-01: adoption](roadmap/PLAN-01.md), then the assigned milestone and PR
5. [Review protocol](review-protocol.md)
6. [Operating guide](operating-guide.md)
7. [Semantic/durability notes](roadmap/04-GQL-AND-DURABILITY-NOTES.md) and [validation](roadmap/05-VALIDATION-AND-RELEASE.md) as needed

The [machine plan](roadmap/plan.json) supplies scheduling and navigation; the
PR Markdown supplies behavior and acceptance. The offline
[finish review view](roadmap/selene-db-v2-finish-review.html) is a read-only
companion. Historical plan registers ([milestones](roadmap/milestones.md),
[work items M00–M04](roadmap/work-items-00-04.md),
[work items M05–M10](roadmap/work-items-05-10.md), and
[work-item contract](roadmap/work-item-contract.md)) are retained for
historical reference.

## Program milestones (F01–F06)

- **[Milestone F01](roadmap/Milestone-F01.md) — Finish graph identity and mixed topology:**
  complete candidate set migration, remove downstream public-row APIs, and finalize mixed edge semantics.
- **[Milestone F02](roadmap/Milestone-F02.md) — Bring durable embedding forward:**
  anchor directory capability, format-2 persistence, unified catalog metadata, and transaction commit protocol.
- **[Milestone F03](roadmap/Milestone-F03.md) — Complete the semantic compiler:**
  AST/semantic tree separation, catalog-backed type resolution, and logical IR.
- **[Milestone F04](roadmap/Milestone-F04.md) — Deliver batch execution and native retrieval:**
  pull-based batch engine, native vector/BM25/JSON retrieval operators, and deletion of legacy row executor.
- **[Milestone F05](roadmap/Milestone-F05.md) — Finish paths, indexes and measured performance:**
  automata path engine, composite/key constraints, JSON scalar expression indexes, and balanced performance gates.
- **[Milestone F06](roadmap/Milestone-F06.md) — Qualify and release 2.0:**
  truthful conformance claims, packaging, docs, and owner-authorized release.

## Authority and current state

The architecture review used source snapshot
`b8782bec34ff0b815b62711ac7e33cac09d8ea71`. The historical kickoff used
installation base `c5c0a9855f5c043ecc927d561e4ad8ba001346d9`. Keep those
coordinates distinct when citing evidence.

The foundation crates (`selene-profile`, `selene-catalog`, `selene-db` facade,
sessions, and transaction authority) were established in merged milestones M00–M04.
The finish plan builds directly on these landed foundations rather than repeating
kickoff work.

The archive branch and tag named by D-002 are **pending owner-only** actions.
They were absent when this package was installed. Verify both refs at the
reviewed snapshot before relying on them.

Untracked local material may support research, but future work must be
executable from tracked source, this package, current tests, and read-only
repository or issue evidence.

## Policy and evidence documents

- [Coherent checkpoint and facade reopen](checkpoint-reopen.md)
- [Checkpoint rotation, artifact leases and explicit retention](rotation-retention.md)

- [2.0 line and 1.x end-of-life policy](eol-and-version-policy.md)
- [Conformance and claim policy](conformance-policy.md)
- [Source snapshot and assumptions](source-snapshot-and-assumptions.md)
- [Executable final-1.x baseline](baseline/README.md)
- [Risk register](risk-register.md)
- [Issue ownership](issue-ownership.md)
- [Review protocol](review-protocol.md)
- [Operating guide](operating-guide.md)
- [Old-to-new mapping](roadmap/06-OLD-TO-NEW-MAP.md)
- [Decisions and deferred work](roadmap/07-DECISIONS-AND-DEFERRED-WORK.md)
- [Post-GA backlog](post-ga-backlog.md)

From the repository root:

```bash
python3 -B .github/scripts/check-v2-plan.py --root .
```
