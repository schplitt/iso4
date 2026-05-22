/**
 * iso4 — public API types for the `iso4` package.
 *
 * Canonical surface for the runtime. Cross-referenced with `../../../DESIGN.md`
 * and `../../../MONOREPO.md`; any deviation between this file and those is
 * a bug.
 *
 * Pure JavaScript sandbox running in a separate Rust V8 process.
 *
 * Invariants enforced at this boundary:
 *   - Only data crosses; functions never cross by value.
 *   - User code is ESM. Results come back through `export`.
 *   - Globals are runtime-curated; the host can only contribute from a
 *     small allowlist (currently just `fetch`).
 *   - Imports are the extensibility surface. Anything richer than `fetch`
 *     goes here.
 *
 * Hardened fetch defaults (SSRF protection, header validation, etc.) live
 * in the separate `@iso4/fetch` package. The `iso4` core accepts any
 * `FetchHandler`; the recommended default is `createSafeFetch` from
 * `@iso4/fetch`.
 *
 * Stub implementations of common stdlib modules (`node:fs`, `node:crypto`,
 * etc.) live in `@iso4/<stdlib-name>` packages. Each is a tiny factory
 * returning a ready-to-plug-in `HostImport`. Hosts pick and choose.
 */

// ─────────────────────────────────────────────────────────────────────────
// Top-level runtime
// ─────────────────────────────────────────────────────────────────────────

/**
 * Options for creating the long-lived runtime process.
 * One Rust V8 process serves many `run()` calls.
 */
export interface RuntimeOptions {
	/**
	 * Maximum number of isolates running concurrently inside the Rust process.
	 * Additional `run()` calls queue. Defaults to the host's CPU count.
	 */
	maxIsolates?: number;

	/**
	 * Override the path to the Rust V8 binary.
	 * Default: auto-detect from sibling platform-specific npm packages.
	 */
	binaryPath?: string;
}

/**
 * Long-lived handle to the Rust V8 process. Create once per host process,
 * reuse across many `run()` / `precompile()` calls. Call `dispose()` on
 * shutdown.
 */
export interface Runtime {
	/**
	 * Execute a piece of JavaScript in a fresh V8 isolate and return the result.
	 * Each call gets its own isolate; no state is shared between calls.
	 *
	 * For the prefix/postfix pattern (host setup + AI-generated code), use
	 * `precompile()` followed by `prefix.run()` — that path uses V8 startup
	 * snapshots and is ~10× faster on steady-state cold start.
	 */
	run(options: RunOptions): Promise<RunResult>;

	/**
	 * Pre-compile a prefix of code and snapshot the resulting isolate state.
	 * The returned `PrecompiledPrefix` exposes its own `.run()` that boots
	 * subsequent isolates from the snapshot with the prefix already evaluated.
	 *
	 * Use cases:
	 *   - Host-provided setup + AI-generated postfix: precompile once, run
	 *     many AI-generated postfixes against the same warm state.
	 *   - Library bundles (lodash, zod, etc.) precompiled once at boot.
	 *
	 * The prefix executes once at precompile time. Any I/O it performs is
	 * baked into the snapshot — subsequent runs see the *cached* result, not
	 * a fresh one. For prefixes that must do fresh I/O per run, put that I/O
	 * in the postfix instead.
	 *
	 * `globals` and `imports` declared at precompile time define the *shape*
	 * of the bridge surface. Each subsequent `prefix.run()` can supply
	 * different *implementations* for the same names, but cannot add new
	 * names that weren't present at precompile time.
	 */
	precompile(options: PrecompileOptions): Promise<PrecompiledPrefix>;

	/**
	 * Tear down the Rust process. After this, `run()` / `precompile()` reject
	 * and any outstanding `PrecompiledPrefix` handles are invalidated.
	 */
	dispose(): Promise<void>;

	/** Whether the underlying Rust process is alive. */
	readonly alive: boolean;
}

/**
 * Create a runtime. Spawns the Rust V8 process on first call.
 */
export type CreateRuntime = (options?: RuntimeOptions) => Promise<Runtime>;

// ─────────────────────────────────────────────────────────────────────────
// Precompiled prefixes
// ─────────────────────────────────────────────────────────────────────────

/**
 * Input for `runtime.precompile()`.
 *
 * The `globals` and `imports` declared here become the bridge surface of
 * the snapshot: their *names* (and signatures, for host modules) are baked
 * in. Subsequent `prefix.run()` calls can pass different implementations
 * for the same names, but cannot introduce new ones — they would not be
 * visible inside the snapshot.
 */
export interface PrecompileOptions {
	/**
	 * ESM source code to evaluate before snapshotting.
	 * Typically: variable declarations, helper functions, library setup.
	 * Top-level `await` works.
	 *
	 * @example
	 *   const config = { apiBase: "https://api.example.com" };
	 *   function callTool(name, args) { return globalThis._tool(name, args); }
	 *   globalThis.callTool = callTool;
	 */
	code: string;

	/**
	 * Globals available to the prefix code and reusable by every run.
	 * Names declared here can be re-bound per run; names NOT declared here
	 * cannot be added per run.
	 *
	 * If the prefix doesn't actually call a declared global, you can pass
	 * a no-op implementation just to declare the slot.
	 */
	globals?: HostGlobals;

	/**
	 * Modules the prefix imports. Same rules apply: names baked in here can
	 * be re-bound per run, but new specifiers cannot be added later.
	 */
	imports?: ImportsConfig;

	/**
	 * Resource limits applied DURING precompilation (the one-time prefix run).
	 * Subsequent `prefix.run()` calls have their own limits.
	 */
	limits?: Partial<ResourceLimits>;

	/**
	 * Optional filename for stack traces of the prefix module.
	 * Default: `"<prefix>"`.
	 */
	filename?: string;
}

/**
 * Handle to a precompiled prefix held inside the Rust runtime. Run many
 * postfixes against the same warm state. Dispose to release the snapshot.
 */
export interface PrecompiledPrefix {
	/** Opaque identifier; useful for logging. */
	readonly id: string;

	/**
	 * Execute a postfix against this prefix's snapshot state.
	 *
	 * The postfix sees:
	 *   - All variables and functions defined in the prefix, with their
	 *     post-evaluation values.
	 *   - Per-run implementations for any `globals` / `imports` supplied
	 *     here (or the precompile-time defaults if omitted).
	 */
	run(options: PrefixRunOptions): Promise<RunResult>;

	/**
	 * Release the snapshot held by the Rust runtime. After this call,
	 * `.run()` rejects. Idempotent.
	 *
	 * The runtime also evicts snapshots via LRU under memory pressure;
	 * explicit `dispose()` is the deterministic way to free a snapshot.
	 */
	dispose(): Promise<void>;

	/** False once `dispose()` has been called or the snapshot was evicted. */
	readonly alive: boolean;
}

/**
 * Per-run input when running on top of a precompiled prefix.
 *
 * Differs from `RunOptions` in that `globals` and `imports` REBIND existing
 * declarations from the precompile step rather than declaring new ones.
 * If you supply a name not declared at precompile time, the runtime fails
 * the run with `code: "ERR_UNDECLARED_BINDING"`.
 */
export interface PrefixRunOptions {
	/** Postfix ESM source code. */
	code: string;

	/**
	 * Implementations for globals declared at precompile time.
	 * Names not declared at precompile time are an error.
	 */
	globals?: HostGlobals;

	/**
	 * Implementations for host-implemented imports declared at precompile time.
	 * Source-module specifiers cannot be rebound (they're frozen in the
	 * snapshot). Only `kind: "host"` imports' methods swap per run.
	 */
	imports?: ImportsConfig;

	/** Per-run resource limits. Independent of the precompile-time limits. */
	limits?: Partial<ResourceLimits>;

	signal?: AbortSignal;
	filename?: string;
}

// ─────────────────────────────────────────────────────────────────────────
// Per-run input
// ─────────────────────────────────────────────────────────────────────────

/**
 * Per-execution input.
 *
 * `code` is always parsed as ECMAScript Module. Top-level `await` works.
 * Results come back through `export default` and named `export`s.
 */
export interface RunOptions {
	/**
	 * ESM source code to execute.
	 *
	 * @example
	 *   const res = await fetch("https://api.example.com/data");
	 *   const data = await res.json();
	 *   export default data;
	 */
	code: string;

	/**
	 * Resource ceilings for this run. Defaults are conservative but generous;
	 * production hosts should tighten them per use case.
	 */
	limits?: Partial<ResourceLimits>;

	/**
	 * Host-supplied globals. Only allowlisted names are accepted.
	 * Names outside the allowlist are a TypeScript-level error.
	 *
	 * The runtime itself owns `console`, `crypto`, `URL`, `TextEncoder`,
	 * `TextDecoder`, `atob`, `btoa`, and `structuredClone`. These cannot
	 * be overridden by the host.
	 */
	globals?: HostGlobals;

	/**
	 * Module resolver. Used for every `import` (static and dynamic) the
	 * user code performs.
	 */
	imports?: ImportsConfig;

	/**
	 * Optional abort signal. When aborted, the run terminates with
	 * `code: "ERR_ABORTED"`. Stdout/stderr captured up to that point are
	 * preserved.
	 */
	signal?: AbortSignal;

	/**
	 * Optional filename used in stack traces and `import.meta.url`.
	 * Default: `"<sandbox>"`.
	 */
	filename?: string;
}

// ─────────────────────────────────────────────────────────────────────────
// Resource limits
// ─────────────────────────────────────────────────────────────────────────

export interface ResourceLimits {
	/**
	 * Hard cap on memory the isolate can use, in megabytes.
	 * Covers the V8 heap and all external `ArrayBuffer` allocations.
	 *
	 * Exceeding this kills the isolate with `code: "ERR_MEMORY_LIMIT"`.
	 *
	 * @default 128
	 */
	memoryMb: number;

	/**
	 * Maximum *active* execution time in milliseconds. Time spent waiting on
	 * a host `fetch`, an awaited host-implemented import method, or any other
	 * bridge call is **not** counted. Tight loops in user code are.
	 *
	 * Exceeding this kills the isolate with `code: "ERR_CPU_TIMEOUT"`.
	 *
	 * @default 5_000
	 */
	cpuTimeMs: number;

	/**
	 * Hard wall-clock cap including async waits. Backstop against host
	 * implementations that never resolve.
	 *
	 * Exceeding this kills the isolate with `code: "ERR_WALL_TIMEOUT"`.
	 *
	 * @default 30_000
	 */
	wallTimeMs: number;

	/**
	 * Maximum total bytes of exported data (sum of default + named exports
	 * after V8 ValueSerializer encoding).
	 *
	 * Exceeding this fails with `code: "ERR_EXPORT_TOO_LARGE"`.
	 *
	 * @default 16 * 1024 * 1024
	 */
	maxExportBytes: number;

	/**
	 * Maximum bytes captured from `console.log` / `console.info`.
	 * Output past this point is dropped; existing output is kept.
	 *
	 * @default 1 * 1024 * 1024
	 */
	maxStdoutBytes: number;

	/**
	 * Maximum bytes captured from `console.error` / `console.warn`.
	 * Output past this point is dropped.
	 *
	 * @default 1 * 1024 * 1024
	 */
	maxStderrBytes: number;

	/**
	 * Maximum bytes per `fetch` response body. Larger responses fail with
	 * `code: "ERR_FETCH_BODY_TOO_LARGE"`.
	 *
	 * @default 16 * 1024 * 1024
	 */
	maxFetchBodyBytes: number;
}

// ─────────────────────────────────────────────────────────────────────────
// Globals (restricted allowlist)
// ─────────────────────────────────────────────────────────────────────────

/**
 * Host-providable globals. Only the names declared here are accepted; the
 * runtime owns everything else (`console`, `crypto`, `URL`, `TextEncoder`,
 * `TextDecoder`, `atob`, `btoa`, `structuredClone`, …).
 *
 * Adding a new global here is a deliberate design decision — see DESIGN.md
 * §4.2.
 */
export interface HostGlobals {
	/**
	 * Host implementation of `fetch`. The runtime exposes a Web-shaped
	 * `fetch` global to sandbox code; that global calls this function with
	 * a normalized, fully-resolved request descriptor.
	 *
	 * Returning rejects the sandbox-side `fetch`. Throwing rejects with the
	 * thrown error. Returning a `Response`-shaped value resolves it.
	 *
	 * Permission control is the host's responsibility: inspect `request.url`,
	 * `request.method`, etc. and either throw a permission error or perform
	 * the actual request.
	 */
	fetch?: FetchHandler;
}

/** Host-side fetch handler. */
export type FetchHandler = (
	request: HostFetchRequest,
) => Promise<HostFetchResponse> | HostFetchResponse;

/**
 * Normalized request as it crosses the bridge. All header names are
 * lower-cased. Bodies are serialized as bytes or string before crossing.
 */
export interface HostFetchRequest {
	url: string;
	method: string;
	/** Header names are lower-cased and deduped. */
	headers: Record<string, string>;
	/** `null` for bodyless methods (GET/HEAD). Strings are UTF-8 encoded. */
	body: Uint8Array | string | null;
	/** AbortSignal that fires when the sandbox's run is aborted or times out. */
	signal: AbortSignal;
}

/**
 * Host-side fetch response. The host can return either a real `Response`
 * shape or any object matching this interface.
 */
export interface HostFetchResponse {
	status: number;
	statusText?: string;
	/** Header names are lower-cased. Duplicate headers may be comma-joined. */
	headers: Record<string, string>;
	/**
	 * Response body. Strings are UTF-8 encoded inside the sandbox before
	 * being exposed as `Response.body` / `.text()` / `.json()`.
	 * `null` is interpreted as an empty body.
	 */
	body: Uint8Array | string | null;
}

// ─────────────────────────────────────────────────────────────────────────
// Imports (the extensibility surface)
// ─────────────────────────────────────────────────────────────────────────

/**
 * Configuration of how `import`/`import()` specifiers are resolved.
 *
 * Resolution order:
 *   1. `static[specifier]` — direct map lookup
 *   2. `resolve(specifier, importer)` — dynamic resolver, if defined
 *   3. throw `ERR_MODULE_NOT_FOUND`
 */
export interface ImportsConfig {
	/**
	 * Static map of specifier → import definition. Populated once at
	 * configuration time. Looked up by exact string match (no globbing,
	 * no path resolution).
	 */
	static?: Record<string, ImportDefinition>;

	/**
	 * Dynamic resolver. Called for every import that misses the static map.
	 * May be async. Return `null` to fall through to ModuleNotFound.
	 *
	 * `importer` is the absolute identifier of the importing module, or
	 * `null` for the entry module's own imports.
	 */
	resolve?: (
		specifier: string,
		importer: string | null,
	) => Promise<ImportDefinition | null> | ImportDefinition | null;
}

/** Discriminated union of how a single import is provided. */
export type ImportDefinition = SourceImport | HostImport;

/**
 * **Flavor B**: the host provides ESM source code. V8 compiles it once,
 * caches the compiled module, and evaluates it inside the isolate. Runs
 * with zero per-call bridge overhead.
 *
 * Source must be valid ESM (`export` statements). For CommonJS-only
 * packages, bundle to ESM with esbuild/rollup first.
 */
export interface SourceImport {
	kind: "source";
	/** ESM source code. */
	source: string;
}

/**
 * **Flavor A**: the host provides an object whose keys become the module's
 * named exports. Each function key becomes a sandbox-side stub that bridges
 * each call back to the host.
 *
 * Non-function values are exported as-is (frozen, deep-copied via
 * ValueSerializer at module instantiation).
 *
 * Constraints (see DESIGN.md §4.3):
 *   - Function values may only take and return data
 *     (V8 ValueSerializer-supported types).
 *   - Function arguments cannot themselves be functions. Calling a host
 *     export with a function argument fails with
 *     `code: "ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED"`.
 *   - Return values follow the same "data only" rule as exports — class
 *     methods are stripped, functions throw.
 *
 * The module's `default` export, if present, is provided via the
 * conventional `default` key.
 */
export interface HostImport {
	kind: "host";
	exports: HostExports;
}

/**
 * Map of export name → value or function.
 * The reserved key `"default"` becomes the module's `export default`.
 */
export interface HostExports {
	default?: HostExportValue;
	[name: string]: HostExportValue | undefined;
}

/**
 * What a host-implemented module can expose:
 *   - Plain data (frozen and serialized at module instantiation)
 *   - Functions (sync or async) that take and return data
 *
 * Function arguments and return values cross via V8 ValueSerializer.
 */
export type HostExportValue =
	| HostExportData
	| HostExportFunction;

/** Plain serializable data. */
export type HostExportData =
	| null
	| undefined
	| boolean
	| number
	| bigint
	| string
	| Uint8Array
	| HostExportData[]
	| { [key: string]: HostExportData };

/**
 * A host-implemented function. Must take serializable arguments and return
 * (or resolve to) a serializable value.
 *
 * Throwing rejects the sandbox-side call with an `Error` carrying the same
 * `message`, `name`, and `code` (if any).
 */
export type HostExportFunction = (
	...args: HostExportData[]
) => Promise<HostExportData> | HostExportData;

// ─────────────────────────────────────────────────────────────────────────
// Result
// ─────────────────────────────────────────────────────────────────────────

/**
 * Result of a `run()` call. Always returned (not thrown) for sandboxed
 * failures. Only infrastructure failures (Rust process crashed, IPC
 * broken) cause `run()` to reject.
 *
 * Discriminated on `ok`. Stdout/stderr/durationMs are always present.
 */
export type RunResult = RunSuccess | RunFailure;

export interface RunSuccess {
	ok: true;
	/**
	 * Serialized exports from the user's ESM module.
	 *   - `default` — value of `export default <expr>`, or `undefined`.
	 *   - `named`   — map of named exports (`export const x = …`).
	 */
	exports: SandboxExports;
	/** Anything written to `console.log` / `.info` / `.debug`. */
	stdout: string;
	/** Anything written to `console.error` / `.warn`. */
	stderr: string;
	/** Total wall-clock duration of the run, in milliseconds. */
	durationMs: number;
}

export interface RunFailure {
	ok: false;
	/** Structured failure information. See `RunErrorCode` for the categories. */
	error: RunError;
	/** Output captured before the failure. */
	stdout: string;
	stderr: string;
	durationMs: number;
}

export interface SandboxExports {
	/** Value of `export default <expr>` in user code, or `undefined`. */
	default: unknown;
	/** Map of named exports. Empty object if there are none. */
	named: Record<string, unknown>;
}

export interface RunError {
	/** Stable, machine-readable code. */
	code: RunErrorCode;
	/** Constructor name of the underlying JS Error (`"TypeError"` etc.) or `"Error"`. */
	name: string;
	/** Human-readable message. */
	message: string;
	/** V8 stack trace, when available. Stripped of host frames. */
	stack?: string;
}

export type RunErrorCode =
	/** User code threw an uncaught exception. */
	| "ERR_USER_CODE"
	/** Memory limit exceeded (V8 heap or external ArrayBuffer). */
	| "ERR_MEMORY_LIMIT"
	/** Active CPU time exceeded `limits.cpuTimeMs`. */
	| "ERR_CPU_TIMEOUT"
	/** Wall-clock time exceeded `limits.wallTimeMs`. */
	| "ERR_WALL_TIMEOUT"
	/** Caller aborted via `options.signal`. */
	| "ERR_ABORTED"
	/** An `import` specifier could not be resolved. */
	| "ERR_MODULE_NOT_FOUND"
	/** Source compilation failed (syntax error in user code or a source module). */
	| "ERR_COMPILE"
	/** A host module export function was called with a function argument. */
	| "ERR_FUNCTION_ARGUMENT_NOT_SUPPORTED"
	/** An export value contained a function or other non-serializable value. */
	| "ERR_EXPORT_NOT_SERIALIZABLE"
	/** Total export bytes exceeded `limits.maxExportBytes`. */
	| "ERR_EXPORT_TOO_LARGE"
	/** An unresolved Promise was found in the export graph. */
	| "ERR_EXPORT_UNRESOLVED_PROMISE"
	/** A `fetch` response body exceeded `limits.maxFetchBodyBytes`. */
	| "ERR_FETCH_BODY_TOO_LARGE"
	/** The host's `fetch` handler threw, or returned an invalid shape. */
	| "ERR_FETCH_HOST"
	/** Sandbox tried to call `fetch` but no host handler was configured. */
	| "ERR_FETCH_NOT_CONFIGURED"
	/** Header name or value contained illegal characters (CRLF, NUL, etc.). */
	| "ERR_FETCH_INVALID_HEADER"
	/** Request URL was not a valid http(s) URL. */
	| "ERR_FETCH_INVALID_URL"
	/** A bridge call (host module method) threw on the host side. */
	| "ERR_HOST_IMPORT"
	/** A prefix.run() tried to bind a global/import not declared at precompile time. */
	| "ERR_UNDECLARED_BINDING"
	/** Precompiled prefix has been disposed or evicted from the snapshot cache. */
	| "ERR_PREFIX_DISPOSED"
	/** Internal runtime invariant violated. File a bug. */
	| "ERR_INTERNAL";

// ─────────────────────────────────────────────────────────────────────────
// Convenience helpers (implemented later)
// ─────────────────────────────────────────────────────────────────────────

/**
 * One-shot convenience: create a runtime, run, dispose. For ephemeral use.
 * Not recommended in hot paths because it pays the Rust-process spawn cost
 * every call (~30–80ms). Use `createRuntime()` + `runtime.run()` for repeat use.
 */
export type Run = (options: RunOptions) => Promise<RunResult>;
