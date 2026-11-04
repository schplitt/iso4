//! Host-declared globals installed natively onto the sandbox global object,
//! the reserved-name check, script evaluation helpers, and the `waitUntil`
//! native with its `iso4:runtime` module.

use crate::blob;
use crate::types::HostGlobalDef;
use crate::v8::streams::{stream_cancel_callback, stream_read_callback};
use crate::v8::{
    RunError, RuntimeErrorData, attributed_state, codec_error_to_run_error, exception_fields,
    exception_message, exception_name, exception_stack, slot_table, syntax_error,
};

// ── Value globals (native install) ────────────────────────────────────────────

/// Fixed factory that builds a `BridgeWithShim` wrapper. Called with the
/// evaluated shim function and the private handler's global name (a plain
/// string, passed as data — never interpolated into code). The wrapper looks
/// the handler stub up on `globalThis` at call time, so the same wrapper works
/// whether it is built for a direct run (stub installed alongside it) or baked
/// into the instance (stub re-installed per call).
const SHIM_FACTORY_SRC: &str =
    "(shim, handlerName) => (async (...args) => await shim(await globalThis[handlerName](...args)))";

/// Names the runtime owns. A host global using one of these is rejected rather
/// than allowed to shadow it: replacing `Response` would leave user code
/// building objects the codec cannot recognise.
///
/// Keep in sync with `RUNTIME_OWNED_GLOBALS` in `names.ts`, the `HostGlobals` docs in
/// `packages/iso4-sandbox/src/types.ts` and DESIGN.md §4.2.
pub const RESERVED_GLOBAL_NAMES: &[&str] = &[
    "console",
    "waitUntil",
    "setTimeout",
    "clearTimeout",
    "Headers",
    "Request",
    "Response",
    "TextEncoder",
    "TextDecoder",
    "URL",
    "URLSearchParams",
    // Non-writable, non-configurable on the global object: a host global could never take effect.
    "NaN",
    "undefined",
    "Infinity",
    // Assigning it replaces a prototype instead of creating a property.
    "__proto__",
];

pub(super) fn check_not_reserved(name: &str) -> Result<(), RunError> {
    if RESERVED_GLOBAL_NAMES.contains(&name) {
        return Err(RunError::ReservedName(format!(
            "global '{name}' is reserved by the runtime and cannot be provided by the host"
        )));
    }
    Ok(())
}

/// Install the value globals — string expressions, data constants, and shim
/// wrappers — natively on the sandbox global object. Plain bridge stubs are
/// installed separately by [`super::bridge::install_bridge_globals`]; their defs are skipped
/// here.
///
/// Every public name reaches the global object through `object.set(key, value)`
/// — a plain string, never interpolated into an identifier position — so no
/// global name can shape generated source. String expressions and
/// shim expressions are evaluated as their own scripts with their own
/// filenames, so they never shift the line numbers of user code.
pub(super) fn install_value_globals(
    scope: &mut v8::PinScope,
    globals: &[HostGlobalDef],
) -> Result<(), RunError> {
    // Built lazily on the first shim global, then reused for the rest.
    let mut shim_factory: Option<v8::Local<v8::Function>> = None;

    for def in globals {
        match def {
            // Installed as a bridge stub by install_bridge_globals.
            HostGlobalDef::Bridge { .. } => {}
            HostGlobalDef::StringExpr {
                name,
                expr,
                enumerable,
            } => {
                check_not_reserved(name)?;
                let value = eval_global_expression(scope, expr, name)?;
                install_public_global(scope, name, value, *enumerable)?;
            }
            HostGlobalDef::Data {
                name,
                blob,
                enumerable,
            } => {
                check_not_reserved(name)?;
                // Web-aware: a data global is one of the two ways a host
                // Request/Response reaches the sandbox.
                let v8_value =
                    blob::deserialize_value_with_web_types(scope, blob).ok_or_else(|| {
                        match blob::take_codec_error() {
                            Some(e) => codec_error_to_run_error(e),
                            None => RunError::Internal(format!(
                                "failed to materialise data global '{name}'"
                            )),
                        }
                    })?;
                install_public_global(scope, name, v8_value, *enumerable)?;
            }
            HostGlobalDef::Shim {
                name,
                shim,
                handler_name,
                enumerable,
            } => {
                check_not_reserved(name)?;
                let factory = match shim_factory {
                    Some(f) => f,
                    None => {
                        let f = build_shim_factory(scope)?;
                        shim_factory = Some(f);
                        f
                    }
                };
                let shim_fn = eval_global_expression(scope, shim, name)?;
                let handler_key = v8::String::new(scope, handler_name).ok_or_else(|| {
                    RunError::Internal(format!("failed to intern shim handler name for '{name}'"))
                })?;
                let recv = v8::undefined(scope).into();
                let wrapper = factory
                    .call(scope, recv, &[shim_fn, handler_key.into()])
                    .ok_or_else(|| {
                        RunError::Internal(format!("failed to build shim wrapper for '{name}'"))
                    })?;
                install_public_global(scope, name, wrapper, *enumerable)?;
            }
        }
    }
    Ok(())
}

/// Install a host-declared value global, honoring its `enumerable` flag.
///
/// The enumerable path keeps the historical `[[Set]]` install (these run in a
/// context no sandbox code has touched yet); a non-enumerable one must go
/// through `DefineOwnProperty`, because `[[Set]]` always creates enumerable
/// properties.
fn install_public_global(
    scope: &mut v8::PinScope,
    name: &str,
    value: v8::Local<v8::Value>,
    enumerable: bool,
) -> Result<(), RunError> {
    if enumerable {
        set_global(scope, name, value)
    } else {
        define_global(scope, name, value, false)
    }
}

/// Names under this prefix are runtime plumbing (`__iso4_call`, the
/// `__iso4_<name>_h` shim handler keys, their placeholders) and install
/// non-enumerable, so enumeration-driven sandbox code (`for (const k in
/// globalThis)`, serializers, "copy the environment" patterns) never trips
/// over them — celld's ops went through exactly that accident before moving
/// to DONT_ENUM. The web classes already install non-enumerable
/// (`webtypes.js`); host-declared globals stay enumerable the way browsers
/// keep `fetch` enumerable. The names remain reachable by string — this is
/// hygiene, not secrecy.
const INTERNAL_GLOBAL_PREFIX: &str = "__iso4_";

/// Install a global via `DefineOwnProperty` — replaces any existing property
/// **without invoking accessors**. `set_global` goes through `[[Set]]`, which
/// runs a setter if sandbox code planted one under the name; on a reused
/// context (warm instances re-arming placeholders and stubs) that would hand
/// control to user code outside any guard. Fails when sandbox code made the
/// property non-configurable — callers treat that as an internal error, which
/// taints the instance.
///
/// Runtime-internal names ([`INTERNAL_GLOBAL_PREFIX`]) get `DONT_ENUM`
/// regardless of `enumerable`; for public names the flag is the host's
/// per-global opt-out.
pub(super) fn define_global(
    scope: &mut v8::PinScope,
    name: &str,
    value: v8::Local<v8::Value>,
    enumerable: bool,
) -> Result<(), RunError> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name)
        .ok_or_else(|| RunError::Internal(format!("failed to intern global name '{name}'")))?;
    let attributes = if name.starts_with(INTERNAL_GLOBAL_PREFIX) || !enumerable {
        v8::PropertyAttribute::DONT_ENUM
    } else {
        v8::PropertyAttribute::NONE
    };
    match global.define_own_property(scope, key.into(), value, attributes) {
        Some(true) => Ok(()),
        _ => Err(RunError::Internal(format!(
            "failed to define global '{name}' (made non-configurable by sandbox code?)"
        ))),
    }
}

/// Set `globalThis[name] = value` via the V8 API. The name is a property key,
/// never code.
fn set_global(
    scope: &mut v8::PinScope,
    name: &str,
    value: v8::Local<v8::Value>,
) -> Result<(), RunError> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name)
        .ok_or_else(|| RunError::Internal(format!("failed to intern global name '{name}'")))?;
    global
        .set(scope, key.into(), value)
        .ok_or_else(|| RunError::Internal(format!("failed to install global '{name}'")))?;
    Ok(())
}

/// Evaluate a host-provided global expression (`<expr>`, wrapped in parens so a
/// bare function/object expression parses) as its own script, tagged with a
/// dedicated filename so it never appears in a user-code stack trace.
fn eval_global_expression<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    expr: &str,
    name: &str,
) -> Result<v8::Local<'s, v8::Value>, RunError> {
    let wrapped = format!("({expr})");
    let filename = format!("<iso4:global:{name}>");
    eval_script(scope, &wrapped, &filename)
}

/// Build the shim-wrapper factory function from [`SHIM_FACTORY_SRC`].
fn build_shim_factory<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Function>, RunError> {
    let value = eval_script(scope, SHIM_FACTORY_SRC, "<iso4:shim-factory>")?;
    v8::Local::<v8::Function>::try_from(value)
        .map_err(|_| RunError::Internal("shim factory did not evaluate to a function".to_string()))
}

/// Compile and run `source` as a classic script under `filename`, returning the
/// completion value. Compile/runtime failures map to `RunError` carrying the
/// V8 exception. Runs inside its own `TryCatch`; the returned value lives in the
/// caller's handle scope.
pub(super) fn eval_script<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: &str,
    filename: &str,
) -> Result<v8::Local<'s, v8::Value>, RunError> {
    let source_str = v8::String::new(scope, source)
        .ok_or_else(|| RunError::Internal("failed to intern global source".to_string()))?;
    let filename_str = v8::String::new(scope, filename)
        .ok_or_else(|| RunError::Internal("failed to intern global filename".to_string()))?;
    // Classic script (is_module = false) — the last three ScriptOrigin bools
    // mirror the module origin built in run_module with is_module flipped off.
    let origin = v8::ScriptOrigin::new(
        scope,
        filename_str.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );
    v8::tc_scope!(let tc, scope);
    let script = match v8::Script::compile(tc, source_str, Some(&origin)) {
        Some(s) => s,
        None => return Err(syntax_error(tc)),
    };
    match script.run(tc) {
        Some(v) => Ok(v),
        None => Err(RunError::RuntimeError(Box::new(RuntimeErrorData {
            name: exception_name(tc),
            message: exception_message(tc),
            stack: exception_stack(tc),
            fields: exception_fields(tc),
        }))),
    }
}

/// Throw a plain Error into V8 with the given message.
/// Splits the string-creation, exception-creation, and throw into separate
/// borrows so the compiler doesn't see `scope` used twice simultaneously.
pub(super) fn throw_v8_error(scope: &mut v8::PinScope, message: &str) {
    if let Some(s) = v8::String::new(scope, message) {
        let exc = v8::Exception::error(scope, s);
        scope.throw_exception(exc);
    }
}

/// Reserved module specifier resolving to the built-in runtime module, which
/// re-exports `waitUntil`. Resolvable in setup code too (capturing the
/// function for later run-time use is legitimate); calling it during setup
/// throws.
pub(super) const ISO4_RUNTIME_SPECIFIER: &str = "iso4:runtime";

/// Global-registry symbol under which the native `waitUntil` is stashed for
/// the module re-export, so deleting the `waitUntil` global cannot break the
/// import. Must match `Symbol.for(...)` in [`ISO4_RUNTIME_MODULE_SRC`].
const ISO4_RUNTIME_WAITUNTIL_SYMBOL: &str = "iso4.runtime.waitUntil";

/// Private-symbol key holding the pristine `Promise.allSettled` (bound),
/// captured at install time — before any guest code runs — and called by the
/// grace loop. A guest replacing `Promise.allSettled` therefore sabotages
/// nothing: privates are unreachable from JS (the type-tag mechanism).
const GRACE_ALL_SETTLED_KEY: &str = "iso4::graceAllSettled";

fn grace_all_settled_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Private>> {
    let name = v8::String::new(scope, GRACE_ALL_SETTLED_KEY)?;
    Some(v8::Private::for_api(scope, Some(name)))
}

/// ESM source of the built-in `iso4:runtime` module.
pub(super) const ISO4_RUNTIME_MODULE_SRC: &str =
    "export const waitUntil = globalThis[Symbol.for('iso4.runtime.waitUntil')];\n";

/// Install the web runtime with streamed-body support. Snapshot creation only;
/// the stream natives find the run table through the isolate slot.
pub(crate) fn install_web_runtime(scope: &mut v8::PinScope) -> Result<(), String> {
    let read = v8::Function::builder(stream_read_callback)
        .build(scope)
        .ok_or_else(|| "failed to create streamRead".to_string())?;
    let cancel = v8::Function::builder(stream_cancel_callback)
        .build(scope)
        .ok_or_else(|| "failed to create streamCancel".to_string())?;
    crate::webtypes::install_with_streams(scope, read.into(), cancel.into())
}

/// A plain `Error` value for rejections built inside native callbacks.
pub(super) fn v8_error_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: &str,
) -> v8::Local<'s, v8::Value> {
    let msg = v8::String::new(scope, message).unwrap_or_else(|| v8::String::empty(scope));
    v8::Exception::type_error(scope, msg)
}

/// Install the `waitUntil` global: a native function pushing its
/// argument into the run's [`super::PendingWork`] set. Installed non-enumerable like
/// the web classes (convention), reserved as a name, and additionally
/// stashed under a registry symbol for the `iso4:runtime` re-export.
pub(crate) fn install_wait_until(scope: &mut v8::PinScope) -> Result<(), RunError> {
    let function = v8::Function::builder(wait_until_callback)
        .build(scope)
        .ok_or_else(|| RunError::Internal("failed to create waitUntil".to_string()))?;
    define_global(scope, "waitUntil", function.into(), false)?;

    let global = scope.get_current_context().global(scope);
    let desc = v8::String::new(scope, ISO4_RUNTIME_WAITUNTIL_SYMBOL)
        .ok_or_else(|| RunError::Internal("failed to intern waitUntil symbol".to_string()))?;
    let key = v8::Symbol::for_key(scope, desc);
    global
        .set(scope, key.into(), function.into())
        .ok_or_else(|| RunError::Internal("failed to stash waitUntil".to_string()))?;

    // Capture the pristine Promise.allSettled before any guest code exists,
    // into a private slot the grace loop calls directly from Rust.
    let all_settled = eval_script(
        scope,
        "(Promise.allSettled.bind(Promise))",
        "<iso4:runtime-install>",
    )?;
    let all_settled_key = grace_all_settled_key(scope)
        .ok_or_else(|| RunError::Internal("failed to intern allSettled key".to_string()))?;
    global
        .set_private(scope, all_settled_key, all_settled)
        .ok_or_else(|| RunError::Internal("failed to stash allSettled".to_string()))?;
    Ok(())
}

/// `waitUntil(value)` — register background work to be driven after the
/// run's value settles. Non-promise values are accepted and count as
/// already-settled work, mirroring `Promise.resolve` semantics.
pub(crate) fn wait_until_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state = slot_table(scope).map_or(std::ptr::null(), |table| attributed_state(scope, table));
    if state.is_null() {
        throw_v8_error(
            scope,
            "[iso4] waitUntil is only available to run code — setup evaluated at \
             prepare() time must finish its work within the warm-up budget",
        );
        return;
    }
    // SAFETY: entry pointers are valid for the remainder of the turn.
    let mut slot = unsafe { &*state }.pending.slot.borrow_mut();
    let Some(registered) = slot.as_mut() else {
        drop(slot);
        throw_v8_error(
            scope,
            "[iso4] waitUntil is only available to run code — setup evaluated at \
             prepare() time must finish its work within the warm-up budget",
        );
        return;
    };
    let value = args.get(0);
    let promise: v8::Local<v8::Promise> = match v8::Local::<v8::Promise>::try_from(value) {
        Ok(p) => p,
        Err(_) => {
            let Some(resolver) = v8::PromiseResolver::new(scope) else {
                drop(slot);
                throw_v8_error(scope, "[iso4] waitUntil could not register the value");
                return;
            };
            resolver.resolve(scope, value);
            resolver.get_promise(scope)
        }
    };
    registered.push(v8::Global::new(scope, promise));
    rv.set_undefined();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v8::testsupport::{get_default, run, run_err, run_ok};

    // ── Globals ───────────────────────────────────────────────────────────

    #[test]
    fn math_random_is_available() {
        // Math is a V8 built-in - no setup needed.
        let out = run_ok("export default typeof Math.random === 'function'");
        assert_eq!(get_default(&out).as_deref(), Some("true"));
    }

    #[test]
    fn json_parse_stringify_available() {
        let out = run_ok(r#"export default JSON.stringify({a:1})"#);
        // JSON.stringify returns a JS string - WireValue::String - displayed as-is.
        assert_eq!(get_default(&out).as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn unconfigured_fetch_is_just_a_missing_global_runtime_error() {
        let err = run_err(r#"export default await fetch("https://example.com")"#);
        assert!(matches!(err, RunError::RuntimeError(_)));
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
        assert!(matches!(err, RunError::RuntimeError(_)));
    }

    #[test]
    fn multiple_host_tools_available_as_globals() {
        // Several tools mounted as globals - search, fetch, summarize.
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
        assert!(matches!(err, RunError::RuntimeError(_)));
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

    // ── Reserved global names ─────────────────────────────────────────────

    #[test]
    fn set_timeout_and_clear_timeout_are_reserved_global_names() {
        assert!(RESERVED_GLOBAL_NAMES.contains(&"setTimeout"));
        assert!(RESERVED_GLOBAL_NAMES.contains(&"clearTimeout"));
    }

    #[test]
    fn non_writable_global_names_are_reserved() {
        for name in ["NaN", "undefined", "Infinity"] {
            assert!(
                matches!(&check_not_reserved(name), Err(RunError::ReservedName(m)) if m.contains("reserved")),
                "expected '{name}' to be reserved"
            );
        }
    }

    #[test]
    fn proto_is_a_reserved_global_name() {
        assert!(matches!(
            &check_not_reserved("__proto__"),
            Err(RunError::ReservedName(m)) if m.contains("reserved")
        ));
    }

    #[test]
    fn any_non_reserved_string_is_a_valid_global_name() {
        for name in ["my-tool", "user.lookup", "class", "default", "1abc", "", "café", "eval"] {
            assert!(check_not_reserved(name).is_ok(), "expected '{name}' to be allowed");
        }
        assert!(matches!(check_not_reserved("console"), Err(RunError::ReservedName(_))));
    }
}
