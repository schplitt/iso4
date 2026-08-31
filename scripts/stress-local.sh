#!/usr/bin/env bash
# Loop-under-load stress run — the full loop-under-load acceptance bar:
# saturate every core, then run the execution-model test suites N times.
# All N rounds must pass. This deliberately does NOT run in CI: saturating
# shared runners flakes; run it on a real, otherwise-idle machine before
# merging execution-model changes. The bounded per-PR guard is
# packages/iso4-sandbox/tests/stress.test.ts.
#
# Usage:
#   ./scripts/stress-local.sh            # 10 rounds (the acceptance bar)
#   ROUNDS=3 ./scripts/stress-local.sh   # quicker sanity loop
#
# Requires a built native binary (pnpm build:dev or pnpm build:native).

set -u

ROUNDS="${ROUNDS:-10}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUITES=(
  tests/integration.test.ts
  tests/warm.test.ts
  tests/capacity.test.ts
  tests/stress.test.ts
)

if [[ "$(uname)" == "Darwin" ]]; then
  NCPU="$(sysctl -n hw.ncpu)"
else
  NCPU="$(nproc)"
fi

echo "[stress] saturating ${NCPU} cores with 'yes' burners"
BURNER_PIDS=()
for _ in $(seq 1 "$NCPU"); do
  yes > /dev/null &
  BURNER_PIDS+=("$!")
done

cleanup() {
  echo "[stress] killing ${#BURNER_PIDS[@]} burners"
  kill "${BURNER_PIDS[@]}" 2> /dev/null
  wait "${BURNER_PIDS[@]}" 2> /dev/null
}
trap cleanup EXIT INT TERM

PASS=0
for i in $(seq 1 "$ROUNDS"); do
  echo "[stress] round ${i}/${ROUNDS}"
  if (cd "$REPO_ROOT/packages/iso4-sandbox" \
    && pnpm exec vitest run "${SUITES[@]}"); then
    PASS=$((PASS + 1))
  else
    echo "[stress] round ${i} FAILED"
  fi
done

echo "[stress] ${PASS}/${ROUNDS} rounds green"
if [[ "$PASS" -ne "$ROUNDS" ]]; then
  exit 1
fi
