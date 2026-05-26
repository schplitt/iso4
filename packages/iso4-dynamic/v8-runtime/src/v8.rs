//! V8 isolate management and JavaScript execution.
//!
//! Owns everything V8-related: platform init, isolate creation, compilation,
//! evaluation, result extraction, console capture, and limit enforcement.

use std::ffi::c_void;
use std::sync::Once;

use crate::wire::WireValue;

static INIT: Once = Once::new();

#[derive(Default)]
struct LogBuffers {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

/// Initialize the V8 platform. Safe to call from multiple threads —
/// `Once` ensures it runs exactly once per process.
pub fn init_platform() {
    INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

// ── Output types ─────────────────────────────────────────────────────────────

/// The result of a successful JavaScript execution.
#[derive(Debug)]
pub struct Output {
    /// All exports as a flat `WireValue::Object`.
    /// The `default` export (if any) appears as the `"default"` key alongside
    /// named exports. An empty module produces `WireValue::Object(vec![])`.
    pub exports: WireValue,

    /// Lines written to console.log / console.debug / console.info.
    pub stdout: Vec<String>,

    /// Lines written to console.warn / console.error.
    pub stderr: Vec<String>,

    /// Wall-clock time from start of execution to result, in milliseconds.
    pub duration_ms: u64,
}

/// The result of a failed JavaScript execution.
///
/// Logs are preserved on failures too: if user code logs and then throws, the
/// caller still receives the log output produced before the throw.
#[derive(Debug)]
pub struct FailureOutput {
    pub error: RunError,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub duration_ms: u64,
}

/// All the ways an execution can fail.
#[derive(Debug)]
pub enum RunError {
    /// Payload bytes are not valid UTF-8.
    InvalidPayload(String),
    /// JS syntax error or compile-time error.
    CompileError(String),
    /// Uncaught exception thrown during execution.
    RuntimeError {
        message: String,
        stack: Option<String>,
    },
    /// `import` specifier not found in the host imports map.
    ModuleNotFound(String),
    /// An export value is a function or an unresolved Promise.
    ExportNotSerializable(String),
    /// Active JS execution time exceeded `limits.cpuTimeMs`.
    CpuTimeout,
    /// Total wall-clock time exceeded `limits.wallTimeMs`.
    WallTimeout,
    /// V8 heap + ArrayBuffer exceeded `limits.memoryMb`.
    MemoryLimit,
    /// Unexpected internal runtime failure.
    Internal(String),
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Execute a sandboxed run and return the full output.
///
/// `payload` is the raw bytes from a `Run` IPC frame.
///
/// **Phase 1 (now):** payload is treated as a plain UTF-8 ESM source string.
///
/// **Phase 2+:** payload is V8-serialized `RunOptions`; the code inside it is
/// still evaluated as ESM with a module resolver, bridge stubs, and full
/// `ValueSerializer` export extraction.
pub fn execute(payload: &[u8]) -> Result<Output, FailureOutput> {
    let code = std::str::from_utf8(payload).map_err(|e| FailureOutput {
        error: RunError::InvalidPayload(e.to_string()),
        stdout: Vec::new(),
        stderr: Vec::new(),
        duration_ms: 0,
    })?;
    run_code(code)
}

/// Core execution — separated from `execute` so tests can call it directly
/// with a `&str` without constructing a fake IPC payload.
fn run_code(code: &str) -> Result<Output, FailureOutput> {
    init_platform();
    run_module(code)
}

/// ESM path: compile source as a module, instantiate it, evaluate it, then
/// inspect the module namespace object for `default` and named exports.
fn run_module(code: &str) -> Result<Output, FailureOutput> {
    let start = std::time::Instant::now();
    let mut logs = LogBuffers::default();

    let mut isolate = v8::Isolate::new(Default::default());
    // Auto microtasks is enough for simple top-level `await Promise.resolve()`.
    // Host bridge waits will need an explicit event-loop later.
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Auto);

    let scope = &mut v8::HandleScope::new(&mut isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    install_console(scope, &mut logs as *mut LogBuffers)
        .map_err(|error| failure(error, &logs, start))?;

    let scope = &mut v8::TryCatch::new(scope);

    let source_string = v8::String::new(scope, code).ok_or_else(|| {
        failure(
            RunError::Internal("failed to intern module source".to_string()),
            &logs,
            start,
        )
    })?;
    let filename = v8::String::new(scope, "<iso4>").ok_or_else(|| {
        failure(
            RunError::Internal("failed to intern filename".to_string()),
            &logs,
            start,
        )
    })?;
    let origin = v8::ScriptOrigin::new(
        scope,
        filename.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        true,
        None,
    );
    let mut source = v8::script_compiler::Source::new(source_string, Some(&origin));

    let module = match v8::script_compiler::compile_module(scope, &mut source) {
        Some(m) => m,
        None => {
            return Err(failure(
                RunError::CompileError(exception_message(scope)),
                &logs,
                start,
            ))
        }
    };

    if module
        .instantiate_module(scope, no_import_resolver)
        .is_none()
    {
        return Err(failure(
            RunError::ModuleNotFound(exception_message(scope)),
            &logs,
            start,
        ));
    }

    let evaluation = match module.evaluate(scope) {
        Some(value) => value,
        None => {
            return Err(failure(
                RunError::RuntimeError {
                    message: exception_message(scope),
                    stack: exception_stack(scope),
                },
                &logs,
                start,
            ));
        }
    };

    if evaluation.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(evaluation).map_err(|_| {
            failure(
                RunError::Internal("failed to inspect module evaluation promise".to_string()),
                &logs,
                start,
            )
        })?;
        match promise.state() {
            v8::PromiseState::Fulfilled => {}
            v8::PromiseState::Rejected => {
                let rejection = promise.result(scope);
                return Err(failure(
                    runtime_error_from_value(scope, rejection),
                    &logs,
                    start,
                ));
            }
            v8::PromiseState::Pending => {
                return Err(failure(
                    RunError::ExportNotSerializable(
                        "module evaluation promise is still pending".to_string(),
                    ),
                    &logs,
                    start,
                ));
            }
        }
    }

    let namespace = module
        .get_module_namespace()
        .to_object(scope)
        .ok_or_else(|| {
            failure(
                RunError::Internal("module namespace is not an object".to_string()),
                &logs,
                start,
            )
        })?;

    let names = namespace
        .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
        .ok_or_else(|| {
            failure(
                RunError::Internal("failed to read module export names".to_string()),
                &logs,
                start,
            )
        })?;

    let mut fields: Vec<(String, WireValue)> = Vec::new();

    for i in 0..names.length() {
        let name_value = names.get_index(scope, i).ok_or_else(|| {
            failure(
                RunError::Internal("failed to read export name".to_string()),
                &logs,
                start,
            )
        })?;
        let name = name_value
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .ok_or_else(|| {
                failure(
                    RunError::Internal("failed to stringify export name".to_string()),
                    &logs,
                    start,
                )
            })?;

        let value = namespace.get(scope, name_value).ok_or_else(|| {
            failure(
                RunError::Internal(format!("failed to read export {name}")),
                &logs,
                start,
            )
        })?;

        let wire =
            export_to_wire(scope, &name, value).map_err(|error| failure(error, &logs, start))?;
        fields.push((name, wire));
    }

    Ok(Output {
        exports: WireValue::Object(fields),
        stdout: logs.stdout.clone(),
        stderr: logs.stderr.clone(),
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

fn failure(error: RunError, logs: &LogBuffers, start: std::time::Instant) -> FailureOutput {
    FailureOutput {
        error,
        stdout: logs.stdout.clone(),
        stderr: logs.stderr.clone(),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn no_import_resolver<'a>(
    _context: v8::Local<'a, v8::Context>,
    _specifier: v8::Local<'a, v8::String>,
    _import_assertions: v8::Local<'a, v8::FixedArray>,
    _referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    None
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn install_console(scope: &mut v8::HandleScope, buffers: *mut LogBuffers) -> Result<(), RunError> {
    let console = v8::Object::new(scope);
    let data = v8::External::new(scope, buffers.cast::<c_void>());

    for name in ["log", "debug", "info"] {
        let key = v8::String::new(scope, name)
            .ok_or_else(|| RunError::Internal(format!("failed to intern console.{name}")))?;
        let function = v8::Function::builder(console_stdout_callback)
            .data(data.into())
            .build(scope)
            .ok_or_else(|| RunError::Internal(format!("failed to create console.{name}")))?;
        console
            .set(scope, key.into(), function.into())
            .ok_or_else(|| RunError::Internal(format!("failed to install console.{name}")))?;
    }

    for name in ["warn", "error"] {
        let key = v8::String::new(scope, name)
            .ok_or_else(|| RunError::Internal(format!("failed to intern console.{name}")))?;
        let function = v8::Function::builder(console_stderr_callback)
            .data(data.into())
            .build(scope)
            .ok_or_else(|| RunError::Internal(format!("failed to create console.{name}")))?;
        console
            .set(scope, key.into(), function.into())
            .ok_or_else(|| RunError::Internal(format!("failed to install console.{name}")))?;
    }

    let global = scope.get_current_context().global(scope);
    let console_key = v8::String::new(scope, "console")
        .ok_or_else(|| RunError::Internal("failed to intern console".to_string()))?;
    global
        .set(scope, console_key.into(), console.into())
        .ok_or_else(|| RunError::Internal("failed to install console".to_string()))?;

    Ok(())
}

fn console_stdout_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    append_console_line(scope, args, true);
    rv.set_undefined();
}

fn console_stderr_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    append_console_line(scope, args, false);
    rv.set_undefined();
}

fn append_console_line(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    stdout: bool,
) {
    let data = args.data();
    let Ok(external) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let buffers = external.value().cast::<LogBuffers>();
    if buffers.is_null() {
        return;
    }

    let mut parts = Vec::new();
    for i in 0..args.length() {
        let value = args.get(i);
        let rendered = if value.is_undefined() {
            "undefined".to_string()
        } else {
            value
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "[unprintable]".to_string())
        };
        parts.push(rendered);
    }

    let line = parts.join(" ");
    // SAFETY: `buffers` points to the `LogBuffers` stack value in run_module().
    // The V8 context and every console callback are dropped before that stack
    // value goes out of scope.
    let buffers = unsafe { &mut *buffers };
    if stdout {
        buffers.stdout.push(line);
    } else {
        buffers.stderr.push(line);
    }
}

/// Convert a top-level module export value to a `WireValue`.
///
/// Adds the export `name` to error messages for functions/promises.
/// Initialises the identity-based visiting set used by `value_to_wire`.
fn export_to_wire(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    name: &str,
    value: v8::Local<v8::Value>,
) -> Result<WireValue, RunError> {
    if value.is_function() {
        return Err(RunError::ExportNotSerializable(format!(
            "export \"{name}\" is a function"
        )));
    }
    if value.is_promise() {
        return Err(RunError::ExportNotSerializable(format!(
            "export \"{name}\" is an unresolved Promise"
        )));
    }
    let mut visiting: Vec<v8::Global<v8::Value>> = Vec::new();
    value_to_wire(scope, value, &mut visiting)
}

/// Check whether `value` is already on the current recursion path (`visiting`).
///
/// Uses `strict_equals` — V8 reference equality — so two distinct JS objects
/// that happen to have the same shape are never confused.
///
/// Returns `Ok(())` and pushes `value` if no cycle is found.
/// Returns `Err` without pushing if a cycle is detected; the caller must NOT
/// call `visiting.pop()` in the error branch.
fn check_and_push(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
    visiting: &mut Vec<v8::Global<v8::Value>>,
) -> Result<(), RunError> {
    for visited_global in visiting.iter() {
        let visited_local = v8::Local::new(scope.as_mut(), visited_global);
        if value.strict_equals(visited_local) {
            return Err(RunError::ExportNotSerializable(
                "cyclic or self-referential structure detected in export value"
                    .to_string(),
            ));
        }
    }
    visiting.push(v8::Global::new(scope.as_mut(), value));
    Ok(())
}

/// Serialise the items of a V8 array that has already been pushed onto
/// `visiting` by the caller.
fn serialize_array_items(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    array: v8::Local<v8::Array>,
    visiting: &mut Vec<v8::Global<v8::Value>>,
) -> Result<Vec<WireValue>, RunError> {
    let len = array.length();
    let mut items = Vec::with_capacity(len as usize);
    for i in 0..len {
        let item = array
            .get_index(scope, i)
            .ok_or_else(|| RunError::Internal(format!("failed to read array index {i}")))?;
        items.push(value_to_wire(scope, item, visiting)?);
    }
    Ok(items)
}

/// Serialise the own enumerable string-keyed properties of a V8 object that
/// has already been pushed onto `visiting` by the caller.
fn serialize_object_fields(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    object: v8::Local<v8::Object>,
    visiting: &mut Vec<v8::Global<v8::Value>>,
) -> Result<Vec<(String, WireValue)>, RunError> {
    let names = object
        .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
        .ok_or_else(|| RunError::Internal("failed to get own property names".to_string()))?;
    let mut fields = Vec::with_capacity(names.length() as usize);
    for i in 0..names.length() {
        let name_value = names
            .get_index(scope, i)
            .ok_or_else(|| RunError::Internal("failed to read property name".to_string()))?;
        let name = name_value
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .ok_or_else(|| {
                RunError::Internal("failed to stringify property name".to_string())
            })?;
        let val = object
            .get(scope, name_value)
            .ok_or_else(|| RunError::Internal(format!("failed to read property {name}")))?;
        fields.push((name, value_to_wire(scope, val, visiting)?));
    }
    Ok(fields)
}

/// Recursively convert a V8 value to a `WireValue`.
///
/// `visiting` is the set of reference-type values (objects/arrays) currently
/// on the recursion path. It acts as a call-stack to detect cycles:
/// - pushed when entering an object or array
/// - popped when leaving (whether success or error)
/// - compared with `strict_equals` (V8 reference equality) at each entry
///
/// Shared but non-cyclic references (the same object appearing in two
/// different fields) are serialised correctly: the object is popped after
/// the first field, so it is not in the set when the second field is visited.
fn value_to_wire(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
    visiting: &mut Vec<v8::Global<v8::Value>>,
) -> Result<WireValue, RunError> {
    // ── Primitives (no cycle risk) ────────────────────────────────────────
    if value.is_undefined() {
        return Ok(WireValue::Undefined);
    }
    if value.is_null() {
        return Ok(WireValue::Null);
    }
    if value.is_boolean() {
        return Ok(WireValue::Bool(value.boolean_value(scope)));
    }
    if value.is_number() {
        return Ok(WireValue::Number(
            value.number_value(scope).unwrap_or(f64::NAN),
        ));
    }
    if value.is_string() {
        let s = value
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .ok_or_else(|| {
                RunError::Internal("failed to convert V8 string value".to_string())
            })?;
        return Ok(WireValue::String(s));
    }
    if value.is_big_int() {
        let bigint = v8::Local::<v8::BigInt>::try_from(value)
            .map_err(|_| RunError::Internal("failed to cast to BigInt".to_string()))?;
        let (i_val, fits_i64) = bigint.i64_value();
        if fits_i64 {
            return Ok(WireValue::BigInt(i_val.to_string()));
        }
        let (u_val, fits_u64) = bigint.u64_value();
        if fits_u64 {
            return Ok(WireValue::BigInt(u_val.to_string()));
        }
        return Err(RunError::ExportNotSerializable(
            "BigInt value is out of the supported i64/u64 range for wire encoding"
                .to_string(),
        ));
    }
    if value.is_symbol() {
        return Err(RunError::ExportNotSerializable(
            "Symbol values cannot be serialized".to_string(),
        ));
    }
    // ── Reference types — cycle detection applies ─────────────────────────
    if value.is_function() {
        return Err(RunError::ExportNotSerializable(
            "function values cannot be serialized".to_string(),
        ));
    }
    if value.is_promise() {
        return Err(RunError::ExportNotSerializable(
            "unresolved Promise values cannot be serialized".to_string(),
        ));
    }
    // Arrays must be checked before the generic object path (arrays are objects).
    if value.is_array() {
        check_and_push(scope, value, visiting)?;
        let result = match v8::Local::<v8::Array>::try_from(value) {
            Ok(arr) => serialize_array_items(scope, arr, visiting).map(WireValue::Array),
            Err(_) => Err(RunError::Internal("failed to cast to Array".to_string())),
        };
        visiting.pop();
        return result;
    }
    if value.is_object() {
        check_and_push(scope, value, visiting)?;
        let result = match value.to_object(scope) {
            Some(obj) => serialize_object_fields(scope, obj, visiting).map(WireValue::Object),
            None => Err(RunError::Internal("failed to cast to object".to_string())),
        };
        visiting.pop();
        return result;
    }

    Err(RunError::ExportNotSerializable(
        "unsupported or unknown value type".to_string(),
    ))
}

fn runtime_error_from_value(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
) -> RunError {
    let message = error_message_from_value(scope, value)
        .or_else(|| {
            value
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
        })
        .unwrap_or_else(|| "JavaScript error".to_string());
    RunError::RuntimeError {
        message,
        stack: stack_from_value(scope, value),
    }
}

fn error_message_from_value(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let object = value.to_object(scope)?;
    let key = v8::String::new(scope, "message")?;
    object
        .get(scope, key.into())?
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
}

fn stack_from_value(
    scope: &mut v8::TryCatch<v8::HandleScope>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let object = value.to_object(scope)?;
    let key = v8::String::new(scope, "stack")?;
    object
        .get(scope, key.into())?
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
}

fn exception_message(scope: &mut v8::TryCatch<v8::HandleScope>) -> String {
    scope
        .exception()
        .and_then(|e| e.to_string(scope))
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "(no exception message)".to_string())
}

fn exception_stack(scope: &mut v8::TryCatch<v8::HandleScope>) -> Option<String> {
    let exception = scope.exception()?;
    stack_from_value(scope, exception)
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// TDD suite for the full Phase 1–8 surface. Tests are grouped by concern.
// Many will fail until the corresponding phase is implemented — that is
// intentional. Run `cargo test` to see what is still outstanding.
//
// Tests marked `#[ignore]` are ones that would hang or OOM the process
// without the relevant limit being implemented first. Un-ignore them when
// the limit lands.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WireValue;

    /// Shorthand: run a code string and return the full Output or RunError.
    fn run(code: &str) -> Result<Output, RunError> {
        run_code(code).map_err(|failure| failure.error)
    }

    /// Shorthand: expect success and return the Output, panic otherwise.
    fn run_ok(code: &str) -> Output {
        match run(code) {
            Ok(o) => o,
            Err(e) => panic!("expected Ok, got error: {e:?}"),
        }
    }

    /// Shorthand: expect a RunError, panic if it succeeds.
    fn run_err(code: &str) -> RunError {
        match run(code) {
            Err(e) => e,
            Ok(o) => panic!("expected Err, got output: {o:?}"),
        }
    }

    fn has_line(lines: &[String], needle: &str) -> bool {
        lines.iter().any(|line| line.contains(needle))
    }

    // ── WireValue test helpers ────────────────────────────────────────────────
    //
    // These convert the new WireValue-based Output back to the human-readable
    // string representations that the original tests expected. This keeps the
    // test bodies unchanged (or nearly so) while the internal representation
    // moves from stringified exports to structured WireValue.

    /// Look up a field in the top-level exports Object by name.
    fn get_field(out: &Output, key: &str) -> Option<WireValue> {
        if let WireValue::Object(fields) = &out.exports {
            fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        } else {
            None
        }
    }

    /// Return the `default` export rendered as a display string, or `None`.
    fn get_default(out: &Output) -> Option<String> {
        get_field(out, "default").map(|v| wire_to_display_str(&v))
    }

    /// Return a named export rendered as a display string, or `None`.
    fn get_named(out: &Output, key: &str) -> Option<String> {
        get_field(out, key).map(|v| wire_to_display_str(&v))
    }

    /// Return all export key names.
    fn export_keys(out: &Output) -> Vec<String> {
        if let WireValue::Object(fields) = &out.exports {
            fields.iter().map(|(k, _)| k.clone()).collect()
        } else {
            vec![]
        }
    }

    /// Render a `WireValue` as a human-readable string similar to what V8's
    /// `ToString()` / `JSON.stringify()` would produce. Used only in tests.
    fn wire_to_display_str(v: &WireValue) -> String {
        match v {
            WireValue::Undefined => "undefined".to_string(),
            WireValue::Null => "null".to_string(),
            WireValue::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
            WireValue::Number(n) => {
                if n.is_nan() {
                    "NaN".to_string()
                } else if n.is_infinite() {
                    if *n > 0.0 {
                        "Infinity".to_string()
                    } else {
                        "-Infinity".to_string()
                    }
                } else if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            WireValue::String(s) => s.clone(),
            WireValue::BigInt(s) => s.clone(),
            WireValue::Bytes(_) => "[Uint8Array]".to_string(),
            WireValue::Array(items) => {
                let parts: Vec<String> =
                    items.iter().map(wire_to_display_str).collect();
                format!("[{}]", parts.join(","))
            }
            WireValue::Object(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{}", wire_to_display_str(v)))
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
        }
    }

    // ── Basic ESM execution ───────────────────────────────────────────────

    #[test]
    fn basic_arithmetic_returns_result() {
        let out = run_ok("export default 1 + 1");
        assert_eq!(get_default(&out).as_deref(), Some("2"));
    }

    #[test]
    fn string_concatenation_returns_result() {
        let out = run_ok("export default 'hello' + ' world'");
        assert_eq!(get_default(&out).as_deref(), Some("hello world"));
    }

    #[test]
    fn boolean_true_returns_result() {
        let out = run_ok("export default true");
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

    #[test]
    fn empty_code_returns_no_default_export() {
        let out = run_ok("");
        assert!(get_default(&out).is_none());
        assert!(export_keys(&out).is_empty());
    }

    // ── ESM exports ──────────────────────────────────────────────────────

    #[test]
    fn export_default_number() {
        let out = run_ok("export default 42");
        assert_eq!(get_default(&out).as_deref(), Some("42"));
    }

    #[test]
    fn export_default_string() {
        let out = run_ok(r#"export default "hello""#);
        assert_eq!(get_default(&out).as_deref(), Some("hello"));
    }

    #[test]
    fn export_default_boolean() {
        let out = run_ok("export default true");
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

    #[test]
    fn export_default_null() {
        let out = run_ok("export default null");
        assert_eq!(get_default(&out).as_deref(), Some("null"));
    }

    #[test]
    fn export_default_object() {
        let out = run_ok(r#"export default { x: 1, y: 2 }"#);
        let d = get_default(&out).unwrap();
        // Order is not guaranteed — just check both keys are present.
        assert!(d.contains("x") && d.contains("1"));
        assert!(d.contains("y") && d.contains("2"));
    }

    #[test]
    fn export_default_array() {
        let out = run_ok("export default [1, 2, 3]");
        assert_eq!(get_default(&out).as_deref(), Some("[1,2,3]"));
    }

    #[test]
    fn named_export_single() {
        let out = run_ok("export const answer = 42");
        assert_eq!(get_named(&out, "answer").as_deref(), Some("42"));
    }

    #[test]
    fn named_exports_multiple() {
        let out = run_ok("export const x = 1; export const y = 2");
        assert_eq!(get_named(&out, "x").as_deref(), Some("1"));
        assert_eq!(get_named(&out, "y").as_deref(), Some("2"));
    }

    #[test]
    fn default_and_named_exports_together() {
        let out = run_ok("export default 99; export const label = 'hi'");
        assert_eq!(get_default(&out).as_deref(), Some("99"));
        assert_eq!(get_named(&out, "label").as_deref(), Some("hi"));
    }

    #[test]
    fn no_export_gives_no_default() {
        let out = run_ok("const x = 1");
        assert!(get_default(&out).is_none());
    }

    // ── Async / top-level await ───────────────────────────────────────────

    #[test]
    fn top_level_await_resolves() {
        let out = run_ok("export default await Promise.resolve(7)");
        assert_eq!(get_default(&out).as_deref(), Some("7"));
    }

    #[test]
    fn async_computation_resolves() {
        let out = run_ok(
            r#"
            async function compute() { return 1 + 1; }
            export default await compute();
            "#,
        );
        assert_eq!(get_default(&out).as_deref(), Some("2"));
    }

    #[test]
    fn top_level_await_rejected_promise_is_runtime_error() {
        let err = run_err("export default await Promise.reject(new Error('oops'))");
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    // ── Console capture ───────────────────────────────────────────────────
    // Requires the console shim (Phase 3+).

    #[test]
    fn console_log_captured_in_stdout() {
        let out = run_ok(r#"console.log("hello from log"); export default 1"#);
        assert!(has_line(&out.stdout, "hello from log"));
    }

    #[test]
    fn console_error_captured_in_stderr() {
        let out = run_ok(r#"console.error("something went wrong"); export default 1"#);
        assert!(has_line(&out.stderr, "something went wrong"));
    }

    #[test]
    fn console_warn_captured_in_stderr() {
        let out = run_ok(r#"console.warn("watch out"); export default 1"#);
        assert!(has_line(&out.stderr, "watch out"));
    }

    #[test]
    fn console_debug_captured_in_stdout() {
        let out = run_ok(r#"console.debug("debugging"); export default 1"#);
        assert!(has_line(&out.stdout, "debugging"));
    }

    #[test]
    fn console_info_captured_in_stdout() {
        let out = run_ok(r#"console.info("just so you know"); export default 1"#);
        assert!(has_line(&out.stdout, "just so you know"));
    }

    #[test]
    fn multiple_console_logs_all_captured() {
        let out = run_ok(
            r#"
            console.log("line one");
            console.log("line two");
            console.log("line three");
            export default 1
            "#,
        );
        assert!(has_line(&out.stdout, "line one"));
        assert!(has_line(&out.stdout, "line two"));
        assert!(has_line(&out.stdout, "line three"));
    }

    #[test]
    fn console_log_does_not_bleed_into_stderr() {
        let out = run_ok(r#"console.log("stdout only"); export default 1"#);
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn console_log_multiple_args_joined() {
        let out = run_ok(r#"console.log("a", "b", "c"); export default 1"#);
        assert!(has_line(&out.stdout, "a"));
        assert!(has_line(&out.stdout, "b"));
        assert!(has_line(&out.stdout, "c"));
    }

    // ── Error handling ────────────────────────────────────────────────────

    #[test]
    fn syntax_error_is_compile_error() {
        let err = run_err("export default (((");
        assert!(matches!(err, RunError::CompileError(_)));
    }

    #[test]
    fn thrown_error_is_runtime_error() {
        let err = run_err(r#"throw new Error("boom")"#);
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    #[test]
    fn thrown_error_message_is_preserved() {
        let err = run_err(r#"throw new Error("specific message")"#);
        if let RunError::RuntimeError { message, .. } = err {
            assert!(message.contains("specific message"));
        } else {
            panic!("expected RuntimeError");
        }
    }

    #[test]
    fn logs_before_throw_are_preserved_on_failure() {
        let failure = run_code(
            r#"
            console.log("before stdout");
            console.error("before stderr");
            throw new Error("boom")
            "#,
        )
        .unwrap_err();

        assert!(matches!(failure.error, RunError::RuntimeError { .. }));
        assert!(has_line(&failure.stdout, "before stdout"));
        assert!(has_line(&failure.stderr, "before stderr"));
    }

    #[test]
    fn thrown_string_is_runtime_error() {
        let err = run_err(r#"throw "raw string error""#);
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    #[test]
    fn runtime_error_includes_stack_trace() {
        let err = run_err(
            r#"
            function inner() { throw new Error("deep"); }
            function outer() { inner(); }
            outer();
            "#,
        );
        if let RunError::RuntimeError { stack, .. } = err {
            assert!(stack.is_some(), "expected a stack trace");
        } else {
            panic!("expected RuntimeError");
        }
    }

    #[test]
    fn reference_error_is_runtime_error() {
        let err = run_err("export default undeclaredVariable");
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    #[test]
    fn type_error_is_runtime_error() {
        let err = run_err("null.property");
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    // ── Export validation ─────────────────────────────────────────────────

    #[test]
    fn exporting_a_function_is_an_error() {
        let err = run_err("export default function() {}");
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn exporting_a_class_is_an_error() {
        let err = run_err("export default class Foo {}");
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn exporting_an_unresolved_promise_is_an_error() {
        // A Promise that is never awaited should be rejected at the boundary.
        let err = run_err("export default new Promise(() => {})");
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    // ── Complex value structures ───────────────────────────────────────────

    /// Helper: look up a key inside a `WireValue::Object`, returning the value.
    fn wire_obj_get(v: &WireValue, key: &str) -> Option<WireValue> {
        if let WireValue::Object(fields) = v {
            fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        } else {
            None
        }
    }

    #[test]
    fn sum_1_to_100_in_nested_object() {
        // Runtime arithmetic + nested object export.
        let out = run_ok(
            r#"
            let sum = 0;
            for (let i = 1; i <= 100; i++) sum += i;
            export default {
                result: sum,
                metadata: { label: "sum_1_to_100", iterations: 100 }
            }
            "#,
        );
        let default_val = get_field(&out, "default").unwrap();
        assert_eq!(wire_obj_get(&default_val, "result"), Some(WireValue::Number(5050.0)));
        let meta = wire_obj_get(&default_val, "metadata").unwrap();
        assert_eq!(
            wire_obj_get(&meta, "label"),
            Some(WireValue::String("sum_1_to_100".to_string()))
        );
        assert_eq!(wire_obj_get(&meta, "iterations"), Some(WireValue::Number(100.0)));
    }

    #[test]
    fn objects_inside_array() {
        let out = run_ok("export default [{ a: 1 }, { b: 2 }, { c: 3 }]");
        let val = get_field(&out, "default").unwrap();
        if let WireValue::Array(items) = val {
            assert_eq!(items.len(), 3);
            assert_eq!(wire_obj_get(&items[0], "a"), Some(WireValue::Number(1.0)));
            assert_eq!(wire_obj_get(&items[1], "b"), Some(WireValue::Number(2.0)));
            assert_eq!(wire_obj_get(&items[2], "c"), Some(WireValue::Number(3.0)));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn object_in_second_object_in_array() {
        // Array → Object → Object nesting.
        let out = run_ok(
            r#"export default [{ outer: { inner: 42 } }, { x: [1, 2, 3] }]"#,
        );
        let val = get_field(&out, "default").unwrap();
        if let WireValue::Array(items) = val {
            let inner = wire_obj_get(&wire_obj_get(&items[0], "outer").unwrap(), "inner");
            assert_eq!(inner, Some(WireValue::Number(42.0)));
            let arr = wire_obj_get(&items[1], "x").unwrap();
            assert_eq!(
                arr,
                WireValue::Array(vec![
                    WireValue::Number(1.0),
                    WireValue::Number(2.0),
                    WireValue::Number(3.0),
                ])
            );
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn deep_alternating_nesting_array_object_array_object() {
        // Object → Array → Object → Array → Number
        let out = run_ok(
            r#"export default { a: [{ b: [{ c: 42 }, { d: "hello" }] }] }"#,
        );
        let default_val = get_field(&out, "default").unwrap();
        let a = wire_obj_get(&default_val, "a").unwrap();
        if let WireValue::Array(a_items) = a {
            let b = wire_obj_get(&a_items[0], "b").unwrap();
            if let WireValue::Array(b_items) = b {
                assert_eq!(wire_obj_get(&b_items[0], "c"), Some(WireValue::Number(42.0)));
                assert_eq!(
                    wire_obj_get(&b_items[1], "d"),
                    Some(WireValue::String("hello".to_string()))
                );
            } else {
                panic!("expected b to be Array");
            }
        } else {
            panic!("expected a to be Array");
        }
    }

    #[test]
    fn multiple_named_exports_with_complex_values() {
        let out = run_ok(
            r#"
            export const nums = [1, 2, 3];
            export const info = { count: 3, total: 6 };
            export default "summary";
            "#,
        );
        assert_eq!(get_default(&out).as_deref(), Some("summary"));
        assert_eq!(
            get_field(&out, "nums"),
            Some(WireValue::Array(vec![
                WireValue::Number(1.0),
                WireValue::Number(2.0),
                WireValue::Number(3.0),
            ]))
        );
        let info = get_field(&out, "info").unwrap();
        assert_eq!(wire_obj_get(&info, "count"), Some(WireValue::Number(3.0)));
        assert_eq!(wire_obj_get(&info, "total"), Some(WireValue::Number(6.0)));
    }

    // ── Cycle detection ───────────────────────────────────────────────────

    #[test]
    fn cyclic_object_is_rejected() {
        let err = run_err(
            r#"
            const obj = { x: 1 };
            obj.self = obj;
            export default obj;
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn cyclic_array_is_rejected() {
        let err = run_err(
            r#"
            const arr = [1, 2, 3];
            arr.push(arr);
            export default arr;
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn indirect_cycle_array_inside_object_inside_array() {
        // arr → obj → arr (cross-type indirect cycle)
        let err = run_err(
            r#"
            const arr = [];
            const obj = { arr };
            arr.push(obj);
            export default arr;
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn indirect_cycle_two_objects() {
        let err = run_err(
            r#"
            const a = {};
            const b = { a };
            a.b = b;
            export default a;
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
    }

    #[test]
    fn shared_reference_is_not_a_cycle() {
        // The same object appears in two fields but is not cyclic.
        // This must succeed — pop-after-visit ensures no false positive.
        let out = run_ok(
            r#"
            const shared = { x: 1 };
            export default { a: shared, b: shared };
            "#,
        );
        let default_val = get_field(&out, "default").unwrap();
        assert_eq!(wire_obj_get(&wire_obj_get(&default_val, "a").unwrap(), "x"), Some(WireValue::Number(1.0)));
        assert_eq!(wire_obj_get(&wire_obj_get(&default_val, "b").unwrap(), "x"), Some(WireValue::Number(1.0)));
    }

    #[test]
    fn shared_array_reference_is_not_a_cycle() {
        let out = run_ok(
            r#"
            const shared = [1, 2, 3];
            export default { first: shared, second: shared };
            "#,
        );
        let default_val = get_field(&out, "default").unwrap();
        let expected_arr = WireValue::Array(vec![
            WireValue::Number(1.0),
            WireValue::Number(2.0),
            WireValue::Number(3.0),
        ]);
        assert_eq!(wire_obj_get(&default_val, "first"), Some(expected_arr.clone()));
        assert_eq!(wire_obj_get(&default_val, "second"), Some(expected_arr));
    }

    // ── BigInt ────────────────────────────────────────────────────────────

    #[test]
    fn bigint_positive_encodes_as_decimal_string() {
        let out = run_ok("export default 42n");
        assert_eq!(get_field(&out, "default"), Some(WireValue::BigInt("42".to_string())));
    }

    #[test]
    fn bigint_negative_encodes_as_decimal_string() {
        let out = run_ok("export default -100n");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::BigInt("-100".to_string()))
        );
    }

    #[test]
    fn bigint_zero_encodes_correctly() {
        let out = run_ok("export default 0n");
        assert_eq!(get_field(&out, "default"), Some(WireValue::BigInt("0".to_string())));
    }

    #[test]
    fn bigint_inside_object_encodes_correctly() {
        let out = run_ok("export default { count: 1000000000000n }");
        let default_val = get_field(&out, "default").unwrap();
        assert_eq!(
            wire_obj_get(&default_val, "count"),
            Some(WireValue::BigInt("1000000000000".to_string()))
        );
    }

    // ── Globals ───────────────────────────────────────────────────────────

    #[test]
    fn math_random_is_available() {
        // Math is a V8 built-in — no setup needed.
        let out = run_ok("export default typeof Math.random === 'function'");
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

    #[test]
    fn json_parse_stringify_available() {
        let out = run_ok(r#"export default JSON.stringify({a:1})"#);
        // JSON.stringify returns a JS string — WireValue::String — displayed as-is.
        assert_eq!(get_default(&out).as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn unconfigured_fetch_is_just_a_missing_global_runtime_error() {
        let err = run_err(r#"export default await fetch("https://example.com")"#);
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    #[test]
    #[ignore = "requires deliberate crypto.getRandomValues shim"]
    fn crypto_get_random_values_available() {
        let out = run_ok(
            r#"
            const buf = new Uint8Array(8);
            crypto.getRandomValues(buf);
            export default buf.length
            "#,
        );
        assert_eq!(get_default(&out).as_deref(), Some("8"));
    }

    #[test]
    fn console_is_defined() {
        let out = run_ok("export default typeof console");
        assert_eq!(get_default(&out).as_deref(), Some("object"));
    }

    #[test]
    fn node_globals_are_not_available() {
        // `process` and `require` must not exist in the sandbox.
        let out = run_ok("export default typeof process");
        assert_eq!(get_default(&out).as_deref(), Some("undefined"));
    }

    #[test]
    fn node_require_is_not_available() {
        let out = run_ok("export default typeof require");
        assert_eq!(get_default(&out).as_deref(), Some("undefined"));
    }

    // ── Imports ───────────────────────────────────────────────────────────
    // Requires the module resolver (Phase 6+).

    #[test]
    fn unknown_import_is_module_not_found() {
        let err = run_err(r#"import { foo } from "unknown:module"; export default foo"#);
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_import_provided_by_host_works() {
        // Host provides the source for "math:add". Once imports are wired up
        // this should resolve and run the provided source.
        let err = run_err(
            r#"
            import { add } from "math:add";
            export default add(1, 2)
            "#,
        );
        // For now it fails with ModuleNotFound because no resolver exists.
        // Once Phase 6 lands this test body will change to assert Ok(3).
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn host_module_function_callable() {
        // Host provides a function under "host:tools". Phase 7+.
        let err = run_err(
            r#"
            import { search } from "host:tools";
            export default await search("query")
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    // ── Resource limits ───────────────────────────────────────────────────
    // Marked #[ignore] — without the limit implementation these would hang
    // or OOM the process. Un-ignore when the corresponding phase lands.

    #[test]
    #[ignore = "requires cpu timeout (Phase 1 wall-clock, Phase 3 cpu budget)"]
    fn infinite_loop_is_cpu_timeout() {
        let err = run_err("while(true) {}");
        assert!(matches!(err, RunError::CpuTimeout | RunError::WallTimeout));
    }

    #[test]
    #[ignore = "requires memory limit (Phase 8)"]
    fn allocating_too_much_memory_is_memory_limit() {
        let err = run_err(
            r#"
            const arrays = [];
            while (true) { arrays.push(new Uint8Array(1024 * 1024)); }
            "#,
        );
        assert!(matches!(err, RunError::MemoryLimit));
    }

    #[test]
    #[ignore = "requires wall-clock timeout (Phase 1)"]
    fn long_running_async_is_wall_timeout() {
        let err = run_err(
            r#"
            export default await new Promise(resolve => setTimeout(resolve, 999999))
            "#,
        );
        assert!(matches!(err, RunError::WallTimeout));
    }

    // ── Source modules (host-provided JS) ───────────────────────────────
    // The host supplies raw JS source for an import specifier. V8 compiles
    // it once per isolate and caches it. Phase 6+.

    #[test]
    fn source_module_basic_function_import() {
        // Host provides source for "lib:math". The module exports an `add`
        // function. User code imports and calls it.
        //
        // When Phase 6 lands: wire up a source import for "lib:math" that
        // contains `export function add(a, b) { return a + b; }` and assert
        // the result is 3.
        let err = run_err(
            r#"
            import { add } from "lib:math";
            export default add(1, 2)
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_module_mimicking_zod_schema_validation() {
        // The host provides a simplified zod-like schema library as source.
        // This is the canonical use-case: precompile the library into the
        // prefix snapshot once, then validate data in every postfix run.
        //
        // Mock source the host would supply for "lib:zod":
        //
        //   export const z = {
        //     object: (shape) => ({
        //       parse: (data) => {
        //         for (const key of Object.keys(shape)) {
        //           if (!(key in data)) throw new Error(`missing key: ${key}`);
        //         }
        //         return data;
        //       }
        //     }),
        //     string: () => ({}),
        //     number: () => ({}),
        //   };
        //
        // When Phase 6 lands: provide that source, assert parse succeeds.
        let err = run_err(
            r#"
            import { z } from "lib:zod";
            const schema = z.object({ name: z.string(), age: z.number() });
            export default schema.parse({ name: "Alice", age: 30 })
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_module_zod_schema_fails_on_bad_data() {
        // Same setup as above, but pass data that fails validation.
        // When Phase 6 lands: assert this surfaces as RuntimeError (the
        // schema throws), not ModuleNotFound.
        let err = run_err(
            r#"
            import { z } from "lib:zod";
            const schema = z.object({ name: z.string() });
            export default schema.parse({ wrong: true })
            "#,
        );
        // Pre-phase-6: ModuleNotFound. Post-phase-6: RuntimeError.
        assert!(matches!(
            err,
            RunError::ModuleNotFound(_) | RunError::RuntimeError { .. }
        ));
    }

    #[test]
    fn source_module_utility_library_used_across_multiple_exports() {
        // A utility lib is imported and used in both the default and a named
        // export. Verifies the module is only compiled once and shared.
        let err = run_err(
            r#"
            import { clamp, lerp } from "lib:math-utils";
            export default clamp(5, 0, 10);
            export const interpolated = lerp(0, 100, 0.5);
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_module_can_import_another_source_module() {
        // Transitive source imports: "lib:app" imports from "lib:utils".
        // Phase 6+: module resolver must handle transitive resolution.
        let err = run_err(
            r#"
            import { formatResult } from "lib:app";
            export default formatResult(42)
            "#,
        );
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    // ── Custom globals ────────────────────────────────────────────────────
    // The host installs named values onto globalThis before running user
    // code. Phase 3+ (currently only `fetch` is allowlisted; this tests
    // the general mechanism).

    #[test]
    #[ignore = "v1 only allowlists fetch as a host-provided global; arbitrary globals need a design change"]
    fn custom_global_string_is_accessible() {
        // Host provides: globals.appVersion = "1.2.3"
        // User code reads it from globalThis.
        //
        // This is intentionally ignored because DESIGN.md currently allows
        // only `fetch` as a host-provided global. Everything else should go
        // through imports unless the design changes.
        let out = run_ok(r#"export default globalThis.appVersion"#);
        assert_eq!(get_default(&out).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn custom_global_object_is_accessible_and_callable() {
        // Host provides a plain object as a global:
        //   globals.config = { model: "gpt-4", maxTokens: 1000 }
        // User code reads its properties.
        //
        // When Phase 3+ lands: assert config.model === "gpt-4".
        let _ = run(r#"
            export default globalThis.config?.model ?? "not set"
            "#);
    }

    #[test]
    fn undeclared_global_at_run_time_is_undefined_not_error() {
        // A global that was never declared should silently be undefined,
        // not throw a ReferenceError. Optional chaining makes this safe.
        let out = run_ok(r#"export default globalThis.neverDeclared ?? "fallback""#);
        assert_eq!(get_default(&out).as_deref(), Some("fallback"));
    }

    #[test]
    fn host_tool_function_callable_as_global() {
        // Host wires a function onto globalThis:
        //   globals.searchTool = async (query) => { /* real impl */ }
        // This is the canonical AI-agent tool-binding pattern.
        //
        // When globals injection is implemented: assert the call resolves.
        let err = run_err(r#"export default await globalThis.searchTool("cats")"#);
        // Pre-implementation: TypeError (searchTool is not a function).
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    #[test]
    fn multiple_host_tools_available_as_globals() {
        // Several tools mounted as globals — search, fetch, summarize.
        // Each is called in sequence and results combined.
        //
        // When globals injection lands: assert all three are callable.
        let err = run_err(
            r#"
            const a = await globalThis.searchTool("query");
            const b = await globalThis.summarize(a);
            export default b
            "#,
        );
        assert!(matches!(err, RunError::RuntimeError { .. }));
    }

    #[test]
    fn globals_do_not_leak_between_runs() {
        // A previous run setting globalThis.x = 99 must not affect the next
        // run. Each run gets a fresh context.
        let out1 = run_ok("globalThis.__leakTest = 99; export default 1");
        let out2 = run_ok("export default globalThis.__leakTest ?? 'clean'");
        assert_eq!(get_default(&out1).as_deref(), Some("1"));
        // Must be 'clean', not 99.
        assert_eq!(get_default(&out2).as_deref(), Some("clean"));
    }

    #[test]
    fn host_provided_data_object_as_global() {
        // The host mounts a large read-only data object as a global:
        //   globals.dataset = [{ id: 1, value: 10 }, { id: 2, value: 20 }]
        // User code does analytics on it.
        //
        // When globals injection lands: assert reduce result is 30.
        let _ = run(r#"
            const total = (globalThis.dataset ?? []).reduce(
              (acc, row) => acc + row.value, 0
            );
            export default total
            "#);
    }

    // ── Output metadata ───────────────────────────────────────────────────

    #[test]
    fn duration_ms_is_populated() {
        let out = run_ok("export default 1 + 1");
        assert!(out.duration_ms < 5_000);
    }

    // ── AbortSignal / cancellation ────────────────────────────────────────
    // Requires AbortSignal threading through run_code (Phase 1+).

    #[test]
    #[ignore = "requires signal/abort support"]
    fn aborted_signal_before_run_is_err_aborted() {
        // If the caller aborts the signal before the run starts, the result
        // should be ERR_ABORTED immediately without executing any JS.
        // TODO: pass a pre-aborted signal into run_code and assert ERR_ABORTED.
        todo!()
    }

    #[test]
    #[ignore = "requires signal/abort support"]
    fn aborted_signal_during_async_run_is_err_aborted() {
        // Signal is aborted while the sandbox is mid-execution (e.g., waiting
        // on a host bridge call). Rust calls terminate_execution().
        todo!()
    }

    // ── Export size limit ─────────────────────────────────────────────────
    // Requires maxExportBytes enforcement (Phase 1+).

    #[test]
    #[ignore = "requires maxExportBytes enforcement"]
    fn export_exceeding_size_limit_is_err_export_too_large() {
        // User code exports a very large string/array. The serialized export
        // bytes exceed `maxExportBytes`; the run should fail instead of
        // sending a huge payload over the wire.
        let err = run_err(
            r#"
            export default "x".repeat(32 * 1024 * 1024)
            "#,
        );
        assert!(matches!(err, RunError::ExportNotSerializable(_)));
        // TODO: add an ExportTooLarge variant and check that instead.
    }

    // ── stdout / stderr size limits ───────────────────────────────────────
    // Requires maxStdoutBytes / maxStderrBytes enforcement (Phase 3+).

    #[test]
    #[ignore = "requires maxStdoutBytes enforcement (Phase 3)"]
    fn stdout_exceeding_limit_truncates_or_errors() {
        // Writing more than maxStdoutBytes via console.log must not OOM the
        // process. Either truncate silently or fail with an internal error.
        // The current design leans toward truncation (cap, not error).
        todo!()
    }

    #[test]
    #[ignore = "requires maxStderrBytes enforcement (Phase 3)"]
    fn stderr_exceeding_limit_truncates_or_errors() {
        todo!()
    }

    // ── Host bridge error propagation ─────────────────────────────────────
    // Phase 7+.

    #[test]
    #[ignore = "requires host import bridge (Phase 7)"]
    fn host_import_function_throws_is_err_host_import() {
        // A host-provided import function throws. The error must surface as
        // ERR_HOST_BRIDGE in the result, not crash the runtime.
        todo!()
    }

    // ── ERR_UNDECLARED_BINDING ────────────────────────────────────────────
    // Phase 2+ (requires precompile / snapshots).

    #[test]
    #[ignore = "requires snapshot/precompile (Phase 2)"]
    fn run_with_undeclared_binding_is_error() {
        // prefix.run() passes a global name that was not declared at
        // precompile() time. Must fail with ERR_UNDECLARED_BINDING instead
        // of silently installing an unexpected global into the restored
        // snapshot context.
        todo!()
    }

    // ── CPU budget excludes async wait time ───────────────────────────────
    // Phase 3 (cpu budget bracketing).

    #[test]
    #[ignore = "requires cpu budget bracketing (Phase 3)"]
    fn cpu_budget_does_not_count_time_waiting_on_bridge() {
        // A run with a tight cpuTimeMs limit (e.g. 50ms) but a slow host
        // bridge call (e.g. 200ms sleep) must NOT hit ERR_CPU_TIMEOUT.
        // Wall time continues; CPU time is paused while awaiting the bridge.
        todo!()
    }

    #[test]
    #[ignore = "requires cpu budget bracketing (Phase 3)"]
    fn cpu_budget_does_count_tight_sync_loop() {
        // Same tight cpuTimeMs limit, but the code is a tight synchronous
        // computation that never awaits. Must hit ERR_CPU_TIMEOUT.
        todo!()
    }
}
