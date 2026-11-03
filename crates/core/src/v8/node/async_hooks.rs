//! `node:async_hooks`: a minimal `AsyncLocalStorage` over V8's
//! continuation-preserved embedder data.

use crate::v8::RunError;
use crate::v8::runtable::{CURRENT_RUN_TOKEN, make_rider, rider_parts};

// A minimal, Node-compatible `AsyncLocalStorage` (`run` + `getStore`), surfaced
// to run and prefix code via `import { AsyncLocalStorage } from 'node:async_hooks'`.
//
// Mechanism: V8's *continuation-preserved embedder data* (CPED) — the same
// primitive modern Node's `AsyncContextFrame` rides on. V8 automatically saves
// the CPED slot with each promise continuation and restores it when that
// continuation runs, so an ambient value follows `async`/`await` chains without
// being threaded through call signatures, and concurrent chains stay isolated.
//
// The runtime installs two native functions (get/set CPED) and hands them to a
// small JS factory that returns the `AsyncLocalStorage` class closing over
// them. The class is stashed on `globalThis` under `Symbol.for(...)`; the
// built-in `node:async_hooks` module re-exports it. The native get/set are
// never exposed to user code — they live only in the factory closure.
//
// No promise hooks are registered, so runs that never construct an
// `AsyncLocalStorage` pay nothing beyond V8's own (cheap) CPED bookkeeping.
// The class is installed into the build-time startup snapshot, so prefix code
// may import it too; a frame set outside any run carries token 0.

/// Reserved module specifier that resolves to the built-in async-context module.
pub(crate) const ASYNC_HOOKS_SPECIFIER: &str = "node:async_hooks";

/// Global-registry symbol description under which the `AsyncLocalStorage` class
/// is stashed. Must match `Symbol.for(...)` in [`ASYNC_HOOKS_MODULE_SRC`].
const ASYNC_CONTEXT_SYMBOL: &str = "iso4.async_hooks.AsyncLocalStorage";

/// Classic script evaluated once, into the startup snapshot. Returns the `AsyncLocalStorage`
/// constructor, closing over the native `getContext`/`setContext` bindings.
///
/// Contexts are singly-linked frames (`{ p: parent, k: instance, v: value }`)
/// carried in the CPED slot. Reads walk the chain by instance identity. Only
/// object literals and own-property reads are used, so user code tampering with
/// builtin prototypes cannot subvert propagation. `run`'s `finally` restores the
/// parent for the synchronous tail; V8 restores the per-continuation frame for
/// the asynchronous tail.
const ASYNC_CONTEXT_FACTORY_SRC: &str = r#"
(function (getContext, setContext) {
  'use strict';
  class AsyncLocalStorage {
    run(store, callback, ...args) {
      const parent = getContext();
      const frame = { p: parent, k: this, v: store };
      setContext(frame);
      try {
        return callback(...args);
      } finally {
        setContext(parent);
      }
    }
    getStore() {
      let frame = getContext();
      while (frame != null) {
        if (frame.k === this) return frame.v;
        frame = frame.p;
      }
      return undefined;
    }
  }
  return AsyncLocalStorage;
})
"#;

/// ESM source of the built-in `node:async_hooks` module. Re-exports the class
/// installed by [`install_async_context`].
pub(crate) const ASYNC_HOOKS_MODULE_SRC: &str = "export const AsyncLocalStorage = \
    globalThis[Symbol.for('iso4.async_hooks.AsyncLocalStorage')];\n";

/// Native getter for the continuation-preserved embedder data slot: the
/// guest-visible value is the rider's inner frame.
pub(crate) fn async_context_get_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let data = scope.get_continuation_preserved_embedder_data();
    match rider_parts(scope, data) {
        Some((_, inner)) => rv.set(inner),
        None => rv.set_undefined(),
    }
}

/// Native setter for the continuation-preserved embedder data slot: wraps the
/// guest frame in a rider carrying the creating run's token (taken from the
/// current rider, so a cross-run continuation keeps ITS run, not the turn
/// owner's).
pub(crate) fn async_context_set_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let current = scope.get_continuation_preserved_embedder_data();
    let token = match rider_parts(scope, current) {
        Some((token, _)) => token,
        None => CURRENT_RUN_TOKEN.with(|c| c.get()),
    };
    let frame = args.get(0);
    let rider = make_rider(scope, token, frame);
    scope.set_continuation_preserved_embedder_data(rider.into());
    rv.set_undefined();
}

/// Install the `AsyncLocalStorage` class on `globalThis` under
/// `Symbol.for(ASYNC_CONTEXT_SYMBOL)`, at snapshot creation, so the built-in
/// `node:async_hooks` module can read it.
pub(crate) fn install_async_context(scope: &mut v8::PinScope) -> Result<(), RunError> {
    let get_fn = v8::Function::builder(async_context_get_callback)
        .build(scope)
        .ok_or_else(|| RunError::Internal("failed to build async-context getter".to_string()))?;
    let set_fn = v8::Function::builder(async_context_set_callback)
        .build(scope)
        .ok_or_else(|| RunError::Internal("failed to build async-context setter".to_string()))?;

    let factory_src = v8::String::new(scope, ASYNC_CONTEXT_FACTORY_SRC)
        .ok_or_else(|| RunError::Internal("failed to intern async-context factory".to_string()))?;
    let script = v8::Script::compile(scope, factory_src, None)
        .ok_or_else(|| RunError::Internal("failed to compile async-context factory".to_string()))?;
    let factory_val = script
        .run(scope)
        .ok_or_else(|| RunError::Internal("failed to run async-context factory".to_string()))?;
    let factory = v8::Local::<v8::Function>::try_from(factory_val)
        .map_err(|_| RunError::Internal("async-context factory is not a function".to_string()))?;

    let undefined = v8::undefined(scope).into();
    let class = factory
        .call(scope, undefined, &[get_fn.into(), set_fn.into()])
        .ok_or_else(|| RunError::Internal("async-context factory call failed".to_string()))?;

    let symbol_desc = v8::String::new(scope, ASYNC_CONTEXT_SYMBOL)
        .ok_or_else(|| RunError::Internal("failed to intern async-context symbol".to_string()))?;
    let symbol = v8::Symbol::for_key(scope, symbol_desc);
    let global = scope.get_current_context().global(scope);
    global
        .set(scope, symbol.into(), class)
        .ok_or_else(|| RunError::Internal("failed to install AsyncLocalStorage".to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use crate::testval::TestValue as WireValue;
    use crate::v8::testsupport::{call_spec, get_default, prepared, run, value_of};
    use crate::v8::{Limits, RunIo, execute_with_prefix};

    // ── Async context (AsyncLocalStorage) ─────────────────────────────────

    #[test]
    fn async_context_available_via_node_async_hooks() {
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            export default typeof AsyncLocalStorage;
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("function"));
    }

    #[test]
    fn async_context_propagates_across_awaits() {
        // The store set by `run` is visible several awaits deep, through a
        // nested async function that never received it as an argument.
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const als = new AsyncLocalStorage();
            async function deep() {
                await Promise.resolve();
                await Promise.resolve();
                return als.getStore();
            }
            export default await als.run('trace-42', async () => {
                await Promise.resolve();
                return deep();
            });
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("trace-42"));
    }

    #[test]
    fn async_context_getstore_undefined_outside_run() {
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const als = new AsyncLocalStorage();
            export default als.getStore() === undefined ? 'undef' : 'defined';
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("undef"));
    }

    #[test]
    fn async_context_nested_run_breadcrumb() {
        // The durable-workflow `step.do` pattern: each nested scope appends a
        // segment to the key, and the inner scope sees the accumulated path
        // while the outer scope is restored afterwards.
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const keyScope = new AsyncLocalStorage();
            function step(name, body) {
                const parent = keyScope.getStore() ?? '';
                const key = parent ? parent + '/' + name : name;
                return keyScope.run(key, body);
            }
            const seen = [];
            await step('charge', async () => {
                await step('validate', async () => {
                    await Promise.resolve();
                    seen.push(keyScope.getStore());
                });
                seen.push(keyScope.getStore());
            });
            export default seen.join(',');
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("charge/validate,charge"));
    }

    #[test]
    fn async_context_concurrent_branches_isolated() {
        // Two branches run concurrently with interleaved awaits; each must see
        // only its own store. This is the case a plain module variable gets
        // wrong.
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const als = new AsyncLocalStorage();
            async function branch(label) {
                return als.run(label, async () => {
                    await Promise.resolve();
                    await Promise.resolve();
                    const a = als.getStore();
                    await Promise.resolve();
                    const b = als.getStore();
                    return a + ':' + b;
                });
            }
            const [x, y] = await Promise.all([branch('A'), branch('B')]);
            export default x + '|' + y;
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("A:A|B:B"));
    }

    #[test]
    fn async_context_two_instances_independent() {
        let out = run(r#"
            import { AsyncLocalStorage } from 'node:async_hooks';
            const a = new AsyncLocalStorage();
            const b = new AsyncLocalStorage();
            export default await a.run('AA', async () => {
                return b.run('BB', async () => {
                    await Promise.resolve();
                    return a.getStore() + ',' + b.getStore();
                });
            });
        "#)
        .unwrap();
        assert_eq!(get_default(&out).as_deref(), Some("AA,BB"));
    }

    #[test]
    fn async_context_is_available_to_prefix_code_and_its_exports() {
        // workerd's shape: the instance is created at module scope and used
        // inside the handler, with the ambient value following awaits.
        let prefix = prepared(
            "import { AsyncLocalStorage } from 'node:async_hooks'\n\
             const als = new AsyncLocalStorage()\n\
             export default (key) => als.run(key, async () => { await Promise.resolve(); return als.getStore() })",
            &[],
            &[],
        );
        let out = execute_with_prefix(
            prefix,
            Limits::default(),
            &[],
            &[],
            RunIo::None,
            Arc::new(AtomicU32::new(0)),
            &call_spec("default", &[WireValue::String("wf".to_string())]),
        )
        .unwrap();
        assert_eq!(value_of(&out), WireValue::String("wf".to_string()));
    }
}
