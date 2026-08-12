---
"@iso4/sandbox": minor
---

refactor: remove runtime V8 snapshot creation — prefixes are validated source, re-evaluated per run (#60, #61, #62)

Since the V8 13.0 → 14.7 bump, runtime snapshot creation could crash the
child process two ways: concurrent `prepare()` calls raced inside
`create_blob` on the process-shared read-only heap, and V8 frees that shared
heap whenever the last live isolate dies — so a snapshot created before such
a reset referenced memory that no longer existed when a later run restored
it. The second failure is unfixable by locking, and upstream V8 documents
runtime multi-snapshot creation as outside the supported envelope. Runtime
snapshotting is therefore removed entirely (see DESIGN.md §11.6 for the full
decision record).

What changes for users — the public API and the wire protocol are unchanged:

- `prepare()` still validates the prefix up front (same error codes:
  `ERR_PREFIX_BRIDGE_CALL`, `ERR_PREFIX_DID_NOT_SETTLE`, compile/module
  errors) and returns the same `Prefix` handle. Internally it now caches the
  validated **source** instead of building a snapshot blob.
- `prefix.run()` boots a fresh isolate and **re-evaluates the prefix source**
  before the postfix. Clean-slate-per-run semantics are unchanged.
- Per-run latency now includes prefix evaluation: sub-millisecond for
  typical setup-sized prefixes, proportional to prefix size for heavy ones.
  Prefix-less `run()` pays web-runtime installation per run again (~0.5 ms).
- Prefix evaluation happens under the run's wall/CPU limits (previously
  snapshot creation at `prepare()` was unlimited); a prefix that loops now
  costs the run its budget instead of hanging `prepare()`.
- A nondeterministic prefix (`Math.random()`, `Date.now()`) now produces
  per-run values instead of one frozen value; deterministic prefixes behave
  identically.

This closes the intermittent child-process SIGSEGV under concurrent
`prepare()` (#60) — the crashing code path no longer exists.
