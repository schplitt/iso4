//! Rust-side microbenchmarks (criterion, wall time).
//!
//! Run: `cargo bench --manifest-path native/v8-runtime/Cargo.toml`
//! CI runs with `-- --output-format bencher` so `scripts/bench-compare.ts`
//! can parse the results (see .github/workflows/bench.yml).
//!
//! Three groups:
//! - `codec` — the wire codec legs in isolation (no V8).
//! - `v8_value` — the value plane crossing into/out of V8: our
//!   `wire_to_v8_value`/`value_to_wire` against V8's own
//!   `ValueSerializer`/`ValueDeserializer` (the encoding we are moving to).
//! - `exec` — per-run execution costs: fresh context, snapshot restore,
//!   unique-source compile, and direct `Function::call` per event.
//!
//! The payload matrix mirrors `packages/iso4-sandbox/bench/payloads.ts`:
//! codec cost tracks value count, not bytes, so shapes are chosen by value
//! density (sparse1k / dense1k / dense2m / bytes2m).
//!
//! `ISO4_BENCH_PROFILE=pr` trims sample counts for quick local runs; CI
//! always uses the full profile so baseline and PR numbers are collected
//! the same way.
//!
//! Sources for compile benches MUST vary per iteration: identical source
//! hits V8's compile cache and reports a ~5x-too-fast number.

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use iso4_v8_runtime::v8::{init_platform, value_to_wire, wire_to_v8_value};
use iso4_v8_runtime::wire::{decode_wire_value, encode_wire_value, WireValue};
use v8::{ValueDeserializerHelper, ValueSerializerHelper};

// ── Deterministic payload fixtures ─────────────────────────────────────────

/// Mulberry32 — same generator and seeds as `bench/payloads.ts`, so both
/// suites bench structurally identical data.
fn prng(seed: u32) -> impl FnMut() -> f64 {
    let mut s = seed;
    move || {
        s = s.wrapping_add(0x6D2B_79F5);
        let mut t = s;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

const WORDS: [&str; 10] = [
    "checkout", "session", "purchase", "pageview", "signup", "tenant", "campaign", "variant",
    "mobile", "desktop",
];

fn sentence(rand: &mut impl FnMut() -> f64, words: usize) -> String {
    (0..words)
        .map(|_| WORDS[(rand() * WORDS.len() as f64) as usize])
        .collect::<Vec<_>>()
        .join(" ")
}

fn obj(fields: Vec<(&str, WireValue)>) -> WireValue {
    WireValue::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// ~10 keys, long string values, ~750 B — realistic analytics event.
fn sparse1k() -> WireValue {
    let mut rand = prng(0x1504);
    let event_id = format!("evt_{}", (rand() * 1e9) as u64);
    let url = format!(
        "https://app.example.com/dashboards/{}?utm_source=newsletter&utm_campaign=q3-launch",
        (rand() * 1e6) as u64
    );
    let referrer = format!(
        "https://www.example.com/search?q={}",
        sentence(&mut rand, 8).replace(' ', "+")
    );
    let description = sentence(&mut rand, 40);
    obj(vec![
        ("eventId", WireValue::String(event_id)),
        ("tenantId", WireValue::String("tenant_4c1f9a2b".into())),
        ("type", WireValue::String("analytics.pageview".into())),
        ("url", WireValue::String(url)),
        (
            "userAgent",
            WireValue::String(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
                    .into(),
            ),
        ),
        ("referrer", WireValue::String(referrer)),
        ("description", WireValue::String(description)),
        (
            "timestamp",
            WireValue::Number(1_722_945_600_000.0 + (rand() * 86_400_000.0).floor()),
        ),
        ("sessionDurationMs", WireValue::Number(rand() * 900_000.0)),
        ("isAuthenticated", WireValue::Bool(true)),
    ])
}

/// ~200 values, ~1.3 KB — value-dense but small.
fn dense1k() -> WireValue {
    let mut rand = prng(0xD513);
    let metrics = WireValue::Object(
        (0..96)
            .map(|i| (format!("m{i}"), WireValue::Number(rand() * 1000.0)))
            .collect(),
    );
    let tags = WireValue::Array(
        (0..48)
            .map(|_| WireValue::String(WORDS[(rand() * WORDS.len() as f64) as usize].into()))
            .collect(),
    );
    let flags = WireValue::Array((0..48).map(|_| WireValue::Bool(rand() > 0.5)).collect());
    obj(vec![
        ("kind", WireValue::String("metrics.batch".into())),
        ("metrics", metrics),
        ("tags", tags),
        ("flags", flags),
    ])
}

/// 12k rows × 4 fields (~48k values, ~0.5–0.7 MB) — value-dense and large.
fn dense2m() -> WireValue {
    let mut rand = prng(0xDE2E);
    let rows = WireValue::Array(
        (0..12_000)
            .map(|i| {
                let name = format!(
                    "{}_{}",
                    WORDS[(i as usize) % WORDS.len()],
                    (rand() * 1e6) as u64
                );
                obj(vec![
                    ("id", WireValue::Number(f64::from(i))),
                    ("name", WireValue::String(name)),
                    ("value", WireValue::Number(rand() * 10_000.0)),
                    ("active", WireValue::Bool(rand() > 0.3)),
                ])
            })
            .collect(),
    );
    obj(vec![
        ("kind", WireValue::String("rows.batch".into())),
        ("rows", rows),
    ])
}

/// One 2 MB byte buffer — bytes plane.
fn bytes2m() -> WireValue {
    let mut rand = prng(0xB2E5);
    let mut buf = vec![0u8; 2 * 1024 * 1024];
    for chunk in buf.chunks_exact_mut(4) {
        let n = (rand() * 4_294_967_296.0) as u32;
        chunk.copy_from_slice(&n.to_le_bytes());
    }
    WireValue::Bytes(buf)
}

fn shapes() -> Vec<(&'static str, WireValue)> {
    vec![
        ("sparse1k", sparse1k()),
        ("dense1k", dense1k()),
        ("dense2m", dense2m()),
        ("bytes2m", bytes2m()),
    ]
}

// ── V8 serializer delegates (minimal: no host objects in the matrix) ───────

struct SerDelegate;
impl v8::ValueSerializerImpl for SerDelegate {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::HandleScope<'s>,
        message: v8::Local<'s, v8::String>,
    ) {
        let exc = v8::Exception::error(scope, message);
        scope.throw_exception(exc);
    }
}

struct DeserDelegate;
impl v8::ValueDeserializerImpl for DeserDelegate {}

// ── Groups ─────────────────────────────────────────────────────────────────

/// Wire codec legs in isolation — no V8 involved.
fn bench_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec");
    for (name, wv) in &shapes() {
        group.bench_function(BenchmarkId::new("encode_wire_value", name), |b| {
            b.iter(|| {
                let mut out = Vec::new();
                encode_wire_value(black_box(wv), &mut out);
                black_box(out)
            });
        });

        let mut encoded = Vec::new();
        encode_wire_value(wv, &mut encoded);
        group.bench_function(BenchmarkId::new("decode_wire_value", name), |b| {
            b.iter(|| {
                let mut offset = 0usize;
                black_box(decode_wire_value(black_box(&encoded), &mut offset).unwrap())
            });
        });
    }
    group.finish();
}

/// The value plane crossing the V8 boundary, per shape:
/// our codec's V8 legs vs `ValueSerializer`/`ValueDeserializer`.
fn bench_v8_value(c: &mut Criterion) {
    init_platform();
    let mut group = c.benchmark_group("v8_value");

    for (name, wv) in &shapes() {
        // WireValue → V8 object graph (current inbound leg).
        group.bench_function(BenchmarkId::new("wire_to_v8_value", name), |b| {
            let isolate = &mut v8::Isolate::new(Default::default());
            let scope = &mut v8::HandleScope::new(isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            b.iter(|| {
                // Per-iteration handle scope: without it, millions of locals
                // accumulate in the outer scope over a bench run.
                let scope = &mut v8::HandleScope::new(scope);
                black_box(wire_to_v8_value(scope, black_box(wv)).unwrap());
            });
        });

        // V8 object graph → WireValue (current outbound leg).
        group.bench_function(BenchmarkId::new("value_to_wire", name), |b| {
            let isolate = &mut v8::Isolate::new(Default::default());
            let scope = &mut v8::HandleScope::new(isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let value = wire_to_v8_value(scope, wv).unwrap();
            b.iter(|| {
                let scope = &mut v8::HandleScope::new(scope);
                let scope = &mut v8::TryCatch::new(scope);
                let mut visiting = Vec::new();
                black_box(value_to_wire(scope, black_box(value), &mut visiting).unwrap());
            });
        });

        // V8 value → v8-blob bytes (the outbound leg we are moving to).
        group.bench_function(BenchmarkId::new("value_serializer", name), |b| {
            let isolate = &mut v8::Isolate::new(Default::default());
            let scope = &mut v8::HandleScope::new(isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let value = wire_to_v8_value(scope, wv).unwrap();
            b.iter(|| {
                let scope = &mut v8::HandleScope::new(scope);
                let serializer = v8::ValueSerializer::new(scope, Box::new(SerDelegate));
                serializer.write_header();
                assert!(serializer.write_value(context, value) == Some(true));
                black_box(serializer.release())
            });
        });

        // v8-blob bytes → V8 value (the inbound leg we are moving to).
        // read_header() before read_value() is mandatory — see
        // docs/protocol.md and the misleading host-object error it prevents.
        group.bench_function(BenchmarkId::new("value_deserializer", name), |b| {
            let isolate = &mut v8::Isolate::new(Default::default());
            let scope = &mut v8::HandleScope::new(isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let value = wire_to_v8_value(scope, wv).unwrap();
            let serializer = v8::ValueSerializer::new(scope, Box::new(SerDelegate));
            serializer.write_header();
            assert!(serializer.write_value(context, value) == Some(true));
            let blob = serializer.release();
            b.iter(|| {
                let scope = &mut v8::HandleScope::new(scope);
                let deserializer =
                    v8::ValueDeserializer::new(scope, Box::new(DeserDelegate), &blob);
                assert!(deserializer.read_header(context) == Some(true));
                black_box(deserializer.read_value(context).unwrap());
            });
        });

    }
    group.finish();
}

/// ~150 B event for the RunCall-pattern benches, as a WireValue so no JSON
/// is involved anywhere (the value plane is the v8 blob — decided item 1).
fn small_event() -> WireValue {
    obj(vec![
        ("id", WireValue::Number(8412.0)),
        ("type", WireValue::String("analytics.pageview".into())),
        ("tenant", WireValue::String("tenant_4c1f9a2b".into())),
        ("name", WireValue::String("checkout session".into())),
        ("value", WireValue::Number(42.5)),
        ("ts", WireValue::Number(1_722_945_600_000.0)),
        ("authenticated", WireValue::Bool(true)),
    ])
}

const TRANSFORM_SRC: &str =
    "(e) => ({ id: e.id, tenant: e.tenant, revenue: e.value * 1.15, tag: e.name })";

/// Per-run execution costs: the fixed tax decomposition from the backlog
/// (context create dominates; snapshot restore, compile, call are the rest).
fn bench_exec(c: &mut Criterion) {
    init_platform();
    let mut group = c.benchmark_group("exec");

    // Fresh context in an existing isolate (~190 µs in the 2026-08 session;
    // the dominant slice of the per-run tax — see backlog item 3).
    group.bench_function("context_create", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        let scope = &mut v8::HandleScope::new(isolate);
        b.iter(|| {
            let scope = &mut v8::HandleScope::new(scope);
            black_box(v8::Context::new(scope, Default::default()));
        });
    });

    // Fresh isolate + context restored from a startup snapshot — the
    // prefix.run() entry cost. blob.clone() (a ~few-hundred-KB memcpy) is
    // part of the measured region; it is also part of the real per-run path
    // since CreateParams takes an owned blob.
    group.bench_function("snapshot_restore", |b| {
        let blob = make_snapshot_blob();
        b.iter(|| {
            let params = v8::Isolate::create_params().snapshot_blob(blob.clone());
            let isolate = &mut v8::Isolate::new(params);
            let scope = &mut v8::HandleScope::new(isolate);
            let context = v8::Context::new(scope, Default::default());
            black_box(context);
        });
    });

    // Compile + run a script with inlined data. The source MUST be unique
    // per iteration: identical source hits V8's compile cache and reports
    // ~5x too fast (measured 2.9 µs cached vs 15.6 µs unique).
    group.bench_function("compile_run_unique_source", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        let scope = &mut v8::HandleScope::new(isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let mut n: u64 = 0;
        b.iter(|| {
            n += 1;
            let scope = &mut v8::HandleScope::new(scope);
            let src = format!(
                "const e{n} = {{ id: {n}, type: 'analytics.pageview', value: {n}.5, ts: 1722945600000 }}; e{n}.id"
            );
            let code = v8::String::new(scope, &src).unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();
            black_box(script.run(scope).unwrap());
        });
    });

    // Direct call of a pre-compiled function — the RunCall target pattern
    // (backlog item 2): no per-event compile, argument already materialised.
    group.bench_function("function_call", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        let scope = &mut v8::HandleScope::new(isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let (func, recv) = compile_transform(scope);
        let event = wire_to_v8_value(scope, &small_event()).unwrap();
        b.iter(|| {
            let scope = &mut v8::HandleScope::new(scope);
            black_box(func.call(scope, recv, &[event]).unwrap());
        });
    });

    // The full RunCall per-event pattern with the v8-blob value plane
    // (item 1 + item 2): argsBlob in → ValueDeserializer → call() →
    // ValueSerializer → result blob out.
    group.bench_function("call_per_event", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        let scope = &mut v8::HandleScope::new(isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let (func, recv) = compile_transform(scope);
        let event = wire_to_v8_value(scope, &small_event()).unwrap();
        let serializer = v8::ValueSerializer::new(scope, Box::new(SerDelegate));
        serializer.write_header();
        assert!(serializer.write_value(context, event) == Some(true));
        let args_blob = serializer.release();
        b.iter(|| {
            let scope = &mut v8::HandleScope::new(scope);
            let deserializer =
                v8::ValueDeserializer::new(scope, Box::new(DeserDelegate), &args_blob);
            assert!(deserializer.read_header(context) == Some(true));
            let event = deserializer.read_value(context).unwrap();
            let result = func.call(scope, recv, &[event]).unwrap();
            let serializer = v8::ValueSerializer::new(scope, Box::new(SerDelegate));
            serializer.write_header();
            assert!(serializer.write_value(context, result) == Some(true));
            black_box(serializer.release());
        });
    });

    group.finish();
}

/// Evaluate `TRANSFORM_SRC` and hand back the function plus an `undefined`
/// receiver for `Function::call`.
fn compile_transform<'s>(
    scope: &mut v8::HandleScope<'s>,
) -> (v8::Local<'s, v8::Function>, v8::Local<'s, v8::Value>) {
    let code = v8::String::new(scope, TRANSFORM_SRC).unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    let func: v8::Local<v8::Function> = script.run(scope).unwrap().try_into().unwrap();
    let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
    (func, recv)
}

/// Build a small startup snapshot the way `precompile` does: evaluate a
/// prefix in a snapshot-creator isolate, mark its context as default,
/// create the blob.
fn make_snapshot_blob() -> Vec<u8> {
    let mut isolate = v8::Isolate::snapshot_creator(None, None);
    {
        let scope = &mut v8::HandleScope::new(&mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        scope.set_default_context(context);
        let code =
            v8::String::new(scope, "globalThis.__prefix = { ready: true, seq: 0 };").unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        script.run(scope).unwrap();
    }
    isolate
        .create_blob(v8::FunctionCodeHandling::Keep)
        .expect("snapshot blob")
        .to_vec()
}

// ── Criterion config ───────────────────────────────────────────────────────

fn configured() -> Criterion {
    let mut c = Criterion::default()
        // Bounded measurement windows keep the full suite in single-digit
        // minutes on CI; criterion warns when a heavy bench (dense2m,
        // snapshot_restore) can't fit the sample count and proceeds anyway.
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1));
    if std::env::var("ISO4_BENCH_PROFILE").as_deref() == Ok("pr") {
        // Quick local profile — a same-machine A/B ratio stabilises with
        // far fewer samples than a stored baseline needs.
        c = c
            .sample_size(15)
            .measurement_time(Duration::from_secs(1))
            .warm_up_time(Duration::from_millis(300));
    }
    c.configure_from_args()
}

criterion_group! {
    name = benches;
    config = configured();
    targets = bench_codec, bench_v8_value, bench_exec
}
criterion_main!(benches);
