//! ESM modules: prefix evaluation, the module resolver for source and host
//! imports, host modules built natively from shape data, and export
//! extraction.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::blob;
use crate::errors::{codec_error_to_run_error, reason_error, termination_or};
use crate::types::{HostGlobalDef, HostModuleNode, ImportBinding, ImportModule};
use crate::v8::bridge::{
    host_module_base_id, install_prefix_bridge_placeholders, owned_bridge_error,
    prefix_bridge_call_from_value,
};
use crate::v8::globals::{
    ISO4_RUNTIME_MODULE_SRC, ISO4_RUNTIME_SPECIFIER, eval_script, install_value_globals,
};
use crate::v8::node::async_hooks::{ASYNC_HOOKS_MODULE_SRC, ASYNC_HOOKS_SPECIFIER};
use crate::v8::{
    RunError, RunFacts, RunState, RuntimeErrorData, ScanOutcome, build_output_and_maybe_grace,
    exception_fields, exception_message, exception_name, exception_stack, finished,
    runtime_error_from_value, syntax_error,
};

/// Extract and serialize a settled module's exports, then finish (or start
/// grace). The guards stay armed through extraction and serialization: V8's
/// ValueSerializer follows structured-clone semantics and invokes guest
/// getters on the properties it walks, so result serialization is guest
/// execution and must stay under the run's budgets — workerd does the same.
pub(super) fn extract_module_exports(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    facts: &RunFacts<'_>,
    rs: &mut RunState,
    evaluated_module: v8::Local<v8::Module>,
) -> ScanOutcome {
    let reason = facts.reason;
    // Serialization runs guest getters; nothing may run after a kill.
    if reason.get().is_some() {
        return finished(Err(rs.fail(reason_error(reason))));
    }

    let Some(namespace) = evaluated_module.get_module_namespace().to_object(scope) else {
        return finished(Err(rs.fail(termination_or(
            reason,
            RunError::Internal("module namespace is not an object".to_string()),
        ))));
    };

    let mut skipped_exports: Vec<String> = Vec::new();

    let Some(names) = namespace.get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
    else {
        return finished(Err(rs.fail(termination_or(
            reason,
            RunError::Internal("failed to read module export names".to_string()),
        ))));
    };

    // Copy the exports into a fresh plain object and serialize that **once**.
    // The module namespace is an exotic object the V8 serializer refuses, and
    // a single blob for all exports beats one blob per export (measured).
    //
    // Non-serializable exports are skipped, never fatal: a value the
    // pre-check refuses (function, unresolved Promise) is simply absent
    // from the result, its name reported in `skipped_exports` — this is
    // what lets `export default { fetch }` coexist with reading the
    // module's declaration exports.
    let exports_object = v8::Object::new(scope);
    let mut staged: Vec<(String, v8::Local<v8::Value>)> = Vec::new();

    for i in 0..names.length() {
        let Some(name_value) = names.get_index(scope, i) else {
            return finished(Err(rs.fail(termination_or(
                reason,
                RunError::Internal("failed to read export name".to_string()),
            ))));
        };
        let Some(name) = name_value
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
        else {
            return finished(Err(rs.fail(termination_or(
                reason,
                RunError::Internal("failed to stringify export name".to_string()),
            ))));
        };

        let Some(value) = namespace.get(scope, name_value) else {
            return finished(Err(rs.fail(termination_or(
                reason,
                RunError::Internal(format!("failed to read export {name}")),
            ))));
        };

        // Pre-check catches the common cases cheaply (a function or
        // Promise directly under the export name) without a trial
        // serialization.
        if check_export_serializable(&name, value).is_err() {
            skipped_exports.push(name);
            continue;
        }

        let Ok(export_key) = v8::Local::<v8::Name>::try_from(name_value) else {
            return finished(Err(rs.fail(termination_or(
                reason,
                RunError::Internal(format!("export name {name} is not a property key")),
            ))));
        };
        if exports_object
            .create_data_property(scope, export_key, value)
            .is_none()
        {
            return finished(Err(rs.fail(termination_or(
                reason,
                RunError::Internal(format!("failed to stage export {name}")),
            ))));
        }
        staged.push((name, value));
    }

    let exports = match blob::serialize_value(scope, exports_object.into()) {
        Ok(blob) => blob,
        Err(_first_message) => {
            // A getter that ended the run must not get a per-export retry.
            if reason.get().is_some() {
                return finished(Err(rs.fail(reason_error(reason))));
            }
            if let Some(err) = rs.bridge_error.get() {
                return finished(Err(rs.fail(owned_bridge_error(err))));
            }
            // A registered host type that cannot cross keeps its dedicated
            // loud diagnostic (stream bodies etc. must not vanish
            // silently); only generic clone refusals are skippable.
            if let Some(e) = blob::take_codec_error() {
                return finished(Err(
                    rs.fail(termination_or(reason, codec_error_to_run_error(e)))
                ));
            }
            // Something nested inside an export refuses to clone (e.g. the
            // function in `export default { fetch }`). Trial-serialize
            // each export and drop the offenders into `skipped_exports`.
            let retry_object = v8::Object::new(scope);
            for (name, value) in &staged {
                match blob::serialize_value(scope, *value) {
                    Ok(_) => {
                        let Some(key) = v8::String::new(scope, name) else {
                            return finished(Err(rs.fail(termination_or(
                                reason,
                                RunError::Internal("failed to intern export name".to_string()),
                            ))));
                        };
                        if retry_object
                            .create_data_property(scope, key.into(), *value)
                            .is_none()
                        {
                            return finished(Err(rs.fail(termination_or(
                                reason,
                                RunError::Internal(format!("failed to stage export {name}")),
                            ))));
                        }
                    }
                    Err(_) => {
                        if let Some(e) = blob::take_codec_error() {
                            return finished(Err(
                                rs.fail(termination_or(reason, codec_error_to_run_error(e)))
                            ));
                        }
                        skipped_exports.push(name.clone());
                    }
                }
            }
            match blob::serialize_value(scope, retry_object.into()) {
                Ok(blob) => blob,
                Err(retry_message) => {
                    return finished(Err(rs.fail(termination_or(
                        reason,
                        RunError::ExportNotSerializable(format!(
                            "exports could not be serialized: {retry_message}"
                        )),
                    ))));
                }
            }
        }
    };

    build_output_and_maybe_grace(scope, facts, rs, exports, skipped_exports)
}

/// Checkpoint bound for settling a prefix's top-level await. Every realistic
/// prefix settles after one checkpoint (a checkpoint drains the whole queue);
/// the bound exists so a never-settling promise fails cleanly instead of
/// looping. Checkpoints on an empty queue are near-free, so the headroom
/// costs nothing.
const PREFIX_SETTLE_MAX_CHECKPOINTS: usize = 1000;

/// Compile, instantiate, and evaluate prefix `code` as an ESM module in the
/// current context, returning the settled module once evaluation succeeds.
///
/// Bridge globals are not installed here (bridge stubs are recreated per
/// call). The returned handle is what a `prefix.call()` resolves its export
/// path against.
pub(super) fn evaluate_prefix_module(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    code: &str,
    filename: &str,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
) -> Result<v8::Global<v8::Module>, RunError> {
    // Install the value globals (string exprs, constants, shim wrappers)
    // before evaluating the prefix, so prefix code sees them. Bridge stubs
    // are NOT installed here — they are created against a live socket only
    // per call. Their names get throwing placeholders instead,
    // so a prefix that calls one fails with ERR_PREFIX_BRIDGE_CALL rather
    // than an accidental ReferenceError/TypeError.
    install_value_globals(scope, globals)?;
    install_prefix_bridge_placeholders(scope, globals, imports)?;

    let source_string = v8::String::new(scope, code)
        .ok_or_else(|| RunError::Internal("failed to intern module source".to_string()))?;
    let filename_str = v8::String::new(scope, filename)
        .ok_or_else(|| RunError::Internal("failed to intern filename".to_string()))?;
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
        true,
        None,
    );
    let mut source = v8::script_compiler::Source::new(source_string, Some(&origin));

    let module = match v8::script_compiler::compile_module(scope, &mut source) {
        Some(m) => m,
        None => return Err(syntax_error(scope)),
    };

    let _resolver_guard = install_resolver(ResolverContext {
        bindings: imports.to_vec(),
        module_cache: HashMap::new(),
        pending_meta: Vec::new(),
        resolve_error: None,
    });

    if module
        .instantiate_module(scope, module_resolver_callback)
        .is_none()
    {
        let err = take_resolver()
            .and_then(|ctx| ctx.resolve_error)
            .unwrap_or_else(|| RunError::Import(exception_message(scope)));
        return Err(err);
    }

    let evaluation = match module.evaluate(scope) {
        Some(v) => v,
        None => {
            if let Some(exception) = scope.exception() {
                if let Some(err) = prefix_bridge_call_from_value(scope, exception) {
                    return Err(err);
                }
            }
            return Err(RunError::RuntimeError(Box::new(RuntimeErrorData {
                name: exception_name(scope),
                message: exception_message(scope),
                stack: exception_stack(scope),
                fields: exception_fields(scope),
            })));
        }
    };

    if evaluation.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(evaluation).map_err(|_| {
            RunError::Internal("failed to inspect module evaluation promise".to_string())
        })?;
        // Any top-level `await` leaves the evaluation promise pending until
        // the microtask queue is drained; the isolate runs under
        // MicrotasksPolicy::Explicit, so drain it here. One checkpoint drains
        // the whole queue, chained awaits included, so it settles everything
        // that does not wait on an external event. The bound only caps a
        // promise that can never settle — the bridge does not exist while a
        // prefix evaluates, so awaited host I/O never resolves.
        for _ in 0..PREFIX_SETTLE_MAX_CHECKPOINTS {
            if !matches!(promise.state(), v8::PromiseState::Pending) {
                break;
            }
            scope.perform_microtask_checkpoint();
        }
        match promise.state() {
            v8::PromiseState::Fulfilled => {}
            v8::PromiseState::Rejected => {
                let rejection = promise.result(scope);
                if let Some(err) = prefix_bridge_call_from_value(scope, rejection) {
                    return Err(err);
                }
                return Err(runtime_error_from_value(scope, rejection));
            }
            v8::PromiseState::Pending => {
                return Err(RunError::PrefixDidNotSettle(
                    "prefix top-level await never settled: nothing in the isolate can \
                     resolve the awaited promise while a prefix evaluates"
                        .to_string(),
                ));
            }
        }
    }

    Ok(v8::Global::new(scope, module))
}

fn no_import_resolver<'a>(
    _context: v8::Local<'a, v8::Context>,
    _specifier: v8::Local<'a, v8::String>,
    _import_assertions: v8::Local<'a, v8::FixedArray>,
    _referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    None
}

// ── Module resolver (Phases 6 + 7) ──────────────────────────────────────────
//
// Two flavours per DESIGN.md §4.3:
//   - Source modules:  host supplies ESM source. We compile it with
//                      script_compiler::compile_module. The top-level
//                      instantiate_module recurses through our resolver to
//                      satisfy transitive imports, so source modules can
//                      themselves import other source modules.
//   - Host modules:    the public TS API accepts an object form, but it is lowered
//                      on the TS side to generated ESM source. By the time the
//                      binding reaches this resolver, it's still just (specifier, source).
//
// V8's module resolver callback has no user-data slot. The state lives in a
// thread-local that is set before `instantiate_module` and cleared once both
// `instantiate_module` and `evaluate` have returned (synthetic-module
// evaluation steps also dip into it). Each `run_module` invocation runs on
// one thread with one isolate, so this is safe.

pub(super) struct ResolverContext {
    /// Specifiers the host declared for this run, in wire-order. Source
    /// bindings carry ESM text compiled verbatim; host bindings carry the
    /// module shape as data, from which `build_host_module` constructs the
    /// module natively.
    pub(super) bindings: Vec<ImportBinding>,
    /// Per-isolate module cache so transitive imports resolve to the same
    /// `v8::Module` instance and import diamonds collapse correctly.
    pub(super) module_cache: HashMap<String, v8::Global<v8::Module>>,
    /// Values arrays staged for host modules, keyed by module identity. The
    /// import-meta callback ([`host_import_meta_callback`]) attaches the array
    /// as `import.meta.__iso4` when V8 initialises the module's meta object
    /// (lazily, during evaluation — still inside the resolver guard's scope).
    pub(super) pending_meta: Vec<(v8::Global<v8::Module>, v8::Global<v8::Array>)>,
    /// First resolver error wins. V8's callback ABI can't carry a
    /// `RunError`, so the caller inspects this after `instantiate_module`.
    pub(super) resolve_error: Option<RunError>,
}

thread_local! {
    static RESOLVER_CTX: RefCell<Option<ResolverContext>> = const { RefCell::new(None) };
}

/// Drop guard that takes the resolver context back out of the thread-local
/// when it goes out of scope, regardless of how the caller exits.
pub(super) struct ResolverGuard;
impl Drop for ResolverGuard {
    fn drop(&mut self) {
        RESOLVER_CTX.with(|c| {
            c.borrow_mut().take();
        });
    }
}

/// Install a resolver context for the duration of `f` (instantiate + evaluate).
/// Returns the context back so the caller can drain `pending_boxes` and inspect
/// `resolve_error`.
pub(super) fn install_resolver(ctx: ResolverContext) -> ResolverGuard {
    RESOLVER_CTX.with(|c| {
        *c.borrow_mut() = Some(ctx);
    });
    ResolverGuard
}

pub(super) fn take_resolver() -> Option<ResolverContext> {
    RESOLVER_CTX.with(|c| c.borrow_mut().take())
}

/// Find the wire binding for `specifier`, cloning the relevant fields so we
/// don't hold a borrow across `scope` calls.
fn lookup_binding(specifier: &str) -> Option<ImportBinding> {
    RESOLVER_CTX.with(|c| {
        c.borrow().as_ref().and_then(|ctx| {
            ctx.bindings
                .iter()
                .find(|b| b.specifier == specifier)
                .cloned()
        })
    })
}

fn cache_get(specifier: &str) -> Option<v8::Global<v8::Module>> {
    RESOLVER_CTX.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|ctx| ctx.module_cache.get(specifier).cloned())
    })
}

fn cache_put(specifier: String, module: v8::Global<v8::Module>) {
    RESOLVER_CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().as_mut() {
            ctx.module_cache.insert(specifier, module);
        }
    });
}

fn record_resolve_error(err: RunError) {
    RESOLVER_CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().as_mut() {
            if ctx.resolve_error.is_none() {
                ctx.resolve_error = Some(err);
            }
        }
    });
}

/// Stage the natively-built values array for a host module so the import-meta
/// callback can attach it when V8 initialises the module's `import.meta`.
fn stage_meta_values(module: v8::Global<v8::Module>, values: v8::Global<v8::Array>) {
    RESOLVER_CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().as_mut() {
            ctx.pending_meta.push((module, values));
        }
    });
}

/// The handle ID of the first function leaf of `specifier` — see
/// [`host_module_base_id`]; reads the bindings from the resolver context.
fn staged_base_id(specifier: &str) -> u32 {
    RESOLVER_CTX.with(|c| {
        c.borrow()
            .as_ref()
            .map(|ctx| host_module_base_id(&ctx.bindings, specifier))
            .unwrap_or(0)
    })
}

/// The module resolver V8 calls during `instantiate_module`.
///
/// An unknown specifier records `RunError::ModuleNotFound` and returns `None`
/// without a V8 exception; the instantiate path reports the recorded error.
pub(super) fn module_resolver_callback<'a>(
    context: v8::Local<'a, v8::Context>,
    specifier: v8::Local<'a, v8::String>,
    _import_assertions: v8::Local<'a, v8::FixedArray>,
    referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    v8::callback_scope!(unsafe let callback_scope, context);
    // `NewEscapableHandleScope` is implemented for a pinned `HandleScope`, not
    // for a pinned `CallbackScope`, so deref through to the handle scope the
    // callback scope bootstrapped.
    v8::escapable_handle_scope!(let scope, &mut **callback_scope);
    let specifier_str = specifier.to_rust_string_lossy(scope);

    if let Some(g) = cache_get(&specifier_str) {
        let local = v8::Local::new(scope, &g);
        return Some(scope.escape(local));
    }

    // Host-declared bindings take precedence, so a host that ships its own
    // `node:async_hooks` shim can still override the built-in. Only when no
    // binding matches do we fall back to the built-in async-context module.
    let binding = match lookup_binding(&specifier_str) {
        Some(b) => b,
        None if specifier_str == ASYNC_HOOKS_SPECIFIER => {
            ImportBinding {
                specifier: specifier_str.clone(),
                module: ImportModule::Source(ASYNC_HOOKS_MODULE_SRC.to_string()),
            }
        }
        None if specifier_str == ISO4_RUNTIME_SPECIFIER => ImportBinding {
            specifier: specifier_str.clone(),
            module: ImportModule::Source(ISO4_RUNTIME_MODULE_SRC.to_string()),
        },
        None => {
            // The root module is never cached, so a cached referrer is an import.
            let importer = RESOLVER_CTX.with(|c| {
                c.borrow().as_ref().and_then(|ctx| {
                    ctx.module_cache
                        .iter()
                        .find(|(_, m)| referrer == **m)
                        .map(|(name, _)| name.clone())
                })
            });
            record_resolve_error(RunError::ModuleNotFound(match importer {
                Some(importer) => {
                    format!("Cannot find module '{specifier_str}' imported from '{importer}'")
                }
                None => format!("Cannot find module '{specifier_str}'"),
            }));
            return None;
        }
    };
    let module = match &binding.module {
        ImportModule::Source(source) => compile_source_module(scope, &specifier_str, source)?,
        ImportModule::Host(exports) => build_host_module(scope, &specifier_str, exports)?,
    };

    let global = v8::Global::new(scope, module);
    cache_put(specifier_str, global);
    Some(scope.escape(module))
}

fn compile_source_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
    source: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    let source_string = v8::String::new(scope, source)?;
    let filename = v8::String::new(scope, specifier)?;
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
        true, // is_module
        None,
    );
    let mut source = v8::script_compiler::Source::new(source_string, Some(&origin));
    v8::tc_scope!(let tc, scope);
    match v8::script_compiler::compile_module(tc, &mut source) {
        Some(m) => Some(m),
        None => {
            let RunError::RuntimeError(mut data) = syntax_error(tc) else {
                record_resolve_error(syntax_error(tc));
                return None;
            };
            data.message = format!("failed to compile source import '{specifier}': {}", data.message);
            record_resolve_error(RunError::RuntimeError(data));
            None
        }
    }
}

// ── Host modules (built natively from shape data) ────────────────────────────
//
// A host module crosses the wire as data: named top-level exports, each a
// tree of function leaves and data leaves. No JS is ever generated from that
// data. The module the sandbox imports is a fixed-shape source-text module —
//
//     export const <name> = import.meta.__iso4[<i>];
//
// — where only identifier-validated export names and integer indices are
// interpolated. The values array is built natively (data leaves via
// `blob::deserialize_value`, function leaves as trampolines from a fixed factory
// with the handle ID passed as a number) and handed to the module through
// V8's import-meta callback. Using a plain source-text module keeps host
// modules snapshot-safe: everything reachable from a prefix snapshot is
// ordinary JS, with no native pointers that would need external references.

/// Property on `import.meta` carrying a host module's values array. Only
/// Rust-generated host-module source reads it; user modules get an empty
/// `import.meta` (the callback attaches nothing for unknown modules).
const IMPORT_META_VALUES_KEY: &str = "__iso4";

/// Fixed factory building the async trampoline for one function leaf. The
/// handle ID is passed as a number — never interpolated into source — and the
/// dispatcher (`BRIDGE_DISPATCH_GLOBAL`, a per-run bridge stub) is looked up
/// on globalThis at call time, so a trampoline baked into a prefix snapshot
/// still reaches the fresh stub of every later run.
const IMPORT_TRAMPOLINE_FACTORY_SRC: &str =
    "(id) => (async (...args) => await globalThis.__iso4_call(id, ...args))";

/// Build the module for a host import binding.
///
/// On failure records a resolver error (surfaced after `instantiate_module`)
/// and returns `None`, like `compile_source_module`.
fn build_host_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
    exports: &[(String, HostModuleNode)],
) -> Option<v8::Local<'s, v8::Module>> {
    // Defensive re-validation: the TS client already rejects invalid names,
    // but Rust is the side interpolating them into export positions, so it
    // owns the final check.
    for (name, _) in exports {
        if !is_valid_export_identifier(name) {
            record_resolve_error(RunError::Import(format!(
                "host module '{specifier}': top-level key '{name}' is not a valid \
                 JavaScript identifier and cannot be exported as a named ESM export"
            )));
            return None;
        }
    }

    let factory = match build_import_trampoline_factory(scope) {
        Ok(f) => f,
        Err(e) => {
            record_resolve_error(e);
            return None;
        }
    };

    // Handle IDs continue the global walk order established by
    // `collect_import_handles` over the declared bindings.
    let mut next_id = staged_base_id(specifier);
    let values = v8::Array::new(scope, exports.len() as i32);
    let mut source = String::new();
    for (i, (name, node)) in exports.iter().enumerate() {
        let value = match build_host_value(scope, specifier, node, factory, &mut next_id) {
            Ok(v) => v,
            Err(e) => {
                record_resolve_error(e);
                return None;
            }
        };
        values.set_index(scope, i as u32, value)?;
        if name == "default" {
            source.push_str(&format!(
                "export default import.meta.{IMPORT_META_VALUES_KEY}[{i}];\n"
            ));
        } else {
            source.push_str(&format!(
                "export const {name} = import.meta.{IMPORT_META_VALUES_KEY}[{i}];\n"
            ));
        }
    }

    let module = compile_source_module(scope, specifier, &source)?;
    stage_meta_values(
        v8::Global::new(scope, module),
        v8::Global::new(scope, values),
    );
    Some(module)
}

/// Build one host-module value natively: function leaves become trampolines
/// (consuming the next handle ID), data leaves are materialised from their
/// value blob, and object nodes become plain objects built via the V8 API.
fn build_host_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
    node: &HostModuleNode,
    factory: v8::Local<v8::Function>,
    next_id: &mut u32,
) -> Result<v8::Local<'s, v8::Value>, RunError> {
    match node {
        HostModuleNode::Function => {
            let id = *next_id;
            *next_id += 1;
            let id_arg = v8::Number::new(scope, id as f64).into();
            let recv = v8::undefined(scope).into();
            factory.call(scope, recv, &[id_arg]).ok_or_else(|| {
                RunError::Internal(format!(
                    "failed to build import trampoline in host module '{specifier}'"
                ))
            })
        }
        HostModuleNode::Data(bytes) => blob::deserialize_value(scope, bytes).ok_or_else(|| {
            RunError::Internal(format!(
                "failed to materialise data leaf in host module '{specifier}'"
            ))
        }),
        HostModuleNode::Object(entries) => {
            let obj = v8::Object::new(scope);
            for (key, child) in entries {
                let v = build_host_value(scope, specifier, child, factory, next_id)?;
                let k = v8::String::new(scope, key).ok_or_else(|| {
                    RunError::Internal(format!(
                        "failed to intern object key in host module '{specifier}'"
                    ))
                })?;
                obj.create_data_property(scope, k.into(), v);
            }
            Ok(obj.into())
        }
    }
}

/// Build the trampoline factory from [`IMPORT_TRAMPOLINE_FACTORY_SRC`].
fn build_import_trampoline_factory<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Function>, RunError> {
    let value = eval_script(
        scope,
        IMPORT_TRAMPOLINE_FACTORY_SRC,
        "<iso4:import-trampoline-factory>",
    )?;
    v8::Local::<v8::Function>::try_from(value).map_err(|_| {
        RunError::Internal("import trampoline factory did not evaluate to a function".to_string())
    })
}

/// Isolate-level callback V8 invokes the first time a module's `import.meta`
/// is accessed. Attaches the staged values array for host modules; any other
/// module (user code, source imports) gets an untouched empty meta object.
pub(super) extern "C" fn host_import_meta_callback(
    context: v8::Local<v8::Context>,
    module: v8::Local<v8::Module>,
    meta: v8::Local<v8::Object>,
) {
    // SAFETY: standard embedder-callback re-entry, same pattern as
    // `module_resolver_callback`.
    v8::callback_scope!(unsafe let scope, context);
    let values = RESOLVER_CTX.with(|c| {
        c.borrow().as_ref().and_then(|ctx| {
            ctx.pending_meta
                .iter()
                .find(|(m, _)| v8::Local::new(scope, m) == module)
                .map(|(_, v)| v.clone())
        })
    });
    let Some(values) = values else { return };
    let Some(key) = v8::String::new(scope, IMPORT_META_VALUES_KEY) else {
        return;
    };
    let array = v8::Local::new(scope, &values);
    meta.create_data_property(scope, key.into(), array.into());
}

/// Reserved words that cannot appear as named ESM exports. Mirrors the list
/// in the TS client (`RESERVED_WORDS` in `names.ts`), minus `default`.
const RESERVED_EXPORT_WORDS: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "let",
    "static",
    "enum",
    "await",
    "implements",
    "package",
    "protected",
    "interface",
    "private",
    "public",
    "null",
    "true",
    "false",
    "eval",
    "arguments",
];

/// Whether `name` can appear as a named ESM export in generated host-module
/// source: Unicode identifier rules plus a reserved-word filter. `default` is
/// allowed (emitted as `export default`). Mirrors `isValidExportName`
/// in the TS client (`names.ts`).
fn is_valid_export_identifier(name: &str) -> bool {
    if name == "default" {
        return true;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    // ID minus XID; V8 uses ID.
    let id_not_xid = |c: char| {
        matches!(c, '\u{037A}' | '\u{309B}' | '\u{309C}' | '\u{FC5E}'..='\u{FC63}'
            | '\u{FDFA}' | '\u{FDFB}' | '\u{FE70}' | '\u{FE72}' | '\u{FE74}' | '\u{FE76}'
            | '\u{FE78}' | '\u{FE7A}' | '\u{FE7C}' | '\u{FE7E}')
    };
    if !(unicode_ident::is_xid_start(first)
        || id_not_xid(first)
        || matches!(first, '\u{0E33}' | '\u{0EB3}' | '\u{FF9E}' | '\u{FF9F}')
        || first == '_'
        || first == '$')
    {
        return false;
    }
    if !chars.all(|c| {
        unicode_ident::is_xid_continue(c)
            || id_not_xid(c)
            || c == '$'
            || c == '\u{200C}'
            || c == '\u{200D}'
    }) {
        return false;
    }
    !RESERVED_EXPORT_WORDS.contains(&name)
}

/// Reject the two export shapes whose diagnostic is only useful with the
/// export name attached.
///
/// V8's serializer refuses functions and promises too, but with a generic
/// "could not be cloned" message. Everything else it accepts —
/// `Date`/`Map`/`Set`/`RegExp`/`Error`/`ArrayBuffer`/TypedArrays/`bigint`/
/// cycles all round-trip as real instances — so there is nothing else to
/// pre-check here.
fn check_export_serializable(name: &str, value: v8::Local<v8::Value>) -> Result<(), RunError> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use super::*;
    use crate::link::Outbound;
    use crate::testval::{self, TestValue as WireValue};
    use crate::types;
    use crate::v8::{FailureOutput, Limits, Output, PrefixSpec, execute, execute_with_prefix};
    use crate::v8::testsupport::{
        TestHost, bridge_call_parts, bridge_resp_ok, call_spec, evaluate_prefix, get_default,
        get_field, get_named, host_import, run_err, run_ok, run_with_source_imports, source_import,
        test_io, value_of,
    };

    // ── Export validation ─────────────────────────────────────────────────
    //
    // Non-serializable exports are skipped, never fatal: the value is
    // absent from the exports and its name is reported in `skipped_exports`.

    #[test]
    fn exporting_a_function_is_skipped_and_reported() {
        let out = run_ok("export default function() {}\nexport const kept = 1");
        assert_eq!(out.skipped_exports, vec!["default".to_string()]);
        assert!(get_field(&out, "default").is_none());
        assert_eq!(get_named(&out, "kept").as_deref(), Some("1"));
    }

    #[test]
    fn exporting_a_class_is_skipped_and_reported() {
        let out = run_ok("export default class Foo {}");
        assert_eq!(out.skipped_exports, vec!["default".to_string()]);
        assert!(get_field(&out, "default").is_none());
    }

    #[test]
    fn exporting_an_unresolved_promise_is_skipped_and_reported() {
        // A Promise that is never awaited cannot cross the boundary; it is
        // absent from the exports rather than failing the run.
        let out = run_ok("export default new Promise(() => {})");
        assert_eq!(out.skipped_exports, vec!["default".to_string()]);
    }

    #[test]
    fn nested_function_in_export_is_skipped_via_trial_serialization() {
        // The pre-check only sees the object; the nested function surfaces in
        // the combined serialize, which falls back to per-export trials.
        let out = run_ok(
            "export default { fetch() { return 1 } }\nexport const limits = { memoryMb: 64 }",
        );
        assert_eq!(out.skipped_exports, vec!["default".to_string()]);
        assert!(get_field(&out, "default").is_none());
        assert!(get_field(&out, "limits").is_some());
    }

    #[test]
    fn serializable_runs_report_no_skipped_exports() {
        let out = run_ok("export default 42");
        assert!(out.skipped_exports.is_empty());
    }

    #[test]
    fn exporting_non_plain_builtins_roundtrips_as_real_instances() {
        // The V8 serialization format carries these natively, so they arrive
        // as real instances rather than being rejected (the old wire codec)
        // or silently flattened to `{}`.
        for (code, expected) in [
            (
                "export default new Date(1700000000000)",
                "Date(1700000000000)",
            ),
            ("export default new Map([['a', 1]])", r#"Map([["a",1]])"#),
            ("export default new Set([1, 2, 3])", "Set([1,2,3])"),
            ("export default /abc/g", "RegExp(abc/g)"),
            (
                "export default new Uint8Array([7, 8]).buffer",
                "ArrayBuffer(7,8)",
            ),
            ("export default new TypeError('boom')", "TypeError(boom)"),
        ] {
            let out = run_ok(code);
            assert_eq!(get_default(&out).as_deref(), Some(expected), "{code}");
        }
    }

    #[test]
    fn exporting_a_nested_builtin_roundtrips() {
        // Nested case: `{ when: new Date() }` keeps the Date instance.
        let out = run_ok("export default { when: new Date(1700000000000) }");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Object(vec![(
                "when".to_string(),
                WireValue::Other("Date(1700000000000)".to_string()),
            )]))
        );
    }

    #[test]
    fn exporting_a_non_uint8_typed_array_roundtrips_with_its_element_type() {
        for (code, expected) in [
            (
                "export default new Float32Array([1, 2])",
                "Float32Array(1,2)",
            ),
            ("export default new Int32Array([1, 2])", "Int32Array(1,2)"),
            (
                "export default new DataView(new ArrayBuffer(4))",
                "DataView(0,0,0,0)",
            ),
        ] {
            let out = run_ok(code);
            assert_eq!(get_default(&out).as_deref(), Some(expected), "{code}");
        }
    }

    #[test]
    fn exporting_a_uint8_array_roundtrips_as_bytes() {
        let out = run_ok("export default new Uint8Array([1, 2, 3])");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Bytes(vec![1, 2, 3]))
        );
    }

    #[test]
    fn exporting_a_uint8_array_subarray_respects_byte_offset() {
        // A subarray view must serialize only its window, not the whole
        // backing buffer.
        let out =
            run_ok("const b = new Uint8Array([0, 1, 2, 3, 4]); export default b.subarray(1, 4)");
        assert_eq!(
            get_field(&out, "default"),
            Some(WireValue::Bytes(vec![1, 2, 3]))
        );
    }

    #[test]
    fn exporting_an_empty_uint8_array_roundtrips_as_empty_bytes() {
        let out = run_ok("export default new Uint8Array(0)");
        assert_eq!(get_field(&out, "default"), Some(WireValue::Bytes(vec![])));
    }

    // ── Imports ───────────────────────────────────────────────────────────
    // Real module resolver wired up: source modules compile through V8 directly,
    // host modules become synthetic v8::Modules whose exports are bridge stubs.
    // Host-import bridge call tests live in `bridge.rs` with the other
    // bridge-call integration tests because they need a host.

    #[test]
    fn unknown_import_is_module_not_found() {
        // With no host-declared bindings, every specifier is unknown.
        let err = run_err(r#"import { foo } from "unknown:module"; export default foo"#);
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    #[test]
    fn source_import_provided_by_host_works() {
        let imports = [source_import(
            "math:add",
            "export const add = (a, b) => a + b;",
        )];
        let out = run_with_source_imports(
            r#"import { add } from "math:add"; export default add(1, 2)"#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("3"));
    }

    #[test]
    fn source_import_unknown_specifier_with_declared_bindings_is_module_not_found() {
        // A binding for "math:add" exists, but the user imports "math:sub".
        let imports = [source_import(
            "math:add",
            "export const add = (a, b) => a + b;",
        )];
        let err = run_with_source_imports(
            r#"import { sub } from "math:sub"; export default sub(1, 2)"#,
            &imports,
        )
        .unwrap_err();
        assert!(matches!(err, RunError::ModuleNotFound(_)));
    }

    // ── Source modules (host-provided JS) ───────────────────────────────
    // The host supplies raw JS source for an import specifier. V8 compiles
    // it once per isolate and caches it. Phase 6+.

    const ZOD_SOURCE: &str = r#"
        export const z = {
          object: (shape) => ({
            parse: (data) => {
              for (const key of Object.keys(shape)) {
                if (!(key in data)) throw new Error(`missing key: ${key}`);
              }
              return data;
            }
          }),
          string: () => ({}),
          number: () => ({}),
        };
    "#;

    #[test]
    fn source_module_basic_function_import() {
        let imports = [source_import(
            "lib:math",
            "export function add(a, b) { return a + b; }",
        )];
        let out = run_with_source_imports(
            r#"import { add } from "lib:math"; export default add(1, 2)"#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("3"));
    }

    #[test]
    fn source_module_mimicking_zod_schema_validation() {
        // Canonical AI-agent use case: host ships a mini-zod as source.
        let imports = [source_import("lib:zod", ZOD_SOURCE)];
        let out = run_with_source_imports(
            r#"
            import { z } from "lib:zod";
            const schema = z.object({ name: z.string(), age: z.number() });
            export default schema.parse({ name: "Alice", age: 30 })
            "#,
            &imports,
        )
        .unwrap();
        let default_export = get_default(&out).expect("default export missing");
        assert!(default_export.contains("Alice"));
        assert!(default_export.contains("30"));
    }

    #[test]
    fn source_module_zod_schema_fails_on_bad_data() {
        // Same setup as above, but pass data that fails validation — the
        // schema's own `throw` should surface as RuntimeError, not as a
        // ModuleNotFound from the resolver.
        let imports = [source_import("lib:zod", ZOD_SOURCE)];
        let err = run_with_source_imports(
            r#"
            import { z } from "lib:zod";
            const schema = z.object({ name: z.string() });
            export default schema.parse({ wrong: true })
            "#,
            &imports,
        )
        .unwrap_err();
        assert!(
            matches!(err, RunError::RuntimeError(_)),
            "expected RuntimeError, got {err:?}"
        );
    }

    #[test]
    fn source_module_utility_library_used_across_multiple_exports() {
        // A utility lib imported and used in both default and named export.
        // Verifies the module cache works: the resolver returns the same
        // v8::Module instance for both `clamp` and `lerp` references.
        let imports = [source_import(
            "lib:math-utils",
            "export const clamp = (n, lo, hi) => Math.max(lo, Math.min(hi, n));\n\
             export const lerp = (a, b, t) => a + (b - a) * t;",
        )];
        let out = run_with_source_imports(
            r#"
            import { clamp, lerp } from "lib:math-utils";
            export default clamp(5, 0, 10);
            export const interpolated = lerp(0, 100, 0.5);
            "#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("5"));
        assert_eq!(get_named(&out, "interpolated").as_deref(), Some("50"));
    }

    #[test]
    fn source_module_can_import_another_source_module() {
        // Transitive source imports: "lib:app" imports from "lib:utils".
        // Module resolver must recurse into the second binding to satisfy
        // the first.
        let imports = [
            source_import("lib:utils", "export const double = (n) => n * 2;"),
            source_import(
                "lib:app",
                "import { double } from \"lib:utils\";\n\
                 export const formatResult = (n) => `result=${double(n)}`;",
            ),
        ];
        let out = run_with_source_imports(
            r#"import { formatResult } from "lib:app"; export default formatResult(42)"#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("result=84"));
    }

    // ── Prefixes ────────────────────────────────────────────────────────────

    #[test]
    fn prefix_evaluates_a_valid_prefix() {
        evaluate_prefix("const x = 1", &[], &[]).unwrap();
    }

    #[test]
    fn prefix_syntax_error_is_user_code() {
        let err = evaluate_prefix("export default (((", &[], &[]).unwrap_err();
        assert!(matches!(&err.error, RunError::RuntimeError(d) if d.name == "SyntaxError"), "got {:?}", err.error);
    }

    #[test]
    fn prefix_runtime_error_is_reported() {
        let err = evaluate_prefix(r#"throw new Error("prefix failed")"#, &[], &[]).unwrap_err();
        assert!(matches!(err.error, RunError::RuntimeError(_)));
    }

    #[test]
    fn prefix_unresolved_import_is_clean_error_not_crash() {
        // Regression (snapshot era): an unresolvable import used to segfault
        // (unconditional create_blob after a failed instantiate). The cold
        // start must return a clean ModuleNotFound.
        let err = evaluate_prefix("import x from 'totally-nonexistent-xyz'; export default x;", &[], &[])
        .unwrap_err();
        assert!(
            matches!(err.error, RunError::ModuleNotFound(_)),
            "expected ModuleNotFound, got {:?}",
            err.error
        );
    }

    #[test]
    fn prefix_with_globalthis_state_evaluates() {
        evaluate_prefix("globalThis.base = 100; export default 1;", &[], &[])
        .unwrap();
    }

    // ── Host modules built natively from shape data ────────────────────
    //
    // Host modules cross the wire as data trees; the runtime builds the module
    // itself. Data leaves need no socket. Function leaves dispatch through the
    // auto-installed `__iso4_call` stub, so those tests use a responder thread
    // like the bridge tests in `bridge.rs`.

    /// Run code with host imports over an in-memory link; the responder
    /// receives the first BridgeCall and answers through the host.
    fn run_with_host_imports(
        code: &str,
        imports: Vec<ImportBinding>,
        respond: impl FnOnce(&TestHost, Outbound) + Send + 'static,
    ) -> (Result<Output, FailureOutput>, std::thread::JoinHandle<()>) {
        let (io, host) = test_io();
        let handle = std::thread::spawn(move || {
            let msg = host.recv();
            respond(&host, msg);
        });
        let result = execute(
            code,
            None,
            Limits::default(),
            &[],
            &imports,
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        );
        (result, handle)
    }

    #[test]
    fn host_module_data_leaves_materialise_natively() {
        // Pure data host module — no socket, no bridge.
        let imports = [host_import(
            "conf:app",
            vec![
                (
                    "retries",
                    HostModuleNode::Data(testval::to_blob(&WireValue::Number(3.0))),
                ),
                (
                    "region",
                    HostModuleNode::Data(testval::to_blob(&WireValue::String("eu".to_string()))),
                ),
                (
                    "flags",
                    HostModuleNode::Data(testval::to_blob(&WireValue::Array(vec![
                        WireValue::Bool(true),
                        WireValue::Null,
                    ]))),
                ),
            ],
        )];
        let out = run_with_source_imports(
            r#"
            import { retries, region, flags } from "conf:app";
            export default `${retries}|${region}|${flags.length}|${flags[0]}`
            "#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("3|eu|2|true"));
    }

    #[test]
    fn host_module_default_and_nested_object_exports() {
        let imports = [host_import(
            "conf:shape",
            vec![
                (
                    "default",
                    HostModuleNode::Data(testval::to_blob(&WireValue::Number(7.0))),
                ),
                (
                    "nested",
                    HostModuleNode::Object(vec![(
                        "deep".to_string(),
                        HostModuleNode::Data(testval::to_blob(&WireValue::Object(vec![(
                            "x".to_string(),
                            WireValue::Number(1.0),
                        )]))),
                    )]),
                ),
            ],
        )];
        let out = run_with_source_imports(
            r#"
            import seven, { nested } from "conf:shape";
            export default seven + nested.deep.x
            "#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("8"));
    }

    #[test]
    fn host_module_function_leaf_dispatches_resolved_bridge_call() {
        let imports = vec![host_import(
            "tools:search",
            vec![("query", HostModuleNode::Function)],
        )];
        let (out, h) = run_with_host_imports(
            r#"
            import { query } from "tools:search";
            export default await query("cats", 2)
            "#,
            imports,
            |s, msg| {
                let (call_id, target_kind, specifier, export_name, args) = bridge_call_parts(msg);
                // The frame carries the resolved import target — the handle ID
                // never leaves the runtime.
                assert_eq!(target_kind, 1);
                assert_eq!(specifier.as_deref(), Some("tools:search"));
                assert_eq!(export_name, ["query"]);
                assert_eq!(
                    args,
                    vec![
                        WireValue::String("cats".to_string()),
                        WireValue::Number(2.0)
                    ]
                );
                s.send(bridge_resp_ok(call_id, &WireValue::Number(42.0)));
            },
        );
        h.join().unwrap();
        let out = out.unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("42"));
        // The bridge record carries the resolved public name.
        assert_eq!(out.bridge_calls.len(), 1);
        assert_eq!(out.bridge_calls[0].name, "tools:search.query");
        assert_eq!(out.bridge_calls[0].outcome, types::BridgeCallOutcome::Ok);
    }

    #[test]
    fn host_module_nested_function_leaf_and_second_module_offsets() {
        // Handle IDs are assigned across ALL declared bindings in walk order;
        // calling a leaf of the second module must resolve to the second
        // module's specifier and path.
        let imports = vec![
            host_import(
                "tools:a",
                vec![
                    ("first", HostModuleNode::Function),
                    (
                        "nested",
                        HostModuleNode::Object(vec![(
                            "inner".to_string(),
                            HostModuleNode::Function,
                        )]),
                    ),
                ],
            ),
            host_import("tools:b", vec![("second", HostModuleNode::Function)]),
        ];
        let (out, h) = run_with_host_imports(
            r#"
            import { second } from "tools:b";
            export default await second()
            "#,
            imports,
            |s, msg| {
                let (call_id, target_kind, specifier, export_name, _) = bridge_call_parts(msg);
                assert_eq!(target_kind, 1);
                assert_eq!(specifier.as_deref(), Some("tools:b"));
                assert_eq!(export_name, ["second"]);
                s.send(bridge_resp_ok(call_id, &WireValue::String("ok".to_string())));
            },
        );
        h.join().unwrap();
        let out = out.unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("ok"));
        assert_eq!(out.bridge_calls[0].name, "tools:b.second");
    }

    #[test]
    fn host_module_dotted_key_and_nested_twin_dispatch_by_segments() {
        let imports = vec![host_import(
            "api",
            vec![(
                "x",
                HostModuleNode::Object(vec![
                    ("a.b".to_string(), HostModuleNode::Function),
                    (
                        "a".to_string(),
                        HostModuleNode::Object(vec![("b".to_string(), HostModuleNode::Function)]),
                    ),
                ]),
            )],
        )];
        let (io, host) = test_io();
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let frame = host.recv();
                let (call_id, _, _, path, _) = bridge_call_parts(frame);
                host.send(bridge_resp_ok(call_id, &WireValue::String(path.join("/"))));
            }
        });
        let out = execute(
            r#"
            import { x } from "api";
            export default (await x["a.b"]()) + "|" + (await x.a.b())
            "#,
            None,
            Limits::default(),
            &[],
            &imports,
            io,
            Arc::new(AtomicU32::new(0)),
            None,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("x/a.b|x/a/b"));
    }

    #[test]
    fn host_module_invalid_export_name_is_import_error() {
        let imports = [host_import(
            "bad:name",
            vec![(
                "not a name",
                HostModuleNode::Data(testval::to_blob(&WireValue::Null)),
            )],
        )];
        let err = run_with_source_imports(
            r#"import * as x from "bad:name"; export default 1"#,
            &imports,
        )
        .unwrap_err();
        assert!(
            matches!(&err, RunError::Import(m) if m.contains("not a name")),
            "expected Import about the invalid key, got {err:?}"
        );
    }

    #[test]
    fn host_module_export_name_refuses_strict_mode_restricted_names() {
        assert!(!is_valid_export_identifier("eval"));
        assert!(!is_valid_export_identifier("arguments"));
        assert!(is_valid_export_identifier("evaluate"));
    }

    #[test]
    fn host_module_export_name_follows_unicode_identifier_rules() {
        for name in ["café", "π", "$", "_", "a\u{200C}b", "a\u{200D}b", "\u{309B}", "\u{0E33}", "a\u{FE70}"] {
            assert!(is_valid_export_identifier(name), "expected '{name}' to be valid");
        }
        for name in ["", "1abc", "my-tool", "\u{200C}a"] {
            assert!(!is_valid_export_identifier(name), "expected '{name}' to be invalid");
        }
    }

    #[test]
    fn host_module_direct_dispatcher_call_with_bad_handle_rejects_catchably() {
        // Sandbox code calling the reserved dispatcher directly with a bogus
        // handle gets a catchable rejection — not a hang, not a crash.
        let imports = vec![host_import(
            "tools:x",
            vec![("f", HostModuleNode::Function)],
        )];
        let (out, h) = run_with_host_imports(
            r#"
            import { f } from "tools:x";
            let caught = "no";
            try { await globalThis.__iso4_call(999) } catch (e) { caught = e.message }
            export default caught
            "#,
            imports,
            |_s, _payload| {
                // No BridgeCall frame is ever written for the bad handle; the
                // responder would block forever, so it must not be reached.
                // (This closure only runs if a frame arrives — fail loudly.)
                panic!("no BridgeCall frame expected for an invalid handle");
            },
        );
        let out = out.unwrap();
        let msg = get_default(&out).unwrap();
        assert!(
            msg.contains("no host import handle"),
            "expected catchable rejection message, got: {msg}"
        );
        // One blocked record for the refused attempt.
        assert_eq!(out.bridge_calls.len(), 1);
        assert_eq!(out.bridge_calls[0].outcome, types::BridgeCallOutcome::Blocked);
        // The responder thread never got a frame; drop it by closing our end.
        drop(h);
    }

    #[test]
    fn host_module_import_meta_is_not_visible_to_user_code() {
        // The values array rides on the HOST MODULE's import.meta only; user
        // code's own import.meta must stay empty.
        let imports = [host_import(
            "conf:x",
            vec![(
                "v",
                HostModuleNode::Data(testval::to_blob(&WireValue::Number(1.0))),
            )],
        )];
        let out = run_with_source_imports(
            r#"
            import { v } from "conf:x";
            export default `${v}|${JSON.stringify(import.meta.__iso4 ?? null)}`
            "#,
            &imports,
        )
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("1|null"));
    }

    #[test]
    fn prefix_with_host_module_data_and_stored_trampoline_works_per_run() {
        // The documented pattern: prefix imports a host module and stashes a
        // function leaf on globalThis. The trampoline and data leaves are
        // plain JS created at the instance's prefix stage; the call
        // dispatches through the fresh per-run `__iso4_call` stub.

        let imports = vec![host_import(
            "tools:search",
            vec![
                ("query", HostModuleNode::Function),
                (
                    "limit",
                    HostModuleNode::Data(testval::to_blob(&WireValue::Number(5.0))),
                ),
            ],
        )];
        let prefix_src = r#"
            import { query, limit } from "tools:search";
            globalThis.search = query;
            globalThis.maxResults = limit;
            export default async () => (await globalThis.search('dogs')) + globalThis.maxResults;
            "#;
        evaluate_prefix(prefix_src, &[], &imports).unwrap();

        let (io, host) = test_io();
        let handle = std::thread::spawn(move || {
            let frame = host.recv();
            let (call_id, target_kind, specifier, export_name, args) =
                bridge_call_parts(frame);
            assert_eq!(target_kind, 1);
            assert_eq!(specifier.as_deref(), Some("tools:search"));
            assert_eq!(export_name, ["query"]);
            assert_eq!(args, vec![WireValue::String("dogs".to_string())]);
            host.send(bridge_resp_ok(call_id, &WireValue::Number(3.0)));
        });

        let out = execute_with_prefix(
            PrefixSpec {
                code: prefix_src,
                filename: "<prefix>",
                globals: &[],
            },
            Limits::default(),
            &[],
            &imports,
            io,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(value_of(&out), WireValue::Number(8.0));
        assert_eq!(out.bridge_calls[0].name, "tools:search.query");
    }

    #[test]
    fn a_prefix_export_can_call_a_host_module_function_directly() {
        // The import binding itself (not a globalThis stash) is the call
        // path; the module is built in the instance from the declared shape.

        let imports = vec![host_import(
            "tools:t",
            vec![("f", HostModuleNode::Function)],
        )];
        let prefix_src = r#"import { f } from "tools:t"; export default async () => await f()"#;
        evaluate_prefix(prefix_src, &[], &imports).unwrap();

        let (io, host) = test_io();
        let handle = std::thread::spawn(move || {
            let frame = host.recv();
            let (call_id, _, specifier, export_name, _) = bridge_call_parts(frame);
            assert_eq!(specifier.as_deref(), Some("tools:t"));
            assert_eq!(export_name, ["f"]);
            host.send(bridge_resp_ok(call_id, &WireValue::Bool(true)));
        });

        let out = execute_with_prefix(
            PrefixSpec {
                code: prefix_src,
                filename: "<prefix>",
                globals: &[],
            },
            Limits::default(),
            &[],
            &imports,
            io,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[]),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(value_of(&out), WireValue::Bool(true));
    }
}
