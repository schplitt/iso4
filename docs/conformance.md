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
Arguments are stringified and joined with a single space. There is no `%s`
format-specifier handling on the wrapped methods.

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
| every other `node:*`                | ❌  | `ERR_MODULE_NOT_FOUND`. No stub modules                                                         |
| bare specifiers (`fs`, `lodash`, …) | ❌  | `ERR_MODULE_NOT_FOUND`                                                                          |
| host-declared imports               | ✅  | the host declares every specifier a run may import, as source or as a data-shaped module        |

There is no resolver, no `node_modules`, and no `require`. Unlike Workers,
iso4 ships **no non-functional stubs**: an unavailable module fails at import
rather than at first call.

---

## Language and engine

| API                                   |     | Notes                                                                                               |
| ------------------------------------- | --- | --------------------------------------------------------------------------------------------------- |
| ECMAScript built-ins                  | ✅  | whatever the embedded V8 provides, including `Temporal`, `Iterator` helpers, `AsyncDisposableStack` |
| `eval` / `new Function`               | 🟡  | allowed while prefix code evaluates, `EvalError` from run code. `DESIGN.md` §7.4                    |
| `Error.captureStackTrace`             | ✅  | V8 extension; `Error.stackTraceLimit` is 10                                                         |
| `SharedArrayBuffer`                   | ❌  | deleted from the global object                                                                      |
| `Atomics`                             | 🟡  | present and legal on plain `ArrayBuffer`s; `Atomics.wait` is disabled isolate-wide                  |
| `WebAssembly`                         | 🟡  | **see the deviation below**                                                                         |
| `process` `Buffer` `require` `global` | ❌  | Node globals are not provided                                                                       |
| `navigator` `self` `caches` `crypto`  | ❌  |                                                                                                     |

**Code generation** is a `prepare()`-time capability on purpose: setup code
may compile fast paths from strings (the zod pattern), and the moment per-run
code starts, `eval` and `new Function` throw a catchable `EvalError` at the
call site. There is no override. This is the same line workerd draws.

---

## Known deviations

**WebAssembly is documented as disabled and is not.** `DESIGN.md` §7.5 says
`set_allow_wasm_code_generation_callback(_ => false)`; that callback is not
installed on either isolate path. Measured today: `new WebAssembly.Module`,
`WebAssembly.validate` and `WebAssembly.compile` all succeed from run code.
Cloudflare forbids exactly these for security reasons. Tracked as
[#122](https://github.com/schplitt/iso4/issues/122).

**`WebAssembly.Memory({ shared: true })` mints a `SharedArrayBuffer`**, even
though the constructor is deleted from the global object. Shared memory plus a
counting thread is the canonical way to rebuild a high-resolution timer, so
this is the one hole in the frozen clock. It is inert in practice — the guest
has no second thread to share the buffer with, and `Atomics.wait` is disabled
isolate-wide — but it should not be reachable. Blocking wasm code generation
closes it too.

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
