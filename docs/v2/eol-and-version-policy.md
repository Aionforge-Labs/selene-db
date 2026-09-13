# 2.0 line and 1.x end-of-life policy

Selene development uses one package line: `2.0.0-alpha.1`. The 1.x line is
end of life. Its source remains available for inspection, but availability is
not maintenance or support.

The named archive branch and tag are **pending owner-only** actions. They were
absent when the tracked 2.0 program was installed. Verify both refs before
relying on them.

## Branch, tag, version, and support matrix

| Surface | Role | Policy |
|---|---|---|
| `development` | Active integration branch | Carries the 2.0 line. All Selene workspace packages and Selene path-version constraints move together. |
| `main` | Historical released branch | Records the released 1.x history until a later release PR updates it. It is not a 1.x maintenance branch. |
| `2.0.0-alpha.1` | Current source coordinate | Identifies the workspace source. It does not assert that the packages have been published to crates.io. |
| `v2.<minor>.<patch>[-prerelease][+build]` | Release tags | Eligible for release automation only when the tag is valid SemVer, has major version 2, and exactly matches the workspace version. |
| Existing `v1.*` tags | Historical release tags | Remain available as history. No new v1 tag or release is permitted. |
| `archive/1.x-final` | Pending owner-only source archive | The owner creates it at exact commit `b8782bec34ff0b815b62711ac7e33cac09d8ea71`. It is not a patch branch. |
| `archive-v1-eol-2026-08-21` | Pending owner-only non-release tag | The owner creates it at the same exact commit. Release automation must not run for it. |

The release workflow's coarse trigger is `v2.*.*`. The authoritative check is
`.github/scripts/check-release-tag.sh`, which rejects v1, archive, malformed,
and tag/workspace-mismatch inputs before publication.

## 1.x support ended

The project provides none of the following for 1.x:

- bug fixes or security patches;
- compatibility maintenance or a new 1.x release;
- an API or file-format compatibility shim;
- a persisted-store reader for 1.x data in the 2.0 line;
- a 1.x-to-2.0 migration tool or migration support.

Existing source, tags, crate releases, and documentation remain historical
artifacts. Their continued availability does not create a support obligation.

The 2.0 line will not support opening or migrating stores written by 1.x.
Persisted data written by an alpha build has no compatibility promise across
later alpha builds and may need to be recreated. This is the format direction;
it does not claim that every 2.0 format change is already implemented.

## Depending on the alpha source

Registry examples use `2.0.0-alpha.1` as the current package coordinate. Check
crates.io before using that coordinate as a registry dependency. A source
checkout can be used immediately with path dependencies while preserving the
published package alias:

```toml
selene-db = { path = "path/to/selene-db/crates/selene-db", version = "2.0.0-alpha.1" }
```

## Owner-only archive procedure

These commands are post-merge owner operations. Contributors and automation
must not create either archive ref.

```bash
git fetch origin --tags --prune
git cat-file -e b8782bec34ff0b815b62711ac7e33cac09d8ea71^{commit}
git branch --no-track archive/1.x-final b8782bec34ff0b815b62711ac7e33cac09d8ea71
git push origin archive/1.x-final
git tag -a archive-v1-eol-2026-08-21 b8782bec34ff0b815b62711ac7e33cac09d8ea71 -m "Selene DB 1.x final archived source; 1.x EOL"
git push origin archive-v1-eol-2026-08-21
```

After creating the refs, the owner must lock `archive/1.x-final` against updates,
force pushes, and deletion with GitHub branch protection or a repository
ruleset. Verify both refs resolve to the pinned commit:

```bash
archive_sha=b8782bec34ff0b815b62711ac7e33cac09d8ea71
git fetch origin refs/heads/archive/1.x-final:refs/remotes/origin/archive/1.x-final \
  refs/tags/archive-v1-eol-2026-08-21:refs/tags/archive-v1-eol-2026-08-21
test "$(git rev-parse origin/archive/1.x-final)" = "$archive_sha"
test "$(git rev-list -n 1 archive-v1-eol-2026-08-21)" = "$archive_sha"
```

Run the local policy check before creating the archive tag:

```bash
bash .github/scripts/check-release-tag.test.sh
if bash .github/scripts/check-release-tag.sh \
  archive-v1-eol-2026-08-21 2.0.0-alpha.1; then
  echo "archive tag was incorrectly authorized" >&2
  exit 1
fi
```

The owner must also inspect GitHub Actions and crates.io after pushing the
archive tag to confirm that no release workflow, GitHub Release, or package
publication was created. Local tests validate repository policy and workflow
wiring; they cannot prove the behavior of remote services.
