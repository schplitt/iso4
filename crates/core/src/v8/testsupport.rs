//! Test-only helpers shared by the engine's test modules: run shorthands,
//! export decoding, and the in-memory host end of a run's link.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use crate::link::{Addr, Inbound, Outbound};
use crate::testval::{self, TestValue as WireValue};
use crate::types;
use crate::types::{BridgeErrorPayload, HostGlobalDef, HostModuleNode, ImportBinding, ImportModule};
use crate::v8::{
    FailureOutput, Limits, Output, PrefixSpec, RoutedEvent, RunError, RunEvent, RunEventSender,
    RunIo, create_instance_core, execute, init_platform, run_module,
};

/// Run code with explicit limits. Used for limit-enforcement tests.
pub(crate) fn run_code(code: &str, filename: &str, limits: Limits) -> Result<Output, FailureOutput> {
    init_platform();
    run_module(
        code,
        filename,
        limits,
        &[],
        &[],
        RunIo::None,
        0,
        Arc::new(AtomicU32::new(0)),
        None,
        None,
        Arc::new(crate::link::NoopObserver),
    )
}

/// Shorthand: run a code string and return the full Output or RunError.
pub(crate) fn run(code: &str) -> Result<Output, RunError> {
    run_code(code, "<iso4>", Limits::default()).map_err(|failure| failure.error)
}

/// Build a source-form `ImportBinding` for tests.
pub(crate) fn source_import(specifier: &str, source: &str) -> ImportBinding {
    ImportBinding {
        specifier: specifier.to_string(),
        module: ImportModule::Source(source.to_string()),
    }
}

/// Build a host-form `ImportBinding` for tests.
pub(crate) fn host_import(specifier: &str, exports: Vec<(&str, HostModuleNode)>) -> ImportBinding {
    ImportBinding {
        specifier: specifier.to_string(),
        module: ImportModule::Host(
            exports
                .into_iter()
                .map(|(name, node)| (name.to_string(), node))
                .collect(),
        ),
    }
}

/// Run user code with a fixed set of source imports; no host bridge.
pub(crate) fn run_with_source_imports(code: &str, imports: &[ImportBinding]) -> Result<Output, RunError> {
    init_platform();
    run_module(
        code,
        "<iso4>",
        Limits::default(),
        &[],
        imports,
        RunIo::None,
        0,
        Arc::new(AtomicU32::new(0)),
        None,
        None,
        Arc::new(crate::link::NoopObserver),
    )
    .map_err(|failure| failure.error)
}

/// Shorthand: expect success and return the Output, panic otherwise.
pub(crate) fn run_ok(code: &str) -> Output {
    match run(code) {
        Ok(o) => o,
        Err(e) => panic!("expected Ok, got error: {e:?}"),
    }
}

/// Shorthand: expect a RunError, panic if it succeeds.
pub(crate) fn run_err(code: &str) -> RunError {
    match run(code) {
        Err(e) => e,
        Ok(o) => panic!("expected Err, got output: {o:?}"),
    }
}

pub(crate) fn has_line(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

// ── Value-shape test helpers ──────────────────────────────────────────────
//
// `Output::exports` is one V8 serialization blob holding the flat
// `{ name: value }` export object. These helpers decode it into the
// `TestValue` tree so test assertions can talk about value shapes.

/// Decode the exports blob into its value tree.
fn exports_of(out: &Output) -> WireValue {
    testval::from_blob(&out.exports)
}

/// Look up a field in the top-level exports Object by name.
pub(crate) fn get_field(out: &Output, key: &str) -> Option<WireValue> {
    if let WireValue::Object(fields) = exports_of(out) {
        fields.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
    } else {
        None
    }
}

/// Return the `default` export rendered as a display string, or `None`.
pub(crate) fn get_default(out: &Output) -> Option<String> {
    get_field(out, "default").map(|v| wire_to_display_str(&v))
}

/// Return a named export rendered as a display string, or `None`.
pub(crate) fn get_named(out: &Output, key: &str) -> Option<String> {
    get_field(out, key).map(|v| wire_to_display_str(&v))
}

/// Return all export key names.
pub(crate) fn export_keys(out: &Output) -> Vec<String> {
    if let WireValue::Object(fields) = exports_of(out) {
        fields.into_iter().map(|(k, _)| k).collect()
    } else {
        vec![]
    }
}

/// Convert a little-endian u64 word array plus a sign bit to a decimal
/// string. Used only in tests (avoids a `num-bigint` dependency).
///
/// Algorithm: repeated division by 10 processing words MSW→LSW,
/// collecting remainder digits until the quotient is all-zeros.
/// Each step works in u128 to handle the 2⁴² carry from word to word.
fn words_to_decimal(sign: bool, words: &[u64]) -> String {
    if words.is_empty() || words.iter().all(|&w| w == 0) {
        return "0".to_string();
    }
    let mut buf = words.to_vec(); // LSW at index 0
    let mut digits = String::new();
    loop {
        let mut rem: u128 = 0;
        for w in buf.iter_mut().rev() {
            let cur: u128 = (rem << 64) | (*w as u128);
            *w = (cur / 10) as u64;
            rem = cur % 10;
        }
        digits.push(char::from_digit(rem as u32, 10).unwrap());
        if buf.iter().all(|&w| w == 0) {
            break;
        }
    }
    if sign {
        digits.push('-');
    }
    digits.chars().rev().collect()
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
        WireValue::BigInt(sign, words) => words_to_decimal(*sign, words),
        WireValue::Bytes(_) => "[Uint8Array]".to_string(),
        // Real instances (Date/Map/Set/RegExp/Error/…) now cross the
        // boundary intact; the value tree renders them as a description.
        WireValue::Other(text) => text.clone(),
        WireValue::Cycle => "[Circular]".to_string(),
        WireValue::Array(items) => {
            let parts: Vec<String> = items.iter().map(wire_to_display_str).collect();
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

/// Evaluate `code` as a prefix on a cold-starting instance — the first
/// run's prefix stage, now that `prepare()` evaluates nothing.
pub(crate) fn evaluate_prefix(
    code: &str,
    globals: &[HostGlobalDef],
    imports: &[ImportBinding],
) -> Result<(), FailureOutput> {
    init_platform();
    create_instance_core(
        PrefixSpec {
            code,
            filename: "<prefix>",
            globals,
        },
        imports,
        0,
    )
    .map(|_| ())
}

/// The spec a run replays — mirroring what prefix.rs stores per prefix —
/// after checking that it evaluates.
pub(crate) fn prepared<'a>(
    code: &'a str,
    globals: &'a [HostGlobalDef],
    imports: &[ImportBinding],
) -> PrefixSpec<'a> {
    evaluate_prefix(code, globals, imports).expect("prefix must evaluate");
    PrefixSpec {
        code,
        filename: "<prefix>",
        globals,
    }
}

/// Direct-API grace defaults: the epilogue budget on, everything else
/// default.
pub(crate) fn grace_limits() -> Limits {
    Limits {
        grace_ms: 5_000,
        ..Limits::default()
    }
}

/// The host's end of a run's link: the run's outbound messages, and the
/// channel its events arrive on.
pub(crate) struct TestHost {
    out: crossbeam_channel::Receiver<(Addr, Outbound)>,
    events: RunEventSender,
}

impl TestHost {
    pub(crate) fn send(&self, msg: Inbound) {
        self.events
            .send(RoutedEvent::new(0, RunEvent::Host(msg)))
            .ok();
    }

    /// The next outbound message, or `None` after `timeout` of silence.
    /// A direct-API run addresses everything to run 0.
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Option<Outbound> {
        self.out.recv_timeout(timeout).ok().map(|(addr, msg)| {
            assert_eq!(addr, Addr::Run(0), "direct-API runs carry run id 0");
            msg
        })
    }

    /// The next outbound message; silence for 10 s fails the test.
    pub(crate) fn recv(&self) -> Outbound {
        self.recv_timeout(Duration::from_secs(10))
            .expect("the run sends a message")
    }
}

/// A one-off run's I/O over an in-memory link, plus the host's end of it.
pub(crate) fn test_io() -> (RunIo, TestHost) {
    let (link, out) = crate::link::channel_link();
    let (events_tx, events) = crossbeam_channel::unbounded();
    (
        RunIo::Channel { events, link },
        TestHost {
            out,
            events: events_tx,
        },
    )
}

/// The parts of a `BridgeCall`: callId, targetKind, specifier, name path,
/// decoded args.
pub(crate) fn bridge_call_parts(msg: Outbound) -> (u32, u8, Option<String>, Vec<String>, Vec<WireValue>) {
    let Outbound::BridgeCall {
        call_id,
        target,
        args,
    } = msg
    else {
        panic!("expected a BridgeCall, got {msg:?}");
    };
    let (kind, specifier, path) = match target {
        types::BridgeTarget::Global(name) => (0, None, vec![name]),
        types::BridgeTarget::Import { specifier, path } => (1, Some(specifier), path),
    };
    // The whole argument list is one value slot holding an array.
    let args = match testval::from_blob(&args) {
        WireValue::Array(items) => items,
        other => panic!("BridgeCall args must decode to an array, got {other:?}"),
    };
    (call_id, kind, specifier, path, args)
}

/// A successful BridgeResponse carrying `value`.
pub(crate) fn bridge_resp_ok(call_id: u32, value: &WireValue) -> Inbound {
    Inbound::BridgeResponse {
        call_id,
        result: Ok(Some(testval::to_blob(value))),
    }
}

/// An error BridgeResponse (name/message/fields).
pub(crate) fn bridge_resp_err_full(
    call_id: u32,
    name: &str,
    message: &str,
    fields: Option<&WireValue>,
) -> Inbound {
    Inbound::BridgeResponse {
        call_id,
        result: Err(BridgeErrorPayload {
            name: name.to_string(),
            message: message.to_string(),
            fields: fields.map(testval::to_blob),
        }),
    }
}

/// An error BridgeResponse with name `Error` and `message`.
pub(crate) fn bridge_resp_err(call_id: u32, message: &str) -> Inbound {
    bridge_resp_err_full(call_id, "Error", message, None)
}

/// Spawn a single-response bridge responder thread: waits for one
/// BridgeCall, calls `respond`, drops the host (closing the run's events).
pub(crate) fn spawn_responder(
    host: TestHost,
    respond: impl FnOnce(&TestHost) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = host.recv();
        respond(&host);
    })
}

/// Run code with one declared global over an in-memory link.
/// Returns the execute result and the responder JoinHandle.
pub(crate) fn run_with_bridge(
    code: &str,
    global: &str,
    limits: Limits,
    respond: impl FnOnce(&TestHost) + Send + 'static,
) -> (Result<Output, FailureOutput>, std::thread::JoinHandle<()>) {
    let (io, host) = test_io();
    let handle = spawn_responder(host, respond);
    let result = execute(
        code,
        None,
        limits,
        &[HostGlobalDef::bridge(global)],
        &[],
        io,
        Arc::new(AtomicU32::new(0)),
        None,
    );
    (result, handle)
}

/// Drain N BridgeCalls from `host`, responding to each with the given
/// WireValue. Returns the number of calls actually received before the
/// run goes quiet.
pub(crate) fn drain_bridge_calls(host: TestHost, max: usize, value: WireValue) -> usize {
    let mut count = 0;
    while count < max {
        match host.recv_timeout(Duration::from_millis(500)) {
            Some(Outbound::BridgeCall { call_id, .. }) => {
                host.send(bridge_resp_ok(call_id, &value));
                count += 1;
            }
            _ => break,
        }
    }
    count
}

/// Build a `CallSpec` with the args encoded as one blob holding an array.
pub(crate) fn call_spec(path: &str, args: &[WireValue]) -> types::CallSpec {
    types::CallSpec {
        export_path: path.to_string(),
        args_blob: testval::to_blob(&WireValue::Array(args.to_vec())),
    }
}

/// Shorthand: `run({ code, call })` with no globals and no socket.
pub(crate) fn run_call(code: &str, path: &str, args: &[WireValue]) -> Result<Output, RunError> {
    init_platform();
    let call = call_spec(path, args);
    run_module(
        code,
        "<iso4>",
        Limits::default(),
        &[],
        &[],
        RunIo::None,
        0,
        Arc::new(AtomicU32::new(0)),
        Some(&call),
        None,
        Arc::new(crate::link::NoopObserver),
    )
    .map_err(|failure| failure.error)
}

/// Decode a call run's value blob.
pub(crate) fn value_of(out: &Output) -> WireValue {
    testval::from_blob(&out.exports)
}
