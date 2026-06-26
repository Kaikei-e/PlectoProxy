#!/usr/bin/env bash
# Plecto WASM-filter benchmark — cost of the extension plane. Fully local; no docker, no telemetry.
# k6 writes compact JSON summaries; performance/plot.py turns them into charts.
# The method (this script + the k6 scenarios) is public; the raw outputs under results-wasm/
# are git-ignored (see bench/.gitignore), not committed.
#
#   bash bench/run-wasm-bench.sh
set -uo pipefail

# Paths derive from the script location (no hardcoded home dirs).
BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WS="$(cd "$BENCH/../plecto" && pwd)"
K6="$BENCH/bin/k6"
EX="$WS/target/release/examples/wasm-bench"
OUTDIR="$BENCH/results-wasm"
BASE="http://localhost:8085"
export K6_NO_USAGE_REPORT=true
mkdir -p "$OUTDIR"

log(){ printf '\n\033[1;35m== %s ==\033[0m\n' "$*"; }

start_proxy(){ # start_proxy <latency_ms> ; sets PROXY_PID, waits until healthy
  BACKEND_LATENCY_MS="$1" "$EX" >"$OUTDIR/proxy_${1}ms.log" 2>&1 &
  PROXY_PID=$!
  for _ in $(seq 50); do curl -fsS "$BASE/baseline/x" >/dev/null 2>&1 && return 0; sleep 0.3; done
  echo "proxy did not come up"; cat "$OUTDIR/proxy_${1}ms.log"; return 1
}
stop_proxy(){ [[ -n "${PROXY_PID:-}" ]] && kill "$PROXY_PID" 2>/dev/null; wait "${PROXY_PID:-}" 2>/dev/null; PROXY_PID=""; }
trap stop_proxy EXIT

log "building wasm-bench example"
( cd "$WS" && cargo build --release -p plecto-server --example wasm-bench -j 4 ) || exit 1

# ---- Phase A: per-request overhead & pooling value (0 ms backend isolates the filter cost) ----
log "Phase A — overhead & pooling (backend 0 ms, 50 VUs/route)"
start_proxy 0 || exit 1
for r in baseline trusted ondemand; do
  log "route /$r"
  ROUTE_PATH="/$r" VUS=50 DUR=30s OUT="$OUTDIR/$r.json" \
    "$K6" run "$BENCH/k6-wasm/route.js" 2>&1 | tail -3
done
stop_proxy

# ---- Phase B: realistic mixed auth traffic over a latency-injected backend ----
log "Phase B — realistic mixed traffic (backend 15 ms, 2000 rps, 90% valid / 10% bad)"
start_proxy 15 || exit 1
RATE=2000 DUR=40s OUT="$OUTDIR/mixed.json" \
  "$K6" run "$BENCH/k6-wasm/mixed.js" 2>&1 | tail -3
stop_proxy

log "done — summaries in $OUTDIR"
ls -1 "$OUTDIR"/*.json
