//! Rust-side microbenchmarks (criterion, wall time).
//!
//! Run: `cargo bench --manifest-path native/v8-runtime/Cargo.toml`
//! CI runs with `-- --output-format bencher` so `scripts/bench-compare.ts`
//! can parse the results (see .github/workflows/bench.yml).
//!
//! Groups:
//! - `v8_value` — the value plane crossing into/out of V8: `blob::serialize_value`
//!   / `blob::deserialize_value`, i.e. V8's own `ValueSerializer` /
//!   `ValueDeserializer` (the only value codec in the protocol).
//! - `exec` — per-run execution costs: fresh context, snapshot restore,
//!   unique-source compile, and direct `Function::call` per event.
//! - `policy` — warm-registry eviction/pressure decisions (policy.rs).
//! - `url` — the sandbox `URL` class end to end: JS call → native callback →
//!   ada parse → component array (url.rs).
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
use iso4_v8_runtime::blob;
use iso4_v8_runtime::v8::init_platform;
use iso4_v8_runtime::webtypes;

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

/// A payload fixture, described independently of any codec and materialised
/// into a V8 object graph by [`to_v8`]. The wire codec used to double as the
/// fixture builder; with a single V8-native codec the benches build the object
/// graph directly.
#[derive(Clone)]
enum Shape {
    Bool(bool),
    Number(f64),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<Shape>),
    Object(Vec<(String, Shape)>),
}

fn to_v8<'s>(scope: &mut v8::PinScope<'s, '_>, shape: &Shape) -> v8::Local<'s, v8::Value> {
    match shape {
        Shape::Bool(b) => v8::Boolean::new(scope, *b).into(),
        Shape::Number(n) => v8::Number::new(scope, *n).into(),
        Shape::String(s) => v8::String::new(scope, s).unwrap().into(),
        Shape::Bytes(bytes) => {
            let len = bytes.len();
            let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes.clone()).make_shared();
            let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
            v8::Uint8Array::new(scope, buffer, 0, len).unwrap().into()
        }
        Shape::Array(items) => {
            let array = v8::Array::new(scope, items.len() as i32);
            for (i, item) in items.iter().enumerate() {
                let v = to_v8(scope, item);
                array.set_index(scope, i as u32, v);
            }
            array.into()
        }
        Shape::Object(fields) => {
            let object = v8::Object::new(scope);
            for (key, value) in fields {
                let v = to_v8(scope, value);
                let k = v8::String::new(scope, key).unwrap();
                object.create_data_property(scope, k.into(), v);
            }
            object.into()
        }
    }
}

fn obj(fields: Vec<(&str, Shape)>) -> Shape {
    Shape::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

/// ~10 keys, long string values, ~750 B — realistic analytics event.
fn sparse1k() -> Shape {
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
        ("eventId", Shape::String(event_id)),
        ("tenantId", Shape::String("tenant_4c1f9a2b".into())),
        ("type", Shape::String("analytics.pageview".into())),
        ("url", Shape::String(url)),
        (
            "userAgent",
            Shape::String(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
                    .into(),
            ),
        ),
        ("referrer", Shape::String(referrer)),
        ("description", Shape::String(description)),
        (
            "timestamp",
            Shape::Number(1_722_945_600_000.0 + (rand() * 86_400_000.0).floor()),
        ),
        ("sessionDurationMs", Shape::Number(rand() * 900_000.0)),
        ("isAuthenticated", Shape::Bool(true)),
    ])
}

/// ~200 values, ~1.3 KB — value-dense but small.
fn dense1k() -> Shape {
    let mut rand = prng(0xD513);
    let metrics = Shape::Object(
        (0..96)
            .map(|i| (format!("m{i}"), Shape::Number(rand() * 1000.0)))
            .collect(),
    );
    let tags = Shape::Array(
        (0..48)
            .map(|_| Shape::String(WORDS[(rand() * WORDS.len() as f64) as usize].into()))
            .collect(),
    );
    let flags = Shape::Array((0..48).map(|_| Shape::Bool(rand() > 0.5)).collect());
    obj(vec![
        ("kind", Shape::String("metrics.batch".into())),
        ("metrics", metrics),
        ("tags", tags),
        ("flags", flags),
    ])
}

/// 12k rows × 4 fields (~48k values, ~0.5–0.7 MB) — value-dense and large.
fn dense2m() -> Shape {
    let mut rand = prng(0xDE2E);
    let rows = Shape::Array(
        (0..12_000)
            .map(|i| {
                let name = format!(
                    "{}_{}",
                    WORDS[(i as usize) % WORDS.len()],
                    (rand() * 1e6) as u64
                );
                obj(vec![
                    ("id", Shape::Number(f64::from(i))),
                    ("name", Shape::String(name)),
                    ("value", Shape::Number(rand() * 10_000.0)),
                    ("active", Shape::Bool(rand() > 0.3)),
                ])
            })
            .collect(),
    );
    obj(vec![
        ("kind", Shape::String("rows.batch".into())),
        ("rows", rows),
    ])
}

/// One 2 MB byte buffer — bytes plane.
fn bytes2m() -> Shape {
    let mut rand = prng(0xB2E5);
    let mut buf = vec![0u8; 2 * 1024 * 1024];
    for chunk in buf.chunks_exact_mut(4) {
        let n = (rand() * 4_294_967_296.0) as u32;
        chunk.copy_from_slice(&n.to_le_bytes());
    }
    Shape::Bytes(buf)
}

fn shapes() -> Vec<(&'static str, Shape)> {
    vec![
        ("sparse1k", sparse1k()),
        ("dense1k", dense1k()),
        ("dense2m", dense2m()),
        ("bytes2m", bytes2m()),
    ]
}

// ── Groups ─────────────────────────────────────────────────────────────────

/// The value plane crossing the V8 boundary, per shape: the blob codec both
/// directions. These are the two legs every value on the wire now pays.
fn bench_v8_value(c: &mut Criterion) {
    init_platform();
    let mut group = c.benchmark_group("v8_value");

    for (name, shape) in &shapes() {
        // V8 value → v8-blob bytes (outbound leg).
        group.bench_function(BenchmarkId::new("value_serializer", name), |b| {
            let isolate = &mut v8::Isolate::new(Default::default());
            v8::scope!(let scope, isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let value = to_v8(scope, shape);
            b.iter(|| {
                // Per-iteration handle scope: without it, millions of locals
                // accumulate in the outer scope over a bench run.
                v8::scope!(let scope, scope);
                black_box(blob::serialize_value(scope, black_box(value)).unwrap())
            });
        });

        // v8-blob bytes → V8 value (inbound leg).
        group.bench_function(BenchmarkId::new("value_deserializer", name), |b| {
            let isolate = &mut v8::Isolate::new(Default::default());
            v8::scope!(let scope, isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let value = to_v8(scope, shape);
            let bytes = blob::serialize_value(scope, value).unwrap();
            b.iter(|| {
                v8::scope!(let scope, scope);
                black_box(blob::deserialize_value(scope, black_box(&bytes)).unwrap());
            });
        });
    }
    group.finish();
}

/// ~150 B event for the RunCall-pattern benches. No JSON is involved anywhere
/// — the value plane is the v8 blob.
fn small_event() -> Shape {
    obj(vec![
        ("id", Shape::Number(8412.0)),
        ("type", Shape::String("analytics.pageview".into())),
        ("tenant", Shape::String("tenant_4c1f9a2b".into())),
        ("name", Shape::String("checkout session".into())),
        ("value", Shape::Number(42.5)),
        ("ts", Shape::Number(1_722_945_600_000.0)),
        ("authenticated", Shape::Bool(true)),
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
        v8::scope!(let scope, isolate);
        b.iter(|| {
            v8::scope!(let scope, scope);
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
            let params = v8::Isolate::create_params().snapshot_blob(blob.clone().into());
            let isolate = &mut v8::Isolate::new(params);
            v8::scope!(let scope, isolate);
            let context = v8::Context::new(scope, Default::default());
            black_box(context);
        });
    });

    // Compile + run a script with inlined data. The source MUST be unique
    // per iteration: identical source hits V8's compile cache and reports
    // ~5x too fast (measured 2.9 µs cached vs 15.6 µs unique).
    group.bench_function("compile_run_unique_source", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let mut n: u64 = 0;
        b.iter(|| {
            n += 1;
            v8::scope!(let scope, scope);
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
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let (func, recv) = compile_fn(scope, TRANSFORM_SRC);
        let event = to_v8(scope, &small_event());
        b.iter(|| {
            v8::scope!(let scope, scope);
            black_box(func.call(scope, recv, &[event]).unwrap());
        });
    });

    // The full RunCall per-event pattern with the v8-blob value plane
    // (item 1 + item 2): argsBlob in → ValueDeserializer → call() →
    // ValueSerializer → result blob out.
    group.bench_function("call_per_event", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let (func, recv) = compile_fn(scope, TRANSFORM_SRC);
        let event = to_v8(scope, &small_event());
        let args_blob = blob::serialize_value(scope, event).unwrap();
        b.iter(|| {
            v8::scope!(let scope, scope);
            let event = blob::deserialize_value(scope, &args_blob).unwrap();
            let result = func.call(scope, recv, &[event]).unwrap();
            black_box(blob::serialize_value(scope, result).unwrap());
        });
    });

    group.finish();
}

// ── url ─────────────────────────────────────────────────────────────────────

/// The sandbox `URL` class end to end, as guest code pays for it: JS call →
/// native callback → ada parse → component-array build (url.rs).
fn bench_url(c: &mut Criterion) {
    init_platform();
    let mut group = c.benchmark_group("url");

    group.bench_function("parse", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        webtypes::install(scope).unwrap();
        let (func, recv) = compile_fn(scope, "s => new URL(s)");
        let input: v8::Local<v8::Value> =
            v8::String::new(scope, "https://user@example.com:8443/a/b/../c?x=1&y=2#frag")
                .unwrap()
                .into();
        b.iter(|| {
            v8::scope!(let scope, scope);
            black_box(func.call(scope, recv, &[input]).unwrap());
        });
    });

    // Relative resolution against a base — the shape `Request` construction
    // and redirect handling produce.
    group.bench_function("parse_relative_with_base", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        webtypes::install(scope).unwrap();
        let (func, recv) = compile_fn(
            scope,
            "s => new URL(s, 'https://example.com/base/dir/index.html')",
        );
        let input: v8::Local<v8::Value> =
            v8::String::new(scope, "../other/path?q=1").unwrap().into();
        b.iter(|| {
            v8::scope!(let scope, scope);
            black_box(func.call(scope, recv, &[input]).unwrap());
        });
    });

    // One component setter: re-parse from href, apply the DOM setter
    // natively, swap in the fresh component array.
    group.bench_function("set_pathname", |b| {
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        webtypes::install(scope).unwrap();
        let (make, recv) = compile_fn(scope, "() => new URL('https://example.com/a?b=1')");
        let url = make.call(scope, recv, &[]).unwrap();
        let (func, recv) = compile_fn(scope, "u => { u.pathname = '/new/path'; return u }");
        b.iter(|| {
            v8::scope!(let scope, scope);
            black_box(func.call(scope, recv, &[url]).unwrap());
        });
    });

    group.finish();
}

/// Evaluate a function-expression source and hand back the function plus an
/// `undefined` receiver for `Function::call`.
fn compile_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    src: &str,
) -> (v8::Local<'s, v8::Function>, v8::Local<'s, v8::Value>) {
    let code = v8::String::new(scope, src).unwrap();
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
        v8::scope!(let scope, &mut isolate);
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

// ── policy: eviction scoring + watermark verdict (#66) ─────────────────────
//
// These run inside the registry lock on acquire/release, so their cost is
// hot-path cost. `watermark_action` is the per-event constant price (target
// ~ns — it is a handful of integer compares); `pick_victim` is paid only
// when a pass actually evicts, scaling with the idle population. CodSpeed
// tracks both so a policy change that regresses the per-call tax or the
// shed-pass walk shows up as a PR diff.

fn bench_policy(c: &mut Criterion) {
    use iso4_v8_runtime::policy::{
        pick_victim, watermark_action, PassOutcome, PressureFacts, VictimFact,
    };
    use std::time::Instant;

    let mut group = c.benchmark_group("policy");
    let now = Instant::now();

    for count in [16usize, 256, 1024] {
        // Deterministic spread of heap sizes and ages — mulberry32, same
        // generator as the payload fixtures.
        let mut rand = prng(0xEC1C_7100 + count as u32);
        let idle: Vec<VictimFact> = (0..count)
            .map(|_| VictimFact {
                heap_used_bytes: (rand() * 128.0 * 1024.0 * 1024.0) as u64,
                last_used: now - Duration::from_micros((rand() * 60_000_000.0) as u64),
            })
            .collect();
        group.bench_with_input(
            BenchmarkId::new("pick_victim", count),
            &idle,
            |b, idle| b.iter(|| black_box(pick_victim(black_box(idle), now))),
        );
    }

    let facts = PressureFacts {
        usage_bytes: 900 * 1024 * 1024,
        budget_bytes: 1024 * 1024 * 1024,
        was_shedding: true,
        last_pass: Some(PassOutcome {
            usage_at_pass: 950 * 1024 * 1024,
        }),
        idle_count: 256,
    };
    group.bench_function("watermark_action", |b| {
        b.iter(|| black_box(watermark_action(black_box(&facts))));
    });

    group.finish();
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
    targets = bench_v8_value, bench_exec, bench_policy, bench_url
}
criterion_main!(benches);
