//! `console.*` capture: five methods wrapped natively, rendered to one line
//! each, buffered per run under its byte caps and echoed to the observer.

use crate::link::ConsoleStream;
use crate::v8::RunError;
use crate::v8::exceptions::capped_text_lossy;
use crate::v8::isolate::LogBuffers;
use crate::v8::runtable::{attributed_state, slot_table};

/// Wrap five methods on V8's own `console` — replacing the object would delete
/// 18 others. Unwrapped ones no-op: no `ConsoleDelegate` is registered.
/// Table in `docs/conformance.md`.
pub(crate) fn install_console(scope: &mut v8::PinScope) -> Result<(), RunError> {
    let global = scope.get_current_context().global(scope);
    let console_key = v8::String::new(scope, "console")
        .ok_or_else(|| RunError::Internal("failed to intern console".to_string()))?;
    let console: v8::Local<v8::Object> = global
        .get(scope, console_key.into())
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| RunError::Internal("V8 console object is missing".to_string()))?;

    for name in ["log", "debug", "info"] {
        let key = v8::String::new(scope, name)
            .ok_or_else(|| RunError::Internal(format!("failed to intern console.{name}")))?;
        let function = v8::Function::builder(console_stdout_callback)
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
            .build(scope)
            .ok_or_else(|| RunError::Internal(format!("failed to create console.{name}")))?;
        console
            .set(scope, key.into(), function.into())
            .ok_or_else(|| RunError::Internal(format!("failed to install console.{name}")))?;
    }

    Ok(())
}

pub(crate) fn console_stdout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    append_console_line(scope, args, true);
    rv.set_undefined();
}

pub(crate) fn console_stderr_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    append_console_line(scope, args, false);
    rv.set_undefined();
}

/// Render every argument and join with spaces. One TryCatch for the whole
/// line — rendering reads guest properties (`toJSON`, `stack`) that may throw,
/// and the reset between args stops one from leaking into the next.
fn render_console_line(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
) -> Option<String> {
    v8::tc_scope!(let tc, scope);
    let mut parts = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        parts.push(render_console_arg(tc, args.get(i)));
        if tc.has_terminated() {
            return None;
        }
        tc.reset();
    }
    Some(parts.join(" "))
}

/// One console argument as text: JSON for plain data, `String(v)` otherwise.
/// Strings stay unquoted and errors carry their stack. Never throws — a
/// hostile getter degrades to a placeholder. See `docs/conformance.md`.
fn render_console_arg(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<v8::Value>,
) -> String {
    if value.is_undefined() {
        return "undefined".to_string();
    }
    if value.is_null() {
        return "null".to_string();
    }
    // The common case: a logged string must not gain quotes.
    if let Ok(text) = v8::Local::<v8::String>::try_from(value) {
        return capped_text_lossy(scope, text, usize::MAX);
    }
    if value.is_native_error() {
        return render_error_arg(scope, value);
    }
    if is_json_renderable(value) {
        if let Some(text) = render_json_arg(scope, value) {
            if text != "{}" {
                return text;
            }
            // JSON said nothing. `String(v)` names a Map, a Promise or a
            // RegExp; for a genuinely empty object it says less than `{}`.
            let named = display_string(scope, value);
            return if named == "[object Object]" { text } else { named };
        }
        scope.reset();
    }
    display_string(scope, value)
}

/// Plain data only: primitives, arrays, `{}`-shaped objects, and anything that
/// opts in with a `toJSON` method. A `Promise`, a `Map`, or a class instance
/// serializes to `{}`, so those take the `String(v)` path instead.
fn is_json_renderable(value: v8::Local<v8::Value>) -> bool {
    if value.is_number() || value.is_boolean() || value.is_array() {
        return true;
    }
    // Binary data serializes to a per-byte object — huge and less readable
    // than `String(v)`, which gives the element list.
    value.is_object() && !value.is_array_buffer() && !value.is_array_buffer_view()
}

/// `JSON.stringify(value)`, or `None` when it throws (a cycle) or yields
/// `undefined` (a function, a symbol) — both fall back to `String(v)`.
fn render_json_arg(scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>, value: v8::Local<v8::Value>) -> Option<String> {
    if scope.has_terminated() {
        return None;
    }
    let text = capped_text_lossy(scope, v8::json::stringify(scope, value)?, usize::MAX);
    (text != "undefined").then_some(text)
}

/// The stack is the useful part of a logged error; `Name: message` when it is
/// absent or throws.
fn render_error_arg(scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>, value: v8::Local<v8::Value>) -> String {
    if let Ok(object) = v8::Local::<v8::Object>::try_from(value) {
        if let Some(key) = v8::String::new(scope, "stack") {
            if let Some(stack) = object.get(scope, key.into()) {
                if let Ok(text) = v8::Local::<v8::String>::try_from(stack) {
                    return capped_text_lossy(scope, text, usize::MAX);
                }
            }
        }
        scope.reset();
    }
    display_string(scope, value)
}

/// `String(value)`, or a placeholder when even that throws.
fn display_string(scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>, value: v8::Local<v8::Value>) -> String {
    if scope.has_terminated() {
        return "[unprintable]".to_string();
    }
    // `String(symbol)` throws by spec; ToDetailString renders `Symbol(desc)`.
    if value.is_symbol() {
        let rendered = value
            .to_detail_string(scope)
            .map(|text| capped_text_lossy(scope, text, usize::MAX));
        scope.reset();
        return rendered.unwrap_or_else(|| "[unprintable]".to_string());
    }
    let rendered = value
        .to_string(scope)
        .map(|text| capped_text_lossy(scope, text, usize::MAX));
    scope.reset();
    rendered.unwrap_or_else(|| "[unprintable]".to_string())
}

fn append_console_line(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    stdout: bool,
) {
    let Some(table) = slot_table(scope) else {
        return;
    };

    // A kill mid-render drops the line; the termination stays pending.
    let Some(line) = render_console_line(scope, &args) else {
        return;
    };
    let state = attributed_state(scope, table);
    if state.is_null() {
        // No run owns this line: prefix warm-up output, delivered on the
        // cold-start call. (Uncapped — warm-up is host-authored code under
        // its own fixed budget.)
        append_capped_line(&mut table.setup_logs.borrow_mut(), stdout, line);
        return;
    }
    // SAFETY: entry pointers are valid for the remainder of the turn.
    let entry = unsafe { &*state };
    entry.observer.console(
        entry.addr,
        if stdout {
            ConsoleStream::Stdout
        } else {
            ConsoleStream::Stderr
        },
        &line,
    );
    append_capped_line(&mut entry.logs.borrow_mut(), stdout, line);
}

/// Push one console line into `buffers`, honouring its byte caps. Lines that
/// would push the total over the cap are silently dropped.
fn append_capped_line(buffers: &mut LogBuffers, stdout: bool, line: String) {
    if stdout {
        let limit = buffers.max_stdout_bytes as usize;
        if limit == 0 || buffers.stdout_bytes + line.len() <= limit {
            buffers.stdout_bytes += line.len();
            buffers.stdout.push(line);
        }
    } else {
        let limit = buffers.max_stderr_bytes as usize;
        if limit == 0 || buffers.stderr_bytes + line.len() <= limit {
            buffers.stderr_bytes += line.len();
            buffers.stderr.push(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::link::{Addr, Observer};
    use crate::v8::testsupport::{evaluate_prefix, get_default, has_line, run_code, run_ok};
    use crate::v8::{Limits, RunError, RunIo, execute_with_io, init_platform};

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
    fn console_lines_reach_the_observer_before_the_caps_apply() {
        struct Recorder(Mutex<Vec<(Addr, ConsoleStream, String)>>);
        impl Observer for Recorder {
            fn console(&self, to: Addr, stream: ConsoleStream, line: &str) {
                self.0
                    .lock()
                    .unwrap()
                    .push((to, stream, line.to_string()));
            }
        }
        init_platform();
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let out = execute_with_io(
            "console.log('a'); console.error('b'); console.log('c'.repeat(100)); export default 1",
            None,
            Limits {
                max_stdout_bytes: 1,
                ..Default::default()
            },
            &[],
            &[],
            RunIo::None,
            0,
            Arc::new(AtomicU32::new(0)),
            None,
            None,
            Arc::clone(&recorder) as Arc<dyn Observer>,
        )
        .unwrap();
        // The result keeps the capped buffers; the observer saw every line.
        assert_eq!(out.stdout, vec!["a".to_string()]);
        assert_eq!(out.stderr, vec!["b".to_string()]);
        let lines = recorder.0.lock().unwrap();
        assert_eq!(
            *lines,
            vec![
                (Addr::Run(0), ConsoleStream::Stdout, "a".to_string()),
                (Addr::Run(0), ConsoleStream::Stderr, "b".to_string()),
                (Addr::Run(0), ConsoleStream::Stdout, "c".repeat(100)),
            ]
        );
    }

    #[test]
    fn console_log_multiple_args_joined() {
        let out = run_ok(r#"console.log("a", "b", "c"); export default 1"#);
        assert!(has_line(&out.stdout, "a"));
        assert!(has_line(&out.stdout, "b"));
        assert!(has_line(&out.stdout, "c"));
    }

    /// One `console.log(<expr>)` line, rendered.
    fn logged(expr: &str) -> String {
        let out = run_ok(&format!("console.log({expr}); export default 1"));
        assert_eq!(out.stdout.len(), 1, "expected one line, got {:?}", out.stdout);
        out.stdout.into_iter().next().unwrap()
    }

    #[test]
    fn plain_data_logs_as_json() {
        assert_eq!(logged("{ user: 'bob', tries: 3 }"), r#"{"user":"bob","tries":3}"#);
        assert_eq!(logged("[1, 2, 3]"), "[1,2,3]");
        assert_eq!(logged("{ nested: { deep: [1] } }"), r#"{"nested":{"deep":[1]}}"#);
        assert_eq!(logged("{}"), "{}");
    }

    #[test]
    fn a_class_instance_logs_its_own_fields() {
        assert_eq!(
            logged("new (class Bar { constructor() { this.a = 1 } })()"),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn to_json_is_honoured() {
        assert_eq!(
            logged("{ toJSON() { return { via: 'toJSON' } } }"),
            r#"{"via":"toJSON"}"#
        );
    }

    /// JSON renders a Map, a Promise and a RegExp as `{}` — useless. Those
    /// fall back to `String(v)`, which at least names the type.
    #[test]
    fn types_json_cannot_describe_fall_back_to_their_name() {
        assert_eq!(logged("new Map([['k', 1]])"), "[object Map]");
        assert_eq!(logged("new Set([1])"), "[object Set]");
        assert_eq!(logged("Promise.resolve(1)"), "[object Promise]");
        assert_eq!(logged("/re/g"), "/re/g");
    }

    #[test]
    fn strings_are_not_quoted_and_primitives_read_plainly() {
        assert_eq!(logged("'plain'"), "plain");
        assert_eq!(logged("null, undefined, 42, true"), "null undefined 42 true");
        assert_eq!(logged("() => 1"), "() => 1");
        assert_eq!(logged("Symbol('s')"), "Symbol(s)");
    }

    /// The stack is the point of logging an error.
    #[test]
    fn an_error_logs_its_stack() {
        let line = logged("new Error('boom')");
        assert!(line.starts_with("Error: boom"), "got {line}");
        assert!(line.contains("at "), "expected stack frames, got {line}");
    }

    /// Binary data would JSON-serialize to one entry per byte.
    #[test]
    fn typed_arrays_do_not_serialize_per_byte() {
        assert_eq!(logged("new Uint8Array([1, 2, 3])"), "1,2,3");
    }

    /// Rendering reads guest properties, so it must survive hostile ones.
    /// Both degrade to `String(v)`; neither may fail the run.
    #[test]
    fn unrenderable_arguments_never_fail_the_run() {
        let out = run_ok(
            "const cyc = { a: 1 }; cyc.self = cyc\n\
             console.log(cyc)\n\
             console.log({ get boom() { throw new Error('getter') } })\n\
             console.log('still running')\n\
             export default 'survived'",
        );
        assert_eq!(get_default(&out).as_deref(), Some("survived"));
        assert_eq!(out.stdout.len(), 3, "got {:?}", out.stdout);
        assert_eq!(out.stdout[2], "still running");
    }

    /// The five `install_console` wraps.
    const WRAPPED_CONSOLE_METHODS: &[&str] = &["log", "debug", "info", "warn", "error"];

    const CONSOLE_SURFACE: &str = "Object.getOwnPropertyNames(console).sort().join(' ')";

    /// Present but unwrapped: V8 builtins that no-op with no ConsoleDelegate.
    const INERT_CONSOLE_METHODS: &[&str] = &[
        "assert",
        "clear",
        "count",
        "dir",
        "group",
        "groupEnd",
        "profile",
        "profileEnd",
        "table",
        "time",
        "timeEnd",
        "timeLog",
        "timeStamp",
        "trace",
    ];

    #[test]
    fn the_console_keeps_v8s_full_method_set() {
        let out = run_ok(&format!("export default {CONSOLE_SURFACE}"));
        let surface = get_default(&out).expect("console surface");
        let present: Vec<&str> = surface.split(' ').collect();
        for name in WRAPPED_CONSOLE_METHODS.iter().chain(INERT_CONSOLE_METHODS) {
            assert!(
                present.contains(name),
                "console.{name} missing from {surface}"
            );
        }
    }

    #[test]
    fn unwrapped_console_methods_are_callable_and_silent() {
        let out = run_ok(
            r#"
            console.table([{ a: 1 }]); console.group('g'); console.groupEnd()
            console.count('c'); console.assert(false, 'nope'); console.dir({})
            console.trace('t'); console.time('t'); console.timeEnd('t')
            export default 'survived'
            "#,
        );
        assert_eq!(get_default(&out).as_deref(), Some("survived"));
        assert!(
            out.stdout.is_empty(),
            "unwrapped methods emit nothing, got {:?}",
            out.stdout
        );
        assert!(
            out.stderr.is_empty(),
            "unwrapped methods emit nothing, got {:?}",
            out.stderr
        );
    }

    /// A prefix evaluates against the same console as run code.
    #[test]
    fn the_console_surface_is_identical_in_prefix_and_run_code() {
        let at_run = get_default(&run_ok(&format!("export default {CONSOLE_SURFACE}")))
            .expect("run surface");
        let err = evaluate_prefix(&format!("throw new Error({CONSOLE_SURFACE})"), &[], &[])
            .unwrap_err();
        let RunError::RuntimeError(data) = &err.error else {
            panic!("expected the probe to throw, got {:?}", err.error);
        };
        assert_eq!(data.message, at_run);
    }

    // ── stdout / stderr size limits ───────────────────────────────────────
    // Requires maxStdoutBytes / maxStderrBytes enforcement (Phase 3+).

    #[test]
    fn stdout_exceeding_limit_is_silently_truncated() {
        // Lines that would push stdout over maxStdoutBytes are silently dropped.
        // The run itself must still succeed.
        let limits = Limits {
            max_stdout_bytes: 50, // allow only ~50 bytes of stdout
            ..Limits::default()
        };
        let out = run_code(
            // Each line is "hello" (5 bytes); 20 lines = 100 bytes total.
            // Only the first ~10 lines (50 bytes) should survive.
            r#"
            for (let i = 0; i < 20; i++) { console.log("hello"); }
            export default 1
            "#,
            "<iso4>",
            limits,
        )
        .unwrap();
        // At most 50 bytes worth of lines should have been captured.
        let total: usize = out.stdout.iter().map(|s| s.len()).sum();
        assert!(total <= 50, "stdout bytes {total} exceeded limit 50");
        // At least some output must have been captured.
        assert!(
            !out.stdout.is_empty(),
            "expected some stdout to be captured"
        );
    }

    #[test]
    fn stderr_exceeding_limit_is_silently_truncated() {
        let limits = Limits {
            max_stderr_bytes: 50,
            ..Limits::default()
        };
        let out = run_code(
            r#"
            for (let i = 0; i < 20; i++) { console.error("oops"); }
            export default 1
            "#,
            "<iso4>",
            limits,
        )
        .unwrap();
        let total: usize = out.stderr.iter().map(|s| s.len()).sum();
        assert!(total <= 50, "stderr bytes {total} exceeded limit 50");
        assert!(
            !out.stderr.is_empty(),
            "expected some stderr to be captured"
        );
    }

    #[test]
    fn stdout_limit_zero_means_no_limit() {
        // max_stdout_bytes = 0 disables the cap.
        let limits = Limits {
            max_stdout_bytes: 0,
            ..Limits::default()
        };
        let out = run_code(
            r#"
            for (let i = 0; i < 100; i++) { console.log("line"); }
            export default 1
            "#,
            "<iso4>",
            limits,
        )
        .unwrap();
        assert_eq!(out.stdout.len(), 100);
    }

    #[test]
    fn a_console_line_killed_mid_render_is_dropped() {
        let failure = run_code(
            "console.log({ toJSON() { for (;;) {} } })\n\
             console.log('after')\n\
             export default 1",
            "<iso4>",
            Limits {
                cpu_time_ms: 50,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(failure.error, RunError::CpuTimeout), "got {:?}", failure.error);
        assert!(failure.stdout.is_empty(), "got {:?}", failure.stdout);
    }

    #[test]
    fn a_huge_console_line_is_capped_without_flattening() {
        // Same heap-cap proof as the thrown-message test.
        let out = run_code(
            "console.log('a'.repeat(64 * 1024 * 1024))\nexport default 1",
            "<iso4>",
            Limits {
                max_stdout_bytes: 1024 * 1024,
                hard_memory_mb: 32,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.stdout.len(), 1);
        assert!(out.stdout[0].ends_with("… [truncated by iso4: 67108864 UTF-16 units total]"));
    }
}
