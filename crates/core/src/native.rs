//! The crate's native edge: the embedded snapshot and the C++ bindings.

/// The startup snapshot `build.rs` creates (see `snapshot.rs`).
pub(crate) static SNAPSHOT_BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/snapshot.bin"));

extern "C" {
    fn iso4_new_frozen_clock_platform(thread_pool_size: std::ffi::c_int) -> *mut ::v8::Platform;
    fn iso4_set_oom_error_handler(
        isolate: ::v8::UnsafeRawIsolatePtr,
        callback: unsafe extern "C" fn(*const std::ffi::c_char, &::v8::OomDetails, *mut std::ffi::c_void),
        data: *mut std::ffi::c_void,
    );
    fn iso4_set_fatal_memory_error_callback(callback: ::v8::OomErrorCallback);
}

/// The default platform with V8's wall clock routed to the entered isolate's
/// frozen clock (`src/clock_platform.cc`). `build.rs` defines its own stock
/// version of this function.
pub(crate) fn new_platform() -> ::v8::UniqueRef<::v8::Platform> {
    // `cores: Some(n)` sizes the pool n - 1, at least 1, at most 16; unset
    // passes 0, V8's own count.
    let pool = match crate::v8::cores_option() {
        None => 0,
        Some(n) => (n.get() - 1).clamp(1, 16) as std::ffi::c_int,
    };
    // SAFETY: a fresh heap-allocated platform the caller now owns.
    unsafe { ::v8::UniquePtr::from_raw(iso4_new_frozen_clock_platform(pool)) }.unwrap()
}

/// Registers the fatal OOM handler with `record` as its data
/// (`src/oom_handler.cc`), if a fatal hook is set. `build.rs` defines its own
/// no-op version.
///
/// # Safety
/// `record` must outlive the isolate: V8 reads it until the process aborts.
pub(crate) unsafe fn set_oom_error_handler(isolate: &mut ::v8::Isolate, record: &crate::v8::OomRecord) {
    if crate::v8::fatal_hook().is_none() {
        return;
    }
    unsafe {
        iso4_set_oom_error_handler(
            isolate.as_raw_isolate_ptr(),
            crate::v8::oom_error_cb,
            std::ptr::from_ref(record).cast_mut().cast(),
        );
    }
}

/// Registers V8's process-wide OOM and fatal error callbacks, if a fatal hook
/// is set. `build.rs` defines its own no-op version.
pub(crate) fn set_fatal_callbacks() {
    if crate::v8::fatal_hook().is_none() {
        return;
    }
    // SAFETY: stores a function pointer in a V8 global.
    unsafe { iso4_set_fatal_memory_error_callback(crate::v8::fatal_memory_error_cb) }
    ::v8::V8::set_fatal_error_handler(crate::v8::v8_fatal_error_cb);
}
