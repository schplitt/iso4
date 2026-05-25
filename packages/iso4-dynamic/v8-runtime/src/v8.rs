//! V8 isolate management and JavaScript execution.
//!
//! This module owns everything V8-related:
//! - initializing the V8 platform (once, at process start)
//! - creating an isolate with memory limits
//! - compiling and evaluating user-supplied ESM code
//! - capturing console output
//! - serializing exports back to raw bytes for the IPC layer

/// Execute a sandboxed run and return the serialized result bytes.
///
/// `payload` is the raw bytes from a `Run` IPC frame. Eventually this will
/// be a V8-serialized `RunOptions` struct that we decode inside V8 itself
/// (since only V8's ValueDeserializer understands its own wire format).
/// For now it is treated as opaque — we just acknowledge it.
///
/// The returned `Vec<u8>` will become the payload of the `Result` IPC frame
/// sent back to the TypeScript host. Eventually it will be a V8-serialized
/// `RunResult` (ok + exports, or error + code).
///
/// # What will happen here, step by step
///
/// 1. **Decode RunOptions** — use V8 ValueDeserializer on `payload` to
///    extract the code string, limits, globals config, imports config.
///
/// 2. **Create an Isolate** — `v8::Isolate::new()` with
///    `CreateParams::heap_limits(0, limits.memory_mb * MB)` so V8 enforces
///    the memory cap. A near-heap-limit callback will let us kill gracefully
///    before V8 hard-crashes.
///
/// 3. **Create a Context** — `v8::Context::new()` inside a HandleScope.
///    Install runtime-owned globals: `console` (captured), later `fetch`
///    (bridge stub). No Node builtins.
///
/// 4. **Compile the module** — `v8::Script::compile()` / `v8::Module::new()`
///    with the code string wrapped as ESM. Module specifier resolution will
///    look up imports from RunOptions.
///
/// 5. **Evaluate** — `module.evaluate()`. Top-level `await` works here
///    because we drive the V8 microtask queue manually after evaluate().
///    Any uncaught exception becomes an ERR_USER_CODE result.
///
/// 6. **Read exports** — walk `module.get_module_namespace()` and pull out
///    `default` plus any named exports.
///
/// 7. **Serialize exports** — `v8::ValueSerializer` turns each export value
///    into bytes. Functions or unresolved Promises fail with
///    ERR_EXPORT_NOT_SERIALIZABLE.
///
/// 8. **Return** — pack stdout, stderr, duration, and serialized exports
///    into a `RunResult` and serialize the whole thing for the IPC layer.
pub fn execute(_payload: &[u8]) -> Result<Vec<u8>, String> {
    // TODO: implement steps above.
    // Returning an error for now so the session layer can send a proper
    // ERR_INTERNAL result frame instead of an empty one.
    Err("v8::execute not yet implemented".to_string())
}
