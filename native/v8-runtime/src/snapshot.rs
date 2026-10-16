//! The build-time startup snapshot: one context with the runtime surface
//! (console, web types, `AsyncLocalStorage`, `waitUntil`, timers) already
//! installed. `build.rs` calls [`create`]; the binary embeds the blob and
//! every isolate boots from it, so no runtime JS is compiled per isolate.
//!
//! Every isolate in the process must use this snapshot: mixing it with V8's
//! stock snapshot in one process corrupts the shared read-only heap.

use std::borrow::Cow;
use std::sync::OnceLock;

use v8::MapFnTo;

use crate::v8::{RunTable, RunTableSlot};

/// Context-data index of the frozen-clock cell.
const CLOCK_CELL_INDEX: usize = 0;

/// Function addresses only, written once.
struct References(Vec<v8::ExternalReference>);
// SAFETY: the table holds code addresses and is never mutated after init.
unsafe impl Send for References {}
unsafe impl Sync for References {}

/// Every native callback installed into the snapshot context, plus the
/// external source string. Creation and
/// restore must pass the same table; V8 records each callback by its index.
fn external_references() -> &'static [v8::ExternalReference] {
    static REFS: OnceLock<References> = OnceLock::new();
    &REFS
        .get_or_init(|| {
            let functions: [v8::FunctionCallback; 15] = [
                crate::v8::console_stdout_callback.map_fn_to(),
                crate::v8::console_stderr_callback.map_fn_to(),
                crate::webtypes::headers_shell_ctor.map_fn_to(),
                crate::webtypes::request_shell_ctor.map_fn_to(),
                crate::webtypes::response_shell_ctor.map_fn_to(),
                crate::url::url_parse_callback.map_fn_to(),
                crate::url::url_set_callback.map_fn_to(),
                crate::v8::stream_read_callback.map_fn_to(),
                crate::v8::stream_cancel_callback.map_fn_to(),
                crate::v8::stamp_stream_body_callback.map_fn_to(),
                crate::v8::async_context_get_callback.map_fn_to(),
                crate::v8::async_context_set_callback.map_fn_to(),
                crate::v8::wait_until_callback.map_fn_to(),
                crate::v8::set_timeout_callback.map_fn_to(),
                crate::v8::clear_timeout_callback.map_fn_to(),
            ];
            // The webtypes.js source restores as an external string pointing
            // into the binary. Null-terminated up front, so CreateParams never
            // copies the table.
            References(
                functions
                    .into_iter()
                    .map(|function| v8::ExternalReference { function })
                    .chain([
                        v8::ExternalReference {
                            pointer: std::ptr::addr_of!(crate::webtypes::RUNTIME_JS).cast_mut().cast(),
                        },
                        v8::ExternalReference {
                            pointer: std::ptr::null_mut(),
                        },
                    ])
                    .collect(),
            )
        })
        .0
}

/// Isolate parameters that boot from the embedded snapshot.
pub fn create_params() -> v8::CreateParams {
    v8::Isolate::create_params()
        .snapshot_blob(v8::StartupData::from(crate::SNAPSHOT_BLOB))
        .external_references(Cow::Borrowed(external_references()))
}

/// Per-isolate setup of a context fresh from the snapshot: route the natives
/// to `table`, restart the frozen clock at the wall clock, and redo the two
/// steps that touch globals V8 adds only after restore.
pub fn boot_context(scope: &mut v8::PinScope, table: *const RunTable) -> Result<(), String> {
    scope.set_slot(RunTableSlot(table));
    let cell = scope
        .get_context_data_from_snapshot_once::<v8::Object>(CLOCK_CELL_INDEX)
        .map_err(|e| format!("snapshot clock cell missing: {e}"))?;
    crate::webtypes::adopt_clock(scope, cell)?;
    crate::webtypes::patch_temporal(scope)?;
    crate::webtypes::delete_removed_globals(scope)
}

/// Build the snapshot blob. Called by `build.rs` only: creating a snapshot in
/// a process that runs isolates is unsupported (DESIGN.md §11.6).
pub fn create() -> Vec<u8> {
    crate::v8::init_platform();
    let mut isolate =
        v8::Isolate::snapshot_creator(Some(Cow::Borrowed(external_references())), None);
    {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        crate::v8::install_console(scope).expect("install console");
        crate::v8::install_web_runtime(scope).expect("install web runtime");
        crate::v8::install_async_context(scope).expect("install AsyncLocalStorage");
        crate::v8::install_wait_until(scope).expect("install waitUntil");
        crate::v8::install_timers(scope).expect("install timers");
        let cell = crate::webtypes::clock_cell(scope).expect("frozen clock cell");
        let index = scope.add_context_data(context, cell);
        assert_eq!(index, CLOCK_CELL_INDEX);
        // A live Global handle aborts blob creation; restore re-adopts the cell.
        scope.remove_slot::<std::rc::Rc<crate::webtypes::FrozenClock>>();
        scope.set_default_context(context);
    }
    let blob = isolate
        .create_blob(v8::FunctionCodeHandling::Keep)
        .expect("create snapshot blob");
    // Otherwise every isolate would share the build machine's hash seed.
    assert!(blob.can_be_rehashed(), "snapshot is not rehashable");
    blob.to_vec()
}
