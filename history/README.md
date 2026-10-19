# history

The record of how this repository's history was extracted from the monorepo,
[The Test Cabinet](https://github.com/TheClockwyrks/TheTestCabinet), where gg's
crates, packages, scripts and documentation lived until the extraction. Nothing
builds or tests against these files; they are kept so the provenance outlives
the scratch clone the extraction ran in.

## The extraction

The history below the scaffold commit is the monorepo's `nightly` at
`14415599f15de83b0fcfc134708a7903e0e7d5f1`, filtered to the paths in
[`paths.txt`](paths.txt) by git filter-repo 2.47.0, with gg's documentation
pages renamed into `docs/`:

```sh
git clone -q --no-local --no-checkout --single-branch --branch nightly --no-tags \
  <the-test-cabinet> x
cd x
uvx --from git-filter-repo==2.47.0 git-filter-repo --force \
  --paths-from-file ../paths.txt \
  --path-rename apps/docs/src/content/docs/gg/:docs/ \
  --prune-empty always --preserve-commit-hashes
```

Of the 6,826 commits reachable from that commit, 733 touched a path in the list
and were kept (56 of them merges); the rest were pruned. No tag was carried. The
tip tree of every moved directory, and the tip blob of every moved file, hashes
the same as the monorepo's at that commit.

The paths are kept where they were in the monorepo, so every relative path
inside the crates and packages (the `include_str!` of an SDK source, a
package's build script finding the repository root) works as it did. The one
rename is the documentation: the monorepo's documentation site served gg's
pages from `apps/docs/src/content/docs/gg/`, and here they are `docs/`, which
the site reads from the superrepo's checkout of this repository. The filter
selects by the old path and the rename applies after it, so a page's history
reaches back to the commits that wrote it there, and the commits before the
rename name the old path.

## The files

- [`paths.txt`](paths.txt) is the list handed to `--paths-from-file`
  (sha256 `f9009d872f073b00333eb9de3e7fe7d2610ecb5c6925afaf04f03b7e4aaa5f91`).
  It is generated, not hand-written: the eighteen moved directories
  (`crates/gg/`, `crates/gg-sandbox-artifacts/`, the eleven
  `packages/gg-sandbox*/` packages, `scripts/gg-npm-locks/`,
  `containers/gg-toolchains/`, the `agent-prompts` and `gg-sdk-documentation`
  skills under `.claude/skills/`, and `apps/docs/src/content/docs/gg/`), the
  sixty moved files (the `scripts/gg-*.sh` family,
  `scripts/build-gg-static.sh`, gg's CI scripts and toolchain installers under
  `scripts/ci/` with their tests, the `tcab-lib.sh` they source, and the three
  `.azure/tcab/gg-*.yml` pipeline templates), and every path a tracked file
  among them had before a rename or copy, collected by
  `git log --follow --name-status` over each of the 912 files, so a file's
  history reaches back past the commits that moved it into place.
- [`commit-map`](commit-map) is filter-repo's map from each monorepo commit (left)
  to the commit it became here (right), all zeros for a pruned one.
  `--preserve-commit-hashes` left the commit ids quoted in the messages as they
  were, so an id in a message below the scaffold commit names a monorepo commit,
  and this map translates it.

## The live-path review

`paths.txt` intersected with the monorepo's tree at the extraction commit names,
outside the moved directories and files, exactly one live file:

```text
scripts/ci/lib.sh
```

It is a copy source: `scripts/ci/tcab-lib.sh` began as a copy of it, and the
monorepo kept its own, the workspace template's helpers. It was kept in the list
so `git log --follow` and blame of `tcab-lib.sh` reach its history before the
copy. Its commits after the copy, which touch the monorepo's copy only, are in
this history too, and the scaffold commit deletes the file.

`scripts/ci/gg-prebuilt.sh`, which staged a prebuilt gg into the monorepo's
service images, was not moved: it belongs to the images that consume gg, and a
release of gg is what they fetch.
