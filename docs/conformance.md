# iso4 API Conformance

What a sandbox actually sees. Every entry here was measured against the
shipped runtime, not read off a design document — where this file and
`DESIGN.md` disagree, this file is the one that was tested, and the
disagreement is a bug in one of them.

**Support markers**, borrowed from
[Cloudflare's console table](https://developers.cloudflare.com/workers/runtime-apis/console/)
because the distinction they draw is the one that matters to library code:

|     | Meaning                                                                                              |
| --- | ---------------------------------------------------------------------------------------------------- |
| ✅  | Implemented. Behaves as the spec describes, within the deviations noted.                             |
| 🟡  | Present and partial. Some of the surface is missing or narrowed.                                     |
| ⚪  | Present and inert. Callable, never throws, does nothing. Exists so feature-detecting libraries load. |
| ❌  | Absent. `typeof` is `"undefined"`; touching it is a `ReferenceError`.                                |
| 🔴  | Present and throws.                                                                                  |

## The shape of the surface

iso4 is not a Workers-compatible runtime and does not try to be. It is an
isolate that runs host-supplied setup code once and many pieces of untrusted
code against it. Three consequences run through everything below:

1. **No ambient I/O.** There is no network, no filesystem, no clock the guest
   can advance. Anything that reaches outside the isolate is a capability the
   host passes in explicitly as a bridge global or an import — including
   `fetch`. A sandbox with no `fetch` configured has no `fetch`.
2. **The clock is frozen** (`DESIGN.md` §7.11). `Date.now()` returns the same
   value for the whole of a turn and advances only when the runtime lets it.
   Anything whose only purpose is to measure elapsed time is therefore either
   absent or inert.
3. **Two stages, one environment.** Prefix code runs at `prepare()` for
   validation and again on each cold start; run code runs per call. The two
   stages are kept surface-identical on purpose, so a prefix cannot validate
   against something a run does not have. Where they differ, it is stated.

---

## Console

Wrapped the same way workerd does it (`Worker::setupContext`): we take V8's
own `console` object and replace five methods on it. Everything else stays as
V8 built it, which means it is callable and does nothing, because V8 routes
its console through a `ConsoleDelegate` and iso4 registers none.

| Method                                                                                                                                                                           |     | Destination                            |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --- | -------------------------------------- |
| `log` `debug` `info`                                                                                                                                                             | ✅  | `stdout`, returned on the run's result |
| `warn` `error`                                                                                                                                                                   | ✅  | `stderr`, returned on the run's result |
| `assert` `clear` `context` `count` `countReset` `dir` `dirxml` `group` `groupCollapsed` `groupEnd` `profile` `profileEnd` `table` `time` `timeEnd` `timeLog` `timeStamp` `trace` | ⚪  | nothing                                |

Output is buffered in Rust and delivered with the run's `Result` frame — there
are no streaming log frames. Each stream is capped by `limits.maxStdoutBytes`
/ `limits.maxStderrBytes`; a line that would cross the cap is dropped whole.

### How arguments are rendered

Arguments are rendered individually and joined with a single space. Plain data
becomes JSON; anything JSON cannot describe falls back to `String(value)`.

| Argument                          | Line                                                                        |
| --------------------------------- | --------------------------------------------------------------------------- |
| `'text'`                          | `text` — strings are never quoted                                           |
| `{ user: 'bob', tries: 3 }`       | `{"user":"bob","tries":3}`                                                  |
| `[1, 2, 3]`                       | `[1,2,3]`                                                                   |
| a class instance with `a = 1`     | `{"a":1}` — own fields, like any object                                     |
| an object with a `toJSON()`       | whatever `toJSON` returns                                                   |
| `new Error('boom')`               | `Error: boom` **plus its stack**, so the line contains newlines             |
| `new Map([['k', 1]])`             | `[object Map]` — JSON renders every Map as `{}`, so the type name says more |
| `Promise`, `Set`, `RegExp`        | likewise `[object Promise]`, `[object Set]`, `/re/g`                        |
| `new Uint8Array([1,2,3])`         | `1,2,3` — binary data would otherwise serialize one entry per byte          |
| `Symbol('s')`                     | `Symbol(s)`                                                                 |
| `null`, `undefined`, `42`, `true` | `null`, `undefined`, `42`, `true`                                           |

Rendering never fails a run. It reads guest properties — `toJSON`, `stack` —
so a throwing getter or a self-referencing object degrades to
`String(value)`, and in the worst case to `[unprintable]`.

Three known limits, all of which print `[object Object]`: an object that
contains itself, an object whose getter throws, and any object JSON rejects
for another reason. A `Date` renders as a quoted ISO string. **`%s` and `%d`
format specifiers are not substituted** — the format string and the arguments
are both printed.

**Timing methods are inert, not missing.** `console.time` and friends exist so
that library code calling them does not crash, but they read no clock and emit
nothing, which keeps them consistent with the frozen clock. This matches what
Cloudflare ships.

**Per stage:**

| Stage                                     | Destination                                                                          |
| ----------------------------------------- | ------------------------------------------------------------------------------------ |
| `prepare()` validation                    | discarded — there is no result frame to carry it                                     |
| warm-up (prefix evaluation on cold start) | delivered once, on the result of the call that paid for the cold start, then cleared |
| per call                                  | that call's own `stdout`/`stderr`                                                    |

The method surface is identical at every stage; only the destination differs.

---

## Timers

| API                             |     | Notes                                                          |
| ------------------------------- | --- | -------------------------------------------------------------- |
| `setTimeout`                    | ✅  |                                                                |
| `clearTimeout`                  | ✅  |                                                                |
| `setInterval` / `clearInterval` | ❌  | a repeating timer has no natural end, and a run must terminate |
| `setImmediate`                  | ❌  | Node-only                                                      |
| `queueMicrotask`                | ❌  | gap — see below                                                |
| `scheduler.wait()`              | ❌  | Workers-specific                                               |

Timers are implemented natively inside the instance loop — no bridge, no IPC,
no thread per timer — and virtualized onto the frozen clock. Measured
behavior:

- ids are per-run numbers; `clearTimeout` cannot reach another run's timers
- `clearTimeout` of an unknown or `undefined` id is a silent no-op
- extra arguments are forwarded: `setTimeout(fn, ms, ...args)`
- a non-function callback throws `TypeError`; the string-eval form
  (`setTimeout('1+1', 0)`) is not supported and throws the same way
- `NaN`, negative, and omitted delays all fire immediately
- equal deadlines fire in creation order
- at most 10 000 timers may be pending per run

---

## Clock, dates, and time

| API           |     | Notes                                                                                                 |
| ------------- | --- | ----------------------------------------------------------------------------------------------------- |
| `Date`        | ✅  | `Date.now()` and `new Date()` read the frozen clock                                                   |
| `Intl`        | ✅  | full ICU data is embedded; `DateTimeFormat` with no argument formats the frozen "now"                 |
| `Temporal`    | 🟡  | `Temporal.Now.*` derives from the frozen instant; the rest is V8's, and V8's Temporal is still moving |
| `performance` | ❌  | `performance.now()` exists only to measure elapsed time                                               |

The clock advances at run entry and on every frame received from the host, in
whole milliseconds, monotonically. Inside a single stretch of JavaScript it
does not move — a busy loop cannot observe time passing. This is the workerd
model, one step stricter.

---

## Encoding and URLs

| API                                       |     | Notes                                                                        |
| ----------------------------------------- | --- | ---------------------------------------------------------------------------- |
| `TextEncoder`                             | ✅  | `encode`, `encodeInto`, `encoding`                                           |
| `TextDecoder`                             | 🟡  | `decode`, `encoding`. No `fatal`, `ignoreBOM`, or `stream` options           |
| `URL`                                     | ✅  | backed by [ada](https://github.com/ada-url/ada), the parser Node itself uses |
| `URLSearchParams`                         | ✅  |                                                                              |
| `TextEncoderStream` / `TextDecoderStream` | ❌  | no stream globals to build on                                                |
| `atob` / `btoa`                           | ❌  | gap                                                                          |
| `URLPattern`                              | ❌  |                                                                              |

`URL.prototype`, `URLSearchParams.prototype` and `Headers.prototype` carry a
few underscore-prefixed internal helpers (`_apply`, `_parse`, `_sorted`, …).
They are implementation detail, not API, and may disappear.

---

## Fetch types

The classes exist even when no `fetch` is configured, because they are how
request and response values cross the bridge.

| Class      |               | Missing Versus the Spec                                                                            |
| ---------- | ------------- | -------------------------------------------------------------------------------------------------- |
| `Headers`  | ✅            | — (case-insensitive, keeps duplicates, `getSetCookie` present)                                     |
| `Request`  | 🟡            | `formData`, `blob`, `signal`, `mode`, `credentials`, `cache`, `integrity`, `referrer`, `keepalive` |
| `Response` | 🟡            | `formData`, `blob`                                                                                 |
| `fetch`    | ❌ by default | host-supplied capability; `@iso4/fetch` ships a hardened implementation                            |

`Response.body` is `null` for a buffered body. Streamed bodies exist on the
wire in both directions, but the guest reads them through the runtime's own
stream object rather than a spec `ReadableStream` — see `DESIGN.md` §7.3 and
`docs/protocol.md` §5.5.

---

## Streams and events

| API                                                 |     |
| --------------------------------------------------- | --- |
| `ReadableStream` `WritableStream` `TransformStream` | ❌  |
| `CompressionStream` / `DecompressionStream`         | ❌  |
| `AbortController` / `AbortSignal`                   | ❌  |
| `Event` `EventTarget` `CustomEvent`                 | ❌  |
| `MessageChannel` `MessagePort` `BroadcastChannel`   | ❌  |
| `WebSocket` `EventSource`                           | ❌  |
| `DOMException`                                      | ❌  |

Aborts are delivered by the host at turn boundaries rather than through an
in-sandbox `AbortSignal` (`DESIGN.md` §14.7). An in-sandbox abort surface is
recorded as future scope.

---

## Modules

| Specifier                           |     | Notes                                                                                           |
| ----------------------------------- | --- | ----------------------------------------------------------------------------------------------- |
| `node:async_hooks`                  | 🟡  | `AsyncLocalStorage` only. **Run code only** — a prefix importing it gets `ERR_MODULE_NOT_FOUND` |
| `node:process`                      | 🟡  | default export = the `process` global (see below). Available to prefix and run code             |
| every other `node:*`                | ❌  | `ERR_MODULE_NOT_FOUND`. No stub modules                                                         |
| bare specifiers (`fs`, `lodash`, …) | ❌  | `ERR_MODULE_NOT_FOUND`                                                                          |
| host-declared imports               | ✅  | the host declares every specifier a run may import, as source or as a data-shaped module        |

There is no resolver, no `node_modules`, and no `require`. Unlike Workers,
iso4 ships **no non-functional stubs**: an unavailable module fails at import
rather than at first call.

---

## Language and engine

| API                                  |     | Notes                                                                                               |
| ------------------------------------ | --- | --------------------------------------------------------------------------------------------------- |
| ECMAScript built-ins                 | ✅  | whatever the embedded V8 provides, including `Temporal`, `Iterator` helpers, `AsyncDisposableStack` |
| `eval` / `new Function`              | 🟡  | allowed while prefix code evaluates, `EvalError` from run code. `DESIGN.md` §7.4                    |
| `Error.captureStackTrace`            | ✅  | V8 extension; `Error.stackTraceLimit` is 10                                                         |
| `SharedArrayBuffer`                  | ❌  | deleted from the global object                                                                      |
| `Atomics`                            | 🟡  | present and legal on plain `ArrayBuffer`s; `Atomics.wait` is disabled isolate-wide                  |
| `WebAssembly`                        | ❌  | deleted from the global object; wasm codegen is additionally denied isolate-wide. `DESIGN.md` §7.5  |
| `process`                            | 🟡  | curated surface with a real per-run `env` — see the `process` section below                         |
| `Buffer` `require` `global`          | ❌  | Node globals are not provided                                                                       |
| `navigator` `self` `caches` `crypto` | ❌  |                                                                                                     |

**Code generation** is a `prepare()`-time capability on purpose: setup code
may compile fast paths from strings (the zod pattern), and the moment per-run
code starts, `eval` and `new Function` throw a catchable `EvalError` at the
call site. There is no override. This is the same line workerd draws.

---

## process

A lazily materialised, non-enumerable `process` global (built on first
access, so untouched runs pay nothing — workerd's pattern), importable as
`node:process` (default export). The surface is curated, not a Node
emulation; everything not listed is `undefined`, never a stub.

| Member                                                      |     | Notes                                                                                                             |
| ----------------------------------------------------------- | --- | ----------------------------------------------------------------------------------------------------------------- |
| `env`                                                       | ✅  | writable per-run snapshot of the host-declared entries; writes coerce to strings and never reach the host         |
| `exit(code)`                                                | ✅  | terminates the run with `ERR_PROCESS_EXIT` (never returns; taints a warm instance — workerd's semantics)          |
| `nextTick(cb, ...args)`                                     | 🟡  | microtask approximation, like workerd (no real nextTick queue)                                                    |
| `platform` `arch`                                           | ✅  | `'linux'` / `'x64'` constants (workerd's posture)                                                                 |
| `title` `argv` `argv0` `execArgv` `pid` `ppid`              | ✅  | `'iso4'` / `['iso4']` / `'iso4'` / `[]` / `1` / `0`                                                               |
| `getBuiltinModule(id)`                                      | ✅  | resolves `node:process` and `node:async_hooks`; `undefined` otherwise                                             |
| `emitWarning(warning, type?)`                               | 🟡  | emits `'warning'` to listeners; with none registered the warning lands on captured stderr                         |
| `on` `once` `off` `emit` (+ aliases, `listeners`, counts)   | 🟡  | minimal real emitter — `emit` fires listeners, but the runtime emits nothing itself except `emitWarning`'s event  |
| `memoryUsage()`                                             | 🟡  | all-zero shape (workerd's behavior)                                                                               |
| `version` `versions`                                        | ❌  | deliberately absent — iso4 is not Node and does not claim a Node version, so Node-detection snippets report false |
| `cwd` `chdir` `stdin` `stdout` `stderr` `hrtime` `uptime` … | ❌  | everything filesystem/OS-shaped is `undefined`                                                                    |

**`process.env` semantics.** The host declares entries per prefix
(`prepare({ env })`) and per run (`env` on `run()`/`execute()`/`call()` — a
run-level env replaces the prefix env wholesale, no merging). Prefix
evaluation (warm-up, `prepare()` validation) reads the prefix env. Each run
gets its own writable snapshot: reads/writes/deletes work, writes are coerced
to strings (Node's rule), and nothing written survives the run or reaches the
host. `process` is a reserved global name (`ERR_RESERVED_NAME`).

**Known deviations.**

- `process.env = {...}` throws — the property is an accessor, and module
  code is always strict. Node allows the assignment. Mutate entries instead.
- Prefix-stage writes to `process.env` are visible for the rest of the
  setup stage only; run snapshots always start from the declared entries
  (`process.env.NODE_ENV ??= 'production'` in a prefix does not carry into
  runs — pass it via `env` instead). Deliberate: run env must not depend on
  what a nondeterministic warm-up computed on one particular instance.
- Listener registrations (`process.on(...)`) live on the shared `process`
  object and persist for a warm instance's lifetime, like any other global
  state a run leaves behind (warmth-carryover contract).
- `versions`/`version` are absent by design, so Node-detection snippets
  (`process.versions?.node`) correctly report "not Node".

---

## Known deviations

**Prefix console output at `prepare()` is discarded.** Validation runs in a
throwaway isolate with no result frame to carry the bytes. Prefix logging is
still not an error. Tracked as
[#86](https://github.com/schplitt/iso4/issues/86).

---

## Gaps worth closing

None of these are implemented; they are listed in the order they are likely to
be missed, and none is blocked by anything architectural.

1. `queueMicrotask` — trivially expressible, and libraries reach for it.
2. `atob` / `btoa` — pure computation, no capability implications.
3. `structuredClone` — the V8 serializer is already linked in for the wire.
4. `crypto.getRandomValues` and `crypto.randomUUID` — needs a decision on
   whether a sandbox may have entropy at all, since randomness makes a prefix
   non-reproducible.
5. `AbortController` / `AbortSignal` — the mechanism exists at the host level;
   this is the in-sandbox surface for it.
6. `Event` / `EventTarget` — a prerequisite for most of the rest.
7. `Blob`, `FormData`, `File` and the `Request`/`Response` methods that
   consume them.
8. Web Streams. The largest of these by far, and the only one that would
   change the shape of the runtime rather than add to it.

---

## Verifying this document

The behavior above is pinned by tests, not by prose:

- Console surface, wrapped-versus-inert split, and prepare/run parity:
  `native/v8-runtime/src/v8.rs`, the `Console capture` test block.
- Frozen clock, including console timing inertness:
  `packages/iso4-sandbox/tests/clock.test.ts`.
- Timers: `DESIGN.md` §7.8 and the timer tests in `src/v8.rs`.
- Prefix/run environment parity: `the_console_surface_is_identical_at_prepare_and_at_run`.

If you change what the sandbox exposes, update this file in the same commit.
