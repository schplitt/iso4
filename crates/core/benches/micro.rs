//! Rust-side microbenchmarks (criterion, wall time).
//!
//! Run: `cargo bench --manifest-path crates/Cargo.toml -p iso4-core`
//! CI measures the same groups under `cargo codspeed` (.github/workflows/codspeed.yml).
//!
//! Groups:
//! - `v8_value` — the value plane crossing into/out of V8: `blob::serialize_value`
//!   / `blob::deserialize_value`, i.e. V8's own `ValueSerializer` /
//!   `ValueDeserializer` (the only value codec in the protocol).
//! - `exec` — per-run execution costs: fresh context, isolate boot,
//!   unique-source compile, and direct `Function::call` per event.
//! - `policy` — warm-registry eviction/pressure decisions (policy.rs).
//! - `url` — the sandbox `URL` class end to end: JS call → native callback →
//!   ada parse → component array (url.rs).
//! - `engine` — whole runs through `Engine` and an in-memory link: cold
//!   one-off run, warm `prefix_run`, one bridge round trip.
//! - `gc` — one full collect (`low_memory_notification`) of a fresh isolate
//!   holding 4 MB / 16 MB of old-space garbage.
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

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use iso4_core::testing::{
    boot_context, create_params, deserialize_value, init_platform, serialize_value,
};

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
            let isolate = &mut v8::Isolate::new(create_params());
            v8::scope!(let scope, isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let value = to_v8(scope, shape);
            b.iter(|| {
                // Per-iteration handle scope: without it, millions of locals
                // accumulate in the outer scope over a bench run.
                v8::scope!(let scope, scope);
                black_box(serialize_value(scope, black_box(value)).unwrap())
            });
        });

        // v8-blob bytes → V8 value (inbound leg).
        group.bench_function(BenchmarkId::new("value_deserializer", name), |b| {
            let isolate = &mut v8::Isolate::new(create_params());
            v8::scope!(let scope, isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let value = to_v8(scope, shape);
            let bytes = serialize_value(scope, value).unwrap();
            b.iter(|| {
                v8::scope!(let scope, scope);
                black_box(deserialize_value(scope, black_box(&bytes)).unwrap());
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
/// (context create dominates; isolate boot, compile, call are the rest).
fn bench_exec(c: &mut Criterion) {
    init_platform();
    let mut group = c.benchmark_group("exec");

    // Fresh context in an existing isolate — the dominant slice of the
    // per-run tax.
    group.bench_function("context_create", |b| {
        let isolate = &mut v8::Isolate::new(create_params());
        v8::scope!(let scope, isolate);
        b.iter(|| {
            v8::scope!(let scope, scope);
            black_box(v8::Context::new(scope, Default::default()));
        });
    });

    // Fresh isolate + context booted from the embedded startup snapshot —
    // the fixed boot every one-off run and cold start pays.
    group.bench_function("isolate_boot", |b| {
        b.iter(|| {
            let isolate = &mut v8::Isolate::new(create_params());
            v8::scope!(let scope, isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            boot_context(scope, std::ptr::null()).unwrap();
        });
    });

    // Compile + run a script with inlined data. The source MUST be unique
    // per iteration: identical source hits V8's compile cache and reports
    // ~5x too fast (measured 2.9 µs cached vs 15.6 µs unique).
    group.bench_function("compile_run_unique_source", |b| {
        let isolate = &mut v8::Isolate::new(create_params());
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
        let isolate = &mut v8::Isolate::new(create_params());
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
        let isolate = &mut v8::Isolate::new(create_params());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let (func, recv) = compile_fn(scope, TRANSFORM_SRC);
        let event = to_v8(scope, &small_event());
        let args_blob = serialize_value(scope, event).unwrap();
        b.iter(|| {
            v8::scope!(let scope, scope);
            let event = deserialize_value(scope, &args_blob).unwrap();
            let result = func.call(scope, recv, &[event]).unwrap();
            black_box(serialize_value(scope, result).unwrap());
        });
    });

    group.finish();
}

// ── gc ──────────────────────────────────────────────────────────────────────

// Measured: one `{ a, b }` object plus its array slot is 49-51 B;
// rounding down to 48 keeps the held bytes at or above the target.
const GARBAGE_OBJ_BYTES: usize = 48;

/// A fresh isolate whose old space holds about `mb` MB of unreachable small
/// objects; the context stays alive so only the garbage is collectable.
struct Garbage {
    _context: v8::Global<v8::Context>,
    isolate: v8::OwnedIsolate,
    used_before: usize,
    target: usize,
}

impl Garbage {
    fn new(mb: usize) -> Self {
        let target = mb * 1024 * 1024;
        let mut isolate = v8::Isolate::new(create_params());
        let context = {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            scope.low_memory_notification();
            let baseline = scope.get_heap_statistics().used_heap_size();
            // Held only by a Rust handle, so no JS state can keep it alive.
            let held = {
                v8::scope!(let scope, scope);
                let src = format!(
                    "(() => {{ const g = []; for (let i = 0; i < {}; i++) g.push({{ a: i, b: i }}); return g }})()",
                    target / GARBAGE_OBJ_BYTES
                );
                let value = run_js(scope, &src);
                v8::Global::new(scope, value)
            };
            // Survives one full collect while reachable, so it sits in old space.
            scope.low_memory_notification();
            let held_bytes = scope
                .get_heap_statistics()
                .used_heap_size()
                .saturating_sub(baseline);
            assert!(
                held_bytes >= target * 9 / 10,
                "gc bench: {held_bytes} B held, want ~{target}"
            );
            drop(held);
            v8::Global::new(scope, context)
        };
        let used_before = isolate.get_heap_statistics().used_heap_size();
        Self {
            _context: context,
            isolate,
            used_before,
            target,
        }
    }
}

impl Drop for Garbage {
    // Runs after the measured collect: a sample that freed nothing fails.
    fn drop(&mut self) {
        let freed = self
            .used_before
            .saturating_sub(self.isolate.get_heap_statistics().used_heap_size());
        assert!(
            freed >= self.target / 2,
            "gc bench: collect freed {freed} B, want ~{}",
            self.target
        );
    }
}

fn run_js<'s>(scope: &mut v8::PinScope<'s, '_>, src: &str) -> v8::Local<'s, v8::Value> {
    let code = v8::String::new(scope, src).unwrap();
    v8::Script::compile(scope, code, None)
        .unwrap()
        .run(scope)
        .unwrap()
}

/// One full collect (`low_memory_notification`, the hard-line and idle-settle
/// GC) of a known amount of old-space garbage.
fn bench_gc(c: &mut Criterion) {
    init_platform();
    let mut group = c.benchmark_group("gc");
    for mb in [4usize, 16] {
        group.bench_function(BenchmarkId::new("full_collect", format!("{mb}mb")), |b| {
            b.iter_batched_ref(
                || Garbage::new(mb),
                |garbage| garbage.isolate.low_memory_notification(),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

// ── url ─────────────────────────────────────────────────────────────────────

/// The sandbox `URL` class end to end, as guest code pays for it: JS call →
/// native callback → ada parse → component-array build (url.rs).
fn bench_url(c: &mut Criterion) {
    init_platform();
    let mut group = c.benchmark_group("url");

    group.bench_function("parse", |b| {
        let isolate = &mut v8::Isolate::new(create_params());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        boot_context(scope, std::ptr::null()).unwrap();
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
        let isolate = &mut v8::Isolate::new(create_params());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        boot_context(scope, std::ptr::null()).unwrap();
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
        let isolate = &mut v8::Isolate::new(create_params());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        boot_context(scope, std::ptr::null()).unwrap();
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
    let func: v8::Local<v8::Function> = run_js(scope, src).try_into().unwrap();
    let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
    (func, recv)
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
    use iso4_core::testing::{
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
        group.bench_with_input(BenchmarkId::new("pick_victim", count), &idle, |b, idle| {
            b.iter(|| black_box(pick_victim(black_box(idle), now)))
        });
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

// ── engine: whole runs through the public Engine API ───────────────────────

fn bench_engine(c: &mut Criterion) {
    use iso4_core::testing::{channel_link, engine, testval};
    use iso4_core::{Addr, Caller, Engine, Inbound, NoopObserver, Outbound, RunHandle};
    use iso4_types::{CallSpec, HostGlobalDef, PrefixRunRequest, ResourceLimits, RunRequest};
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    init_platform();
    let engine: Arc<Engine> = Arc::new(engine(0, 0));
    let (link, host) = channel_link();
    let caller = Caller {
        link,
        observer: Arc::new(NoopObserver),
        call_ids: Arc::new(AtomicU32::new(0)),
        brand_key: String::new(),
    };
    let next = |host: &crossbeam_channel::Receiver<(Addr, Outbound)>| {
        host.recv_timeout(Duration::from_secs(10))
            .expect("engine bench: no message within 10 s")
            .1
    };
    let expect_success = |msg: Outbound| match msg {
        Outbound::Result {
            completion: iso4_types::RunCompletion::Success(_),
        } => {}
        other => panic!("engine bench: expected a successful Result, got {other:?}"),
    };
    let no_args = testval::to_blob(&testval::TestValue::Array(Vec::new()));
    let mut run_id: u32 = 0;
    let prefix_run = |prefix_id: &str, run_id: u32, globals: Vec<HostGlobalDef>| {
        let req = PrefixRunRequest {
            run_id,
            prefix_id: prefix_id.to_string(),
            limits: ResourceLimits::default(),
            globals,
            import_rebinds: Vec::new(),
            call: CallSpec {
                export_path: "f".to_string(),
                args_blob: no_args.clone(),
            },
        };
        let mut handle = None;
        engine
            .prefix_run(req, &caller, &mut |h: RunHandle| handle = Some(h))
            .expect("prefix_run refused");
        handle.expect("registered handle")
    };

    let mut group = c.benchmark_group("engine");

    // Fresh isolate per run.
    let oneoff_code = "export const x = 1".to_string();
    group.bench_function("oneoff_cold", |b| {
        b.iter(|| {
            run_id += 1;
            let req = RunRequest {
                run_id,
                code: oneoff_code.clone(),
                filename: None,
                limits: ResourceLimits::default(),
                globals: Vec::new(),
                imports: Vec::new(),
                call: None,
            };
            let mut handle = None;
            engine
                .run(req, &caller, &mut |h: RunHandle| handle = Some(h))
                .expect("run refused");
            expect_success(next(&host));
            drop(handle);
        });
    });

    // One call into an idle warm instance.
    let warm_prefix = engine.prepare(
        "export function f(){ return 1 }".to_string(),
        None,
        Vec::new(),
        Vec::new(),
    );
    run_id += 1;
    let handle = prefix_run(&warm_prefix, run_id, Vec::new());
    expect_success(next(&host));
    drop(handle);
    group.bench_function("prefix_run_warm", |b| {
        b.iter(|| {
            run_id += 1;
            let handle = prefix_run(&warm_prefix, run_id, Vec::new());
            expect_success(next(&host));
            drop(handle);
        });
    });

    // One iteration = one warm prefix_run plus one host bridge call.
    let bridge_prefix = engine.prepare(
        "export async function f(){ return await tool() }".to_string(),
        None,
        vec![HostGlobalDef::bridge("tool")],
        Vec::new(),
    );
    let answer = testval::to_blob(&testval::TestValue::Number(1.0));
    let round_trip = |run_id: u32| {
        // The run binds `tool` to this caller's host session.
        let handle = prefix_run(&bridge_prefix, run_id, vec![HostGlobalDef::bridge("tool")]);
        let call_id = match next(&host) {
            Outbound::BridgeCall { call_id, .. } => call_id,
            other => panic!("engine bench: expected a BridgeCall, got {other:?}"),
        };
        handle.deliver(Inbound::BridgeResponse {
            call_id,
            result: Ok(Some(answer.clone())),
        });
        expect_success(next(&host));
    };
    run_id += 1;
    round_trip(run_id);
    group.bench_function("bridge_roundtrip", |b| {
        b.iter(|| {
            run_id += 1;
            round_trip(run_id);
        });
    });

    group.finish();
    engine.dispose_prefix(&warm_prefix);
    engine.dispose_prefix(&bridge_prefix);
}

// ── Criterion config ───────────────────────────────────────────────────────

fn configured() -> Criterion {
    let mut c = Criterion::default()
        // Bounded measurement windows keep the full suite in single-digit
        // minutes on CI; criterion warns when a heavy bench (dense2m,
        // isolate_boot) can't fit the sample count and proceeds anyway.
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
    targets = bench_v8_value, bench_exec, bench_policy, bench_url, bench_engine, bench_gc
}
criterion_main!(benches);
