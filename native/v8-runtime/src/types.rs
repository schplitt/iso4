//! Plain types shared by the engine and the socket frontend: limits, the
//! prefix shape (globals, imports, rebinds), calls, bridge records and run
//! completions. Depends on nothing else in the crate: no V8, no sockets, no
//! encoding.

/// Default resource limits. The runtime owns its safety posture: the TS client
/// sends only the limits the caller explicitly set (each wire field is
/// `Optional<u32>`), and any absent field is filled from these constants. An
/// explicit `0` on the wire is preserved as-is and disables that limit.
///
/// The public `ResourceLimits` jsdoc in `packages/iso4-sandbox/src/types.ts`
/// documents these same numbers, pointing here as the source of truth.
pub const DEFAULT_MEMORY_MB: u32 = 128;
pub const DEFAULT_CPU_TIME_MS: u32 = 5_000;
pub const DEFAULT_WALL_TIME_MS: u32 = 30_000;
pub const DEFAULT_MAX_EXPORT_BYTES: u32 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_STDOUT_BYTES: u32 = 1024 * 1024;
pub const DEFAULT_MAX_STDERR_BYTES: u32 = 1024 * 1024;
pub const DEFAULT_MAX_BRIDGE_CALL_BYTES: u32 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_BRIDGE_CALLS: u32 = 10;
/// Grace budget for `waitUntil` background work after the Result frame,
/// Cloudflare's number. Zero disables the epilogue entirely.
pub const DEFAULT_GRACE_MS: u32 = 30_000;

/// Streaming bodies: hard cap on one `StreamChunk`'s byte length. The host
/// splits larger reads; the runtime rejects a violation as malformed.
pub const STREAM_CHUNK_MAX_BYTES: u32 = 64 * 1024;
/// Streaming bodies: the credit window — how many un-consumed bytes may be in
/// flight per stream. Granted implicitly at hydration; replenished by
/// `StreamPull` as the sandbox consumes. Bounds runtime-side buffering.
pub const STREAM_CREDIT_WINDOW_BYTES: u32 = 256 * 1024;

/// Resource limits applied to a `Run` request, with runtime defaults already
/// resolved: each field is the caller's explicit value or, when the caller left
/// it unset on the wire, the corresponding `DEFAULT_*` constant. A field of
/// zero means the limit is explicitly disabled (no cap).
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// The retirement line: a warm instance whose heap sits above this at
    /// two consecutive finishes is retired (no run fails). Zero = none.
    pub soft_memory_mb: u32,
    /// The terminating V8 cap (`heap_limits` + the near-heap callback) and
    /// the ArrayBuffer budget. Zero = uncapped.
    pub hard_memory_mb: u32,
    pub cpu_time_ms: u32,
    pub wall_time_ms: u32,
    pub max_export_bytes: u32,
    pub max_stdout_bytes: u32,
    pub max_stderr_bytes: u32,
    /// Maximum bytes the sandbox may send as arguments in a single bridge call
    /// (sandbox → host). Zero means no per-call cap.
    pub max_bridge_call_bytes: u32,
    /// Maximum number of bridge calls (globals + host imports combined) allowed
    /// per run. Zero means no limit.
    pub max_bridge_calls: u32,
    /// Wall budget for `waitUntil` background work after the Result frame
    ///. Zero disables the epilogue: registered work dies at settle.
    pub grace_ms: u32,
}

/// Headroom above the advertised cap before the terminating line: the
/// geometric mean of the cap and 8 MB (8 → +8, 128 → +32, 512 → +64).
/// Monotonic, so a bigger cap never yields a smaller ceiling.
pub fn heap_band_mb(cap_mb: u32) -> u32 {
    if cap_mb == 0 {
        return 0;
    }
    let band = ((8.0 * f64::from(cap_mb)).sqrt()).round();
    band as u32
}

/// Whether an absent hard override earns a headroom band — the band is for
/// reuse, so a one-off (nothing to retire) does not get one. DESIGN §4.1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BandPolicy {
    /// Reused instances: cap is the soft line, a band above it terminates.
    Managed,
    /// Fresh isolate per run: the cap terminates, no soft line.
    Strict,
}

/// Resolve the advertised cap plus optional hard override into the two
/// enforced lines. Truth table: `docs/protocol.md` §5.2.
pub(crate) fn resolve_heap_lines(
    cap_mb: u32,
    hard_override: Option<u32>,
    band: BandPolicy,
) -> (u32, u32) {
    match (hard_override, band) {
        // No soft line on a one-off: nothing is ever retired there.
        (None, BandPolicy::Strict) => (0, cap_mb),
        (None, BandPolicy::Managed) => (cap_mb, cap_mb.saturating_add(heap_band_mb(cap_mb))),
        (Some(0), _) => (0, 0),
        (Some(hard), BandPolicy::Strict) => (0, hard),
        (Some(hard), BandPolicy::Managed) if cap_mb >= hard => (0, hard),
        (Some(hard), BandPolicy::Managed) => (cap_mb, hard),
    }
}

impl Default for ResourceLimits {
    /// The runtime defaults — the posture applied when the client sends no
    /// explicit limits at all.
    fn default() -> Self {
        Self {
            soft_memory_mb: DEFAULT_MEMORY_MB,
            hard_memory_mb: DEFAULT_MEMORY_MB + heap_band_mb(DEFAULT_MEMORY_MB),
            cpu_time_ms: DEFAULT_CPU_TIME_MS,
            wall_time_ms: DEFAULT_WALL_TIME_MS,
            max_export_bytes: DEFAULT_MAX_EXPORT_BYTES,
            max_stdout_bytes: DEFAULT_MAX_STDOUT_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            max_bridge_call_bytes: DEFAULT_MAX_BRIDGE_CALL_BYTES,
            max_bridge_calls: DEFAULT_MAX_BRIDGE_CALLS,
            grace_ms: DEFAULT_GRACE_MS,
        }
    }
}

/// A host global the sandbox is allowed to reference, tagged by how the
/// runtime installs it natively. Mirrors `GlobalDefPayload` on the TS side
/// (`ipc.ts`); see `docs/protocol.md` §5.2.
/// Every variant carries `enumerable`: whether the installed public name
/// shows up in `for...in` / `Object.keys`. Host shorthands always send
/// `true`; the object global forms may opt out. Runtime-internal names
/// (`__iso4_*`) install non-enumerable regardless.
#[derive(Debug, Clone)]
pub enum HostGlobalDef {
    /// A plain host function — installed as a bridge stub under `name`.
    Bridge { name: String, enumerable: bool },
    /// A JS expression the runtime evaluates as its own script and sets on
    /// `globalThis[name]`.
    StringExpr {
        name: String,
        expr: String,
        enumerable: bool,
    },
    /// A constant carried as a V8 serialization blob, materialised natively.
    Data {
        name: String,
        blob: Vec<u8>,
        enumerable: bool,
    },
    /// A bridge handler (installed as a stub under `handler_name`) plus a shim
    /// expression the runtime wraps and sets on `globalThis[name]`.
    Shim {
        name: String,
        shim: String,
        handler_name: String,
        enumerable: bool,
    },
}

impl HostGlobalDef {
    /// The wire-level bridge stub name to install for this def, if any. Plain
    /// bridge globals install under their own name; shims install a hidden
    /// handler stub under `handler_name`. String/data globals need no stub.
    ///
    /// This is also the set of names that a `PrefixRun` may re-install and that
    /// the `ERR_UNDECLARED_BINDING` check validates against.
    pub fn bridge_stub_name(&self) -> Option<&str> {
        match self {
            HostGlobalDef::Bridge { name, .. } => Some(name),
            HostGlobalDef::Shim { handler_name, .. } => Some(handler_name),
            HostGlobalDef::StringExpr { .. } | HostGlobalDef::Data { .. } => None,
        }
    }

    /// Whether the installed public name is enumerable.
    pub fn enumerable(&self) -> bool {
        match self {
            HostGlobalDef::Bridge { enumerable, .. }
            | HostGlobalDef::StringExpr { enumerable, .. }
            | HostGlobalDef::Data { enumerable, .. }
            | HostGlobalDef::Shim { enumerable, .. } => *enumerable,
        }
    }

    /// A convenience constructor for tests: a plain bridge global.
    #[cfg(test)]
    pub fn bridge(name: &str) -> Self {
        HostGlobalDef::Bridge {
            name: name.to_string(),
            enumerable: true,
        }
    }
}

/// Deepest host-module shape node accepted, as a path length from a top-level
/// export. Mirrors the TS client's constant (`docs/protocol.md` §5.2).
pub(crate) const MAX_HOST_MODULE_DEPTH: usize = 64;

/// One node in a host-module shape tree. The tree is plain data — the client
/// never generates sandbox source from it; the runtime builds the module
/// natively (see `v8.rs`). Mirrors `HostModuleNodePayload` on the TS side.
#[derive(Debug, Clone)]
pub enum HostModuleNode {
    /// A host function leaf. The runtime assigns it a handle ID (tree-walk
    /// order over the declared bindings) and installs an async trampoline
    /// that dispatches through the reserved `__iso4_call` bridge stub.
    Function,
    /// A constant data leaf carried as a V8 serialization blob, materialised
    /// natively via `blob::deserialize_value`.
    Data(Vec<u8>),
    /// A nested object of named children (may mix functions and data).
    Object(Vec<(String, HostModuleNode)>),
}

/// The two flavors of import module per DESIGN.md §4.3, now discriminated on
/// the wire instead of being lowered to source text by the client.
#[derive(Debug, Clone)]
pub enum ImportModule {
    /// Host-provided ESM source, compiled verbatim inside the isolate.
    Source(String),
    /// A host-module shape: named top-level exports, each a tree of function
    /// leaves and data leaves. The runtime builds the module from this data.
    Host(Vec<(String, HostModuleNode)>),
}

/// One entry in the `imports` list of a `Run` / `Precompile` request.
#[derive(Debug, Clone)]
pub struct ImportBinding {
    pub specifier: String,
    pub module: ImportModule,
}

/// One host-import function-leaf rebinding requested by a `PrefixRun`. Only
/// the location crosses the wire — the replacement handler stays on the TS
/// side (bridge dispatch is name-addressed). The runtime validates each entry
/// against the shape declared at `Precompile` and rejects undeclared locations
/// with `ERR_UNDECLARED_BINDING`, frozen ones with `ERR_FROZEN_BINDING`.
#[derive(Debug, Clone)]
pub struct ImportRebind {
    pub specifier: String,
    /// Key segments of the function leaf inside the host module; a key may
    /// itself contain dots.
    pub path: Vec<String>,
}

/// A host → sandbox function call requested by a `Run` / `PrefixRun` frame
///. `export_path` addresses a callable relative to the module's exports
/// (`"named"`, `"default.fetch"`); `args_blob` is **one** V8 serialization
/// blob holding an array of arguments — the same convention as `BridgeCall`
/// args, identity between arguments preserved.
#[derive(Debug)]
pub struct CallSpec {
    pub export_path: String,
    pub args_blob: Vec<u8>,
}

/// How a `Terminate` stops its run: soft abandons it at the next turn
/// boundary, hard interrupts executing JS now (`docs/protocol.md` §5.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TerminateMode {
    Soft = 0,
    Hard = 1,
}

/// Structured error fields for `RunCompletionPayload`.
#[derive(Clone)]
pub struct RunErrorPayload {
    pub code: String,
    pub name: String,
    pub message: String,
    pub stack: Option<String>,
    /// Own-enumerable properties beyond `name`/`message`/`stack`, as a V8
    /// serialization blob holding a plain object. `None` when there are none.
    pub fields: Option<Vec<u8>>,
    /// Present only for `ERR_INSTANCE_RESET`: the cause class and the wire
    /// run id of the run whose mid-JS interruption reset the instance.
    pub reset: Option<ResetInfo>,
    /// True when the failure happened while the prefix evaluated (a cold
    /// start that broke the prefix), false for run code.
    pub prefix_stage: bool,
    /// Present only for `ERR_PREFIX_BROKEN`: the stored cold-start failure.
    /// Only `code`, `name`, `message` and `stack` cross.
    pub cause: Option<std::sync::Arc<RunErrorPayload>>,
}

/// Why a shared instance was reset under an innocent victim — the cause
/// class carried on `ERR_INSTANCE_RESET`. Wire byte values are assigned in
/// `wire::reset_cause_byte` (NOT this declaration order) and mirrored by the
/// TS decoder's cause table; change all three together or victims get
/// mislabelled causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCause {
    Cpu,
    Memory,
    /// The culprit's wall deadline fired mid-turn (possible when its CPU is
    /// uncapped — the guard enforces min(cpu, wall) inside a turn).
    Wall,
    Abort,
    Internal,
    /// The culprit used something the runtime does not support (today:
    /// dynamic `import()`), so the runtime ended it.
    Unsupported,
}

/// The `ERR_INSTANCE_RESET` extras — see `RunErrorPayload::reset`.
#[derive(Clone, Copy)]
pub struct ResetInfo {
    pub cause: ResetCause,
    pub culprit_run_id: u32,
}

/// Per-call metadata for one bridge call attempt — names, timing, and sizes
/// only, never payloads. Recorded by the runtime (`v8.rs`) and shipped on the
/// Result frame with the name already resolved — the runtime owns both the
/// import handle table and the shim naming convention, so no client-side
/// resolution remains.
#[derive(Debug, Clone)]
pub struct BridgeCallRecord {
    /// The public name sandbox code called: plain globals as-is (`fetch`),
    /// shimmed globals under their public name (not the private stub), and
    /// host-module import leaves as `<specifier>.<path>`.
    pub name: String,
    /// Offset from run start to the attempt, in ms (same clock as
    /// `duration_ms` on the run result).
    pub start_ms: f64,
    /// Round-trip time the sandbox waited (handler + IPC), in ms. For calls
    /// still unanswered when the run ended: time until run end. `0` for
    /// blocked attempts.
    pub duration_ms: f64,
    /// Serialized call payload size in bytes (the size `maxBridgeCallBytes`
    /// is enforced against). `0` for attempts blocked before serialisation.
    pub arg_bytes: u32,
    /// Serialized response value size in bytes. `0` on handler error or when
    /// the call never settled.
    pub response_bytes: u32,
    /// The callId the BridgeCall frame carried; `u32::MAX` for blocked
    /// attempts, which never got one.
    pub call_id: u32,
    pub outcome: BridgeCallOutcome,
}

/// How a bridge call attempt ended, as the runtime saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BridgeCallOutcome {
    /// The host resolved and the response reached the sandbox.
    Ok = 0,
    /// Refused runtime-side (limit, oversized payload, function argument,
    /// invalid import handle, transport failure); never sent.
    Blocked = 1,
    /// The host answered with an error; the sandbox promise rejected.
    Error = 2,
    /// Sent, but no answer before the run ended.
    Unanswered = 3,
}

/// Payload for a successful run.
pub struct RunSuccessPayload {
    /// All exports as **one** V8 serialization blob holding a plain
    /// `{ name: value }` object. The `default` export (if present) is the
    /// `"default"` key alongside named exports; an empty module produces a
    /// blob holding `{}`. For a run that carried a `call` this is the
    /// called function's return value instead — the host knows which it asked
    /// for, so the slot needs no tag.
    pub exports: Vec<u8>,
    /// Export names absent from `exports` because their value cannot cross
    /// the boundary. Always empty for a call run.
    pub skipped_exports: Vec<String>,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: f64,
    /// The run's own logic time: execution + async waits counted until the
    /// answer arrived; engine time excluded. `cpu ≤ wall ≤ duration`.
    pub wall_time_ms: f64,
    /// Active V8 execution time (bridge waits excluded), in ms.
    pub cpu_time_ms: f64,
    /// One record per bridge call attempt, in attempt order.
    pub bridge_calls: Vec<BridgeCallRecord>,
    /// `used_heap_size` of the isolate that served the run, measured after
    /// the run settled. Present for prefix runs (warm instances —
    /// feeds eviction scoring); absent for one-off runs, whose isolate
    /// is already gone.
    pub heap_used_bytes: Option<u64>,
    /// Post-Result flags: bit 0 = `waitUntil` background work is still
    /// running, bit 1 = outbound stream bodies are open. Nonzero means a
    /// final `RunComplete` frame follows this Result on the same
    /// connection (wire-compatible with the former bool).
    pub background_flags: u8,
}

/// Payload for a failed run.
pub struct RunFailurePayload {
    pub error: RunErrorPayload,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: f64,
    /// See `RunSuccessPayload::wall_time_ms`.
    pub wall_time_ms: f64,
    /// Active V8 execution time (bridge waits excluded), in ms.
    pub cpu_time_ms: f64,
    /// One record per bridge call attempt, in attempt order.
    pub bridge_calls: Vec<BridgeCallRecord>,
    /// See `RunSuccessPayload::heap_used_bytes`.
    pub heap_used_bytes: Option<u64>,
}

/// The two variants of a completed run.
pub enum RunCompletion {
    Success(RunSuccessPayload),
    Failure(RunFailurePayload),
}

/// What a `BridgeCall` targets: a global by name, or a host-module function
/// leaf by specifier and key segments.
pub enum BridgeTarget<'a> {
    Global(&'a str),
    Import {
        specifier: &'a str,
        path: &'a [String],
    },
}
