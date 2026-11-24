#!/usr/bin/env bash
# pnpm bench:counted: builds a Linux release iso4-v8 in Docker, then runs
# counted.ts in a container pinned to 1 GB memory and 2 CPUs.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../../.." && pwd)"
IMAGE=iso4-counted

docker build -t "$IMAGE" - < "$HERE/Dockerfile"
pnpm --dir "$ROOT/packages/iso4-sandbox" build

docker run --rm -v "$ROOT":/src -w /src \
  -v iso4-counted-cargo-target:/cargo-target \
  -v iso4-counted-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/cargo-target \
  "$IMAGE" sh -euc 'node scripts/prepare-v8.ts && cargo build --release --manifest-path crates/Cargo.toml --package iso4-server'

docker run --rm --memory 1g --memory-swap 1g --cpus 2 \
  -v "$ROOT":/src -w /src \
  -v iso4-counted-cargo-target:/cargo-target:ro \
  -e COUNTED_SHA="$(git -C "$ROOT" rev-parse HEAD)" \
  "$IMAGE" node packages/iso4-sandbox/bench/counted/counted.ts /cargo-target/release/iso4-v8
