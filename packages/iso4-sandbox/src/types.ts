/**
 * \@iso4/sandbox — public API types.
 *
 * Execution model: precompile a prefix once into a V8 startup snapshot,
 * then call prefix.run({ code }) for each piece of dynamic code (e.g.
 * agent-generated). A fresh isolate is created from the snapshot per call.
 * Multiple concurrent callers each get their own pool slot and run in
 * parallel. See DESIGN.md §1 and §13.
 */

// ─────────────────────────────────────────────────────────────────────────
// Resource limits
// ─────────────────────────────────────────────────────────────────────────

export interface ResourceLimits {
  /**
   * Hard cap on memory the isolate can use, in megabytes.
   * Covers the V8 heap and all external `ArrayBuffer` allocations.
   * @default 128
   */
  memoryMb: number

  /**
   * Maximum *active* execution time in milliseconds. Time spent waiting on
   * host bridge calls (globals, host imports) is excluded.
   * @default 5_000
   */
  cpuTimeMs: number

  /**
   * Hard wall-clock cap including async waits.
   * @default 30_000
   */
  wallTimeMs: number

  /**
   * Maximum bytes allowed in a single bridge call payload (arguments +
   * return value combined). Applies to all host-bridge calls — globals and
   * host imports alike.
   * @default 16 * 1024 * 1024
   */
  maxBridgePayloadBytes: number

  /**
   * Maximum number of bridge calls (globals + host imports combined) a
   * single run may make. Zero disables the per-run limit entirely.
   *
   * Protects against untrusted sandbox code making unbounded calls to host
   * handlers (e.g. `while(true) { await myTool() }`) which consumes near-zero
   * CPU budget but can exhaust host resources (network, DB, LLM quota).
   *
   * @default 10
   */
  maxBridgeCalls: number
}

// ─────────────────────────────────────────────────────────────────────────
// Globals (generic bridge — any non-reserved name is permitted)
// ─────────────────────────────────────────────────────────────────────────

/**
 * Host-provided globals. Any name not reserved by V8 or the runtime is
 * permitted. Each value becomes a bridge stub in the sandbox global object.
 *
 * Reserved names (must not be used): `console`, `URL`, `URLSearchParams`,
 * `TextEncoder`, `TextDecoder`, `crypto`, `Event`, `AbortController`,
 * `AbortSignal`.
 *
 * Common usage:
 * ```ts
 * globals: {
 * fetch: myFetchHandler,   // \@iso4/fetch provides createSafeFetch()
 * myTool: async (arg) => doSomething(arg),
 * }
 * ```
 *
 * `fetch` is not special — it goes through the same bridge path as any
 * other entry.
 *
 * Three value shapes are supported — see `HostGlobalValue`.
 */
export type HostGlobals = Record<string, HostGlobalValue>

/**
 * The value types accepted for a named global.
 *
 * - `HostExportFunction` — bridge call: every sandbox invocation dispatches
 *   to the host handler.
 * - `string` — a JS expression installed once at prefix setup:
 *   `globalThis.<name> = <expr>`. Use for pure sandbox utilities that need
 *   no host involvement.
 * - `BridgeWithShim` — bridge call + in-sandbox result wrapper. The handler
 *   can be rebound per `prefix.run()`; the shim is compiled into the snapshot.
 */

export type HostGlobalValue = HostExportFunction | string | BridgeWithShim<any>

/**
 * A global that pairs a bridge handler with an in-sandbox shim that wraps
 * its return value. Typical use: adding `.json()` / `.text()` convenience
 * methods to a fetch response without extra bridge round-trips.
 *
 * The TS layer:
 *  1. Registers `handler` as a bridge stub under `__iso4_<name>_h`.
 *  2. Prepends to the prefix source:
 *     ```js
 *     globalThis.<name> = async (...args) =>
 *       await (<shim>)(await __iso4_<name>_h(...args))
 *     ```
 *
 * The shim is a JS function expression `(result) => transformedResult` (sync
 * or async). Sandbox built-ins (TextDecoder, JSON, …) are available in scope.
 * The private stub `__iso4_<name>_h` stays on globalThis so the handler can
 * be rebound per `prefix.run()` while the shim stays fixed in the snapshot.
 */
/**
 * `H` is the specific handler function type. Carried through the type system
 * so `RebindGlobals` can enforce that any per-run replacement has the same
 * return type, keeping it compatible with the shim that is already compiled
 * into the prefix snapshot.
 */
export interface BridgeWithShim<
  H extends (...args: unknown[]) => unknown = (...args: unknown[]) => unknown,
> {
  kind: 'bridge-with-shim'
  /**
   * Bridge handler — registered as `__iso4_<name>_h` for rebinding.
   */
  handler: H
  /**
   * JS function expression that transforms the bridge return value.
   * Runs in the sandbox. May be async.
   *
   * @example
   * ```ts
   * shim: `(result) => ({
   *   ...result,
   *   ok:   result.status >= 200 && result.status < 300,
   *   json: () => JSON.parse(new TextDecoder().decode(result.body)),
   *   text: () => new TextDecoder().decode(result.body),
   * })`
   * ```
   */
  shim: string
}

/**
 * What can be supplied as a per-run override for a global declared at
 * precompile time:
 *
 * - `BridgeWithShim<H>` global — only the handler `H` itself. The shim is
 *   fixed in the snapshot; rebinding must preserve the return type so the
 *   shim keeps working.
 * - `HostExportFunction` global — any compatible `HostExportFunction`.
 * - `string` global — cannot be rebound (compiled into the snapshot).
 */
export type RebindValue<V extends HostGlobalValue>
  = V extends BridgeWithShim<infer H> ? H
    : V extends HostExportFunction ? V
      : never

/**
 * The per-run globals override map for a `Prefix<G>`.
 *
 * String-valued globals are filtered out entirely (key remapping to `never`)
 * rather than mapped to `?: never` — they cannot be rebound because their
 * expression is compiled into the snapshot, and they should not appear as
 * valid keys at all.
 *
 * Each remaining key is optional and typed as `RebindValue<G[K]>`, which
 * enforces the correct constraint per global kind.
 */
export type RebindGlobals<G extends HostGlobals> = {
  [K in keyof G as G[K] extends string ? never : K]?: RebindValue<G[K]>
}

// ─────────────────────────────────────────────────────────────────────────
// Imports (source modules and host-implemented modules)
// ─────────────────────────────────────────────────────────────────────────

/**
 * Resolution order: static map → dynamic resolver → ERR_MODULE_NOT_FOUND.
 */
export interface ImportsConfig {
  static?: Record<string, ImportDefinition>
  resolve?: (
    specifier: string,
    importer: string | null,
  ) => Promise<ImportDefinition | null> | ImportDefinition | null
}

export type ImportDefinition = SourceImport | HostImport

/**
 * Host provides ESM source; V8 compiles and caches it in-isolate.
 */
export interface SourceImport {
  kind: 'source'
  source: string
}

/**
 * Host provides an object; each function becomes a bridge-call stub.
 */
export interface HostImport {
  kind: 'host'
  exports: HostExports
}

export interface HostExports {
  default?: HostExportValue
  [name: string]: HostExportValue | undefined
}

export type HostExportValue = HostExportData | HostExportFunction

/**
 * Represents the plain-data values that cross the bridge.
 * Kept for `HostExports` / import system typing.
 */
export type HostExportData
  = | null
    | undefined
    | boolean
    | number
    | bigint
    | string
    | Uint8Array
    | HostExportData[]
    | { [key: string]: HostExportData }

/**
 * A host-side function exposed as a global or import export.
 *
 * Args and return are `unknown` — the bridge serialises/deserialises values
 * with V8 ValueSerializer; we don't try to model that at the type level.
 * The one invariant that matters (functions cannot cross the boundary) is
 * enforced at runtime by the V8 layer, not here.
 *
 * Use specific function types for your handlers and let TypeScript infer
 * them through `HostGlobals` and `RebindGlobals<G>`.
 */
export type HostExportFunction = (...args: unknown[]) => unknown

// ─────────────────────────────────────────────────────────────────────────
// Sandbox (runtime)
// ─────────────────────────────────────────────────────────────────────────

export interface SandboxOptions {
  /**
   * Maximum number of isolates (UDS connection slots) running concurrently.
   * The Sandbox maintains a pool of this many connections to the Rust process;
   * each executes one run() at a time. Additional callers queue.
   *
   * Multiple MCP agents / concurrent callers each get their own slot and
   * run in parallel up to this limit.
   *
   * Defaults to the number of logical CPUs on the host machine.
   */
  maxIsolates?: number

  /**
   * Override the path to the Rust V8 binary.
   * Default: auto-detect from \@iso4/v8-* platform packages.
   */
  binaryPath?: string
}

export interface Sandbox {
  /**
   * Execute a piece of JavaScript in a fresh V8 isolate.
   * For repeated use with a shared context, prefer precompile() + prefix.run().
   */
  run: (options: RunOptions) => Promise<RunResult>

  /**
   * Pre-compile a prefix of code into a V8 startup snapshot.
   *
   * The type parameter `G` is inferred from the `globals` you pass and flows
   * into the returned `Prefix<G>`. This lets TypeScript enforce at the call
   * site that `prefix.run()` can only rebind names declared here:
   *
   * ```ts
   * const prefix = await sandbox.precompile({
   *   globals: { fetch: myHandler, myTool: otherHandler },
   * })
   * // prefix: Prefix<{ fetch: ..., myTool: ... }>
   *
   * prefix.run({ globals: { fetch: newHandler } })          // ✅
   * prefix.run({ globals: { undeclared: someHandler } })    // ❌ TS error
   * ```
   *
   * Globals declared here serve as **default** implementations for all runs.
   * `prefix.run()` may override any subset; omitted names reuse the default.
   */
  // eslint-disable-next-line @typescript-eslint/no-empty-object-type
  precompile: <G extends HostGlobals = {}>(
    options: PrecompileOptions<G>,
  ) => Promise<Prefix<G>>

  /**
   * Tear down the Rust process. Outstanding prefixes are invalidated.
   */
  dispose: () => Promise<void>

  /**
   * Enables `await using sandbox = await createSandbox()` syntax.
   */
  [Symbol.asyncDispose]: () => Promise<void>

  readonly alive: boolean
}

export type CreateSandbox = (options?: SandboxOptions) => Promise<Sandbox>

// ─────────────────────────────────────────────────────────────────────────
// Precompiled prefix
// ─────────────────────────────────────────────────────────────────────────

/**
 * Options for `sandbox.precompile()`.
 *
 * `G` is the shape of globals this prefix declares. TypeScript infers it
 * from the `globals` field, so the returned `Prefix<G>` is typed to allow
 * only those names to be rebound in subsequent `prefix.run()` calls.
 */
export interface PrecompileOptions<G extends HostGlobals> {
  /**
   * ESM source code to evaluate before snapshotting. Top-level await works.
   *
   * @example
   *   import { search } from 'tools:search';
   *   globalThis.search = search;
   */
  code: string

  /**
   * Declares the globals that sandbox code will be able to call.
   *
   * This defines the **bridge surface shape** — the full set of names the
   * sandbox may invoke as globals. Names declared here:
   *
   * - Become bridge stubs in the V8 context on every `prefix.run()` call.
   * - Can be rebound per-run via `prefix.run({ globals })`.
   * - Cannot be supplemented at run time — a name not declared here will
   *   cause `ERR_UNDECLARED_BINDING` if passed to `prefix.run()`.
   *
   * The handlers provided here are the **default** implementations reused by
   * any `prefix.run()` call that does not override them.
   */
  globals?: G

  imports?: ImportsConfig

  /**
   * Resource limits applied to prefix code evaluation.
   *
   * **Important:** limits are NOT enforced during `precompile()`. Prefix code
   * is assumed to be developer-authored, not AI-agent-generated.
   *
   * **Currently a no-op placeholder.** Pass limits directly to each
   * `prefix.run()` call.
   */
  limits?: Partial<ResourceLimits>

  /**
   * Optional filename for stack traces. Default: "<prefix>".
   */
  filename?: string
}

/**
 * Handle to a precompiled prefix.
 *
 * `G` is the shape of globals declared at precompile time. `prefix.run()`
 * accepts only `Partial<G>` for its `globals` field — you may override any
 * declared global's implementation per run, but you cannot add names that
 * were not declared at precompile time.
 */
export interface Prefix<G extends HostGlobals> {
  readonly id: string

  /**
   * Execute dynamic code against this prefix's snapshot state.
   *
   * Globals declared at precompile time are active. You may rebind any subset
   * of them via `options.globals`; the precompile-time handler is used for
   * any name not overridden here.
   *
   * @example
   *   const result = await prefix.run({
   *     code: `export default await (${agentFn})()`,
   *   })
   */
  run: (options: PrefixRunOptions<G>) => Promise<RunResult>

  /**
   * Release the snapshot. Subsequent run() calls reject. Idempotent.
   */
  dispose: () => Promise<void>

  /**
   * Enables `await using prefix = await sandbox.precompile(...)` syntax.
   */
  [Symbol.asyncDispose]: () => Promise<void>

  readonly alive: boolean
}

/**
 * Options for `prefix.run()`.
 *
 * `G` is the declared-globals shape from the matching `precompile()` call.
 * The `globals` field is typed as `Partial<G>` — only names present in `G`
 * may be overridden. Passing any other name is a TypeScript error at compile
 * time and `ERR_UNDECLARED_BINDING` at runtime.
 */
export interface PrefixRunOptions<G extends HostGlobals> {
  /**
   * Dynamic ESM source code to compile and run against the prefix state.
   */
  code: string

  /**
   * Override implementations for a **subset** of globals declared at
   * precompile time. Only names present in `G` are accepted (TypeScript
   * error + runtime `ERR_UNDECLARED_BINDING` for anything else), but the
   * function signature does not need to match the precompile-time handler
   * exactly — any `HostExportFunction`-compatible value is accepted.
   */
  globals?: RebindGlobals<G>

  /**
   * Rebind host-module imports declared at precompile time.
   * Source module imports are frozen in the snapshot and cannot be rebound.
   * (Phase 6/7 — not yet implemented.)
   */
  imports?: ImportsConfig

  limits?: Partial<ResourceLimits>
  signal?: AbortSignal
  filename?: string
}

// ─────────────────────────────────────────────────────────────────────────
// Direct run (no prefix)
// ─────────────────────────────────────────────────────────────────────────

export interface RunOptions {
  code: string
  limits?: Partial<ResourceLimits>
  globals?: HostGlobals
  imports?: ImportsConfig
  signal?: AbortSignal
  filename?: string
}

// ─────────────────────────────────────────────────────────────────────────
// Result
// ─────────────────────────────────────────────────────────────────────────

export type RunResult = RunSuccess | RunFailure

export interface RunSuccess {
  ok: true
  exports: SandboxExports
  stdout: string[]
  stderr: string[]
  durationMs: number
}

export interface RunFailure {
  ok: false
  error: RunError
  stdout: string[]
  stderr: string[]
  durationMs: number
}

export type SandboxExports = {
  /**
   * Value of `export default` in user code, or `undefined`.
   */
  default: unknown
} & {
  /**
   * Named exports live directly on the export object.
   */
  [name: string]: unknown
}

export interface RunError {
  code: RunErrorCode
  name: string
  message: string
  stack?: string
}

export type RunErrorCode
  = | 'ERR_USER_CODE'
    | 'ERR_MEMORY_LIMIT'
    | 'ERR_CPU_TIMEOUT'
    | 'ERR_WALL_TIMEOUT'
    | 'ERR_ABORTED'
    | 'ERR_MODULE_NOT_FOUND'
    | 'ERR_COMPILE'
    | 'ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED'
    | 'ERR_EXPORT_NOT_SERIALIZABLE'
    | 'ERR_EXPORT_TOO_LARGE'
    | 'ERR_EXPORT_UNRESOLVED_PROMISE'
    | 'ERR_HOST_BRIDGE'
    | 'ERR_BRIDGE_PAYLOAD_TOO_LARGE'
    | 'ERR_BRIDGE_CALL_LIMIT_EXCEEDED'
    | 'ERR_UNDECLARED_BINDING'
    | 'ERR_PREFIX_DISPOSED'
    | 'ERR_INTERNAL'
