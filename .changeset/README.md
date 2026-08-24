# Changesets

Short markdown files that describe which packages changed and at what
bump level. `changesets/action` consumes them on push to `main`.

## When to add a changeset

Any PR that changes user-visible behaviour in a published package needs
one. Pure refactors, docs edits, and test-only changes do not.

```sh
pnpm changeset
```

The interactive prompt asks which packages are affected and whether the
bump is `patch`, `minor`, or `major`. Commit the generated file in
`.changeset/<random-name>.md` as part of your PR.

## Keep the body short

A changeset becomes a `CHANGELOG.md` entry, so it is release notes, not a
commit message. Write it as one conventional-commit line — `fix: …`,
`feat: …`, `perf: …` — which on its own is usually enough. Add at most two
sentences after it, and only for what a reader of the changelog needs: what
they will notice, or what they have to do differently.

Rationale, mechanism, measurements and file names belong in the commit
message and the PR, where anyone investigating will look. Repeating them
here makes the changelog long without making it more useful.

## Fixed version group

`@iso4/sandbox` and all `@iso4/v8-*` platform packages are in a **fixed
group** — they always publish at the same version. You only ever write a
changeset for `@iso4/sandbox`; the platform packages bump automatically.
This guarantees the binary version always matches the host package.

## How releases work

1. Merge a PR that includes a changeset file into `main`.
2. The release workflow opens (or updates) a **"Version Packages" PR**
   that bumps `package.json` versions and writes `CHANGELOG.md` entries.
3. Merge the Version Packages PR.
4. The release workflow runs again — no pending changesets — so it
   builds the native binaries, places them in the platform packages,
   runs `changeset publish`, pushes to npm, and creates GitHub releases
   with the changelog.

Publishing uses GitHub OIDC (no `NPM_TOKEN` secret required). Each
package must be configured on npmjs.com under Publishing → "Allow
publishing from GitHub Actions without a token".
