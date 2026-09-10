# Open issue ownership

Each issue open at installation has one 2.0 implementation owner. Foundation
work may reference an issue but must not close it before the owning PR produces
direct acceptance evidence.

<a id="issue-1088"></a>
- [#1088](https://github.com/jscott3201/selene-db/issues/1088) → [F02-PR01](roadmap/Milestone-F02-PR-01.md): anchored persistence directory capability.
  The [current owner ruling](https://github.com/jscott3201/selene-db/issues/1088#issuecomment-5610433175)
  requires retained handle-relative prevention after real parent/root replacement,
  not detection followed by pathname I/O. Initial native support is Linux/macOS;
  dependency policy remains unchanged. Closure still requires the owning native
  lifecycle evidence, independent review and exact-head CI/merge verification.
<a id="issue-1092"></a>
- [#1092](https://github.com/jscott3201/selene-db/issues/1092) → [F05-PR05](roadmap/Milestone-F05-PR-05.md): named composite unique and key constraints.
<a id="issue-1093"></a>
- [#1093](https://github.com/jscott3201/selene-db/issues/1093) → [F01-PR02](roadmap/Milestone-F01-PR-02.md): typed generation-bound candidate sets and removal of row bitmap APIs.
<a id="issue-1094"></a>
- [#1094](https://github.com/jscott3201/selene-db/issues/1094) → [F05-PR05](roadmap/Milestone-F05-PR-05.md): incremental backing-index uniqueness enforcement.
<a id="issue-1097"></a>
- [#1097](https://github.com/jscott3201/selene-db/issues/1097) → [F05-PR06](roadmap/Milestone-F05-PR-06.md): JSON scalar-path expression indexes and pushdown.
<a id="issue-1128"></a>
- [#1128](https://github.com/jscott3201/selene-db/issues/1128) → [F02-PR04](roadmap/Milestone-F02-PR-04.md): WAL watermarks and precise commit outcomes.
<a id="issue-1137"></a>
- [#1137](https://github.com/jscott3201/selene-db/issues/1137) → [F05-PR07](roadmap/Milestone-F05-PR-07.md): read-hot map evidence and balanced performance gates.

Use `Closes #NNNN` only when the owner PR's tests or benchmarks demonstrate the
resolution. Moving ownership requires REPLAN and coordinated changes to the
issue map, machine plan, projections, and both affected contracts.
