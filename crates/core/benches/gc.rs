//! GC benchmark (criterion), its own binary so micro.rs's code layout stays put.
//!
//! Run: `cargo bench --manifest-path crates/Cargo.toml -p iso4-core --bench gc`
//!
//! `gc` — one full collect (`low_memory_notification`) of a fresh isolate
//! holding 4 MB / 16 MB of old-space garbage, on the calling thread only.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use iso4_core::testing::{create_params, init_platform};

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
    // No GC work on worker threads, so CodSpeed's instruction count repeats.
    v8::V8::set_flags_from_string("--single-threaded-gc");
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

fn configured() -> Criterion {
    let mut c = Criterion::default()
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1));
    if std::env::var("ISO4_BENCH_PROFILE").as_deref() == Ok("pr") {
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
    targets = bench_gc
}
criterion_main!(benches);
