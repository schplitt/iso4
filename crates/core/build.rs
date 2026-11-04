//! Builds the V8 startup snapshot (`src/snapshot.rs`) into `OUT_DIR`, where
//! `src/lib.rs` embeds it, and compiles the frozen-clock platform
//! (`src/clock_platform.cc`). The engine modules the snapshot reaches are
//! compiled into this script so its native callbacks are the crate's own.
#![allow(warnings)]

#[path = "src/blob.rs"]
mod blob;
#[path = "src/cpuclock.rs"]
mod cpuclock;
#[path = "src/errors.rs"]
mod errors;
#[path = "src/link.rs"]
mod link;
#[path = "src/snapshot.rs"]
mod snapshot;
#[path = "src/types.rs"]
mod types;
#[path = "src/url.rs"]
mod url;
#[path = "src/v8.rs"]
mod v8;
#[path = "src/webcodec.rs"]
mod webcodec;
#[path = "src/webtypes.rs"]
mod webtypes;

// Read only when an isolate boots, which never happens in this script.
static SNAPSHOT_BLOB: &[u8] = &[];

// The stock platform: this script runs no guest code, and a build script
// cannot link the C++ it compiles.
fn new_platform() -> ::v8::UniqueRef<::v8::Platform> {
    ::v8::new_default_platform(0, false)
}

fn main() {
    println!("cargo:rerun-if-changed=src");
    let out = std::path::Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("snapshot.bin");
    std::fs::write(out, snapshot::create()).unwrap();
    compile_clock_platform();
}

// Built against the vendored V8 headers and Chromium's libc++ so the object
// matches the prebuilt librusty_v8.a (release profile). Those headers need a
// recent clang++ (not g++), taken from PATH unless CXX is set.
fn compile_clock_platform() {
    let vendor = std::path::Path::new("../vendor/v8");
    let mut build = cc::Build::new();
    if std::env::var_os("CXX").is_none() {
        build.compiler("clang++");
    }
    build
        .cpp(true)
        .std("c++20")
        .file("src/clock_platform.cc")
        .flag("-nostdinc++")
        .include(vendor.join("buildtools/third_party/libc++"))
        .include(vendor.join("third_party/libc++/src/include"))
        .include(vendor.join("third_party/libc++abi/src/include"))
        .include(vendor)
        .include(vendor.join("v8"))
        .include(vendor.join("v8/include"))
        .include(vendor.join("third_party/abseil-cpp"))
        .define("NDEBUG", None)
        .define("_LIBCPP_HARDENING_MODE", "_LIBCPP_HARDENING_MODE_EXTENSIVE")
        .flag("-fno-exceptions")
        .flag("-fno-rtti")
        .flag("-fvisibility=hidden")
        .warnings(false)
        .cpp_link_stdlib(None)
        .compile("iso4_clock_platform");
}
