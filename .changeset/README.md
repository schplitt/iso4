# Changesets

This directory contains [changesets](https://github.com/changesets/changesets)
— short markdown files describing changes to packages that need a version
bump and a changelog entry.

## When to add a changeset

Every PR that changes user-visible behavior in any published package
(`iso4`, `@iso4/fetch`, future `@iso4/*` packages) must include a
changeset. Internal refactors, docs-only edits, and test-only edits do not
require one.

## How to add one

```sh
pnpm changeset
```

The interactive prompt asks which packages are affected and whether each is
a `patch`, `minor`, or `major` bump. Commit the generated file in
`.changeset/<random-name>.md` as part of your PR.

## How releases work

On push to `main`, the release workflow:

1. If any changeset files exist, opens (or updates) a "Version Packages"
   PR that consumes them — bumping `package.json` versions and writing
   `CHANGELOG.md` entries per package.
2. When that PR is merged, the workflow publishes every bumped package to
   npm and creates git tags for each.

So the path from "change merged" to "release published" is two PR merges:
the change itself, then the Version Packages PR.

## Naming and scope rules

- Each changeset is scoped per package; one PR can produce multiple
  package bumps in a single changeset file.
- `iso4` and the per-platform `@iso4/v8-*` packages must bump major
  together when the IPC wire protocol changes. (Cannot be enforced
  mechanically — reviewers must catch it. 
- The `iso4-monorepo` root package is private and is never published; do
  not include it in changesets.
