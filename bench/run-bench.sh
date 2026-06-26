#!/usr/bin/env bash
# Plecto LB benchmark orchestrator — k6 (load) + InfluxDB + Grafana (visualization).
#
# Fully LOCAL: Plecto, k6, InfluxDB and Grafana all run on this host / localhost only.
# k6 telemetry and Grafana/Influx phone-home are disabled. The only network use is the
# one-time fetch of the k6 binary and the two docker images.
#
# The method (this script, the k6 scenarios, the Grafana provisioning, docker-compose) is
# public; the raw outputs under results/ and the downloaded k6 binary are git-ignored
# (see bench/.gitignore), not committed.
#
#   bash bench/run-bench.sh          # run the full benchmark
#   bash bench/run-bench.sh --down   # stop & remove the docker stack
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$BENCH_DIR/.." && pwd)"
WORKSPACE="$REPO_ROOT/plecto"
RESULTS="$BENCH_DIR/results"
BIN="$BENCH_DIR/bin"
K6="$BIN/k6"
EXAMPLE_BIN="$WORKSPACE/target/release/examples/load-balancing"
PROXY="http://localhost:8080/"
INFLUX="http://localhost:8086"
COMPOSE=(docker compose -f "$BENCH_DIR/docker-compose.yml")

# Local-only: no usage report to grafana.com, no external timestamps.
export K6_NO_USAGE_REPORT=true

log(){ printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
err(){ printf '\033[1;31m!! %s\033[0m\n' "$*" >&2; }

PLECTO_PID=""
cleanup(){ [[ -n "$PLECTO_PID" ]] && kill "$PLECTO_PID" 2>/dev/null || true; }
trap cleanup EXIT

if [[ "${1:-}" == "--down" ]]; then
  log "tearing down docker stack"
  "${COMPOSE[@]}" down -v
  exit 0
fi

mkdir -p "$RESULTS" "$BIN"

# ---------------------------------------------------------------------------
# 0) k6 (arm64 host binary, installed once into bench/bin)
# ---------------------------------------------------------------------------
if [[ ! -x "$K6" ]]; then
  log "installing k6 (linux/arm64) into bench/bin"
  ver="$(curl -fsSL https://api.github.com/repos/grafana/k6/releases/latest \
         | grep -oE '"tag_name"[^,]*' | grep -oE 'v[0-9.]+' | head -1)"
  [[ -n "$ver" ]] || { err "could not resolve latest k6 version"; exit 1; }
  url="https://github.com/grafana/k6/releases/download/${ver}/k6-${ver}-linux-arm64.tar.gz"
  log "downloading k6 ${ver}"
  curl -fsSL "$url" -o "$BIN/k6.tar.gz"
  tar -xzf "$BIN/k6.tar.gz" -C "$BIN" --strip-components=1 "k6-${ver}-linux-arm64/k6"
  rm -f "$BIN/k6.tar.gz"
fi
log "k6: $("$K6" version)"

# ---------------------------------------------------------------------------
# 1) InfluxDB + Grafana
# ---------------------------------------------------------------------------
log "starting InfluxDB + Grafana"
"${COMPOSE[@]}" up -d
log "waiting for InfluxDB"
for _ in $(seq 60); do curl -fsS "$INFLUX/ping" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS -XPOST "$INFLUX/query" --data-urlencode "q=CREATE DATABASE k6" >/dev/null 2>&1 || true
# Fresh series each run so the dashboard time range isn't polluted by older runs.
curl -fsS -XPOST "$INFLUX/query" --data-urlencode "q=DROP DATABASE k6" >/dev/null 2>&1 || true
curl -fsS -XPOST "$INFLUX/query" --data-urlencode "q=CREATE DATABASE k6" >/dev/null 2>&1 || true
log "waiting for Grafana"
for _ in $(seq 60); do curl -fsS "http://localhost:3000/api/health" >/dev/null 2>&1 && break; sleep 1; done

# ---------------------------------------------------------------------------
# 2) build + launch the load-balancing example (proxy :8080, backends a/b/c)
# ---------------------------------------------------------------------------
log "building load-balancing example (cargo -j 4, WSL2-friendly)"
( cd "$WORKSPACE" && cargo build --release -p plecto-server --example load-balancing -j 4 )

pkill -f 'release/examples/load-balancing' 2>/dev/null || true
sleep 1
log "launching Plecto"
"$EXAMPLE_BIN" >"$RESULTS/plecto.log" 2>&1 &
PLECTO_PID=$!

log "waiting for proxy to become healthy"
ok=""
for _ in $(seq 60); do curl -fsS "$PROXY" >/dev/null 2>&1 && { ok=1; break; }; sleep 0.5; done
[[ -n "$ok" ]] || { err "proxy never became healthy"; cat "$RESULTS/plecto.log"; exit 1; }

# banner lines: "  inst  : a -> http://127.0.0.1:PORT   (...)"  (proxy line uses localhost, so it's excluded)
mapfile -t BACKENDS < <(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$RESULTS/plecto.log" | head -3)
[[ "${#BACKENDS[@]}" -eq 3 ]] || { err "expected 3 backends, parsed: ${BACKENDS[*]:-none}"; exit 1; }
log "backends: ${BACKENDS[*]}"

run_k6(){ # run_k6 <script> <html> [extra env KEY=VAL ...]
  local script="$1" html="$2"; shift 2
  env "$@" K6_WEB_DASHBOARD=true K6_WEB_DASHBOARD_EXPORT="$html" \
    "$K6" run --out "influxdb=$INFLUX/k6" "$script" 2>&1 | tee "${html%.html}.log"
}

# ---------------------------------------------------------------------------
# 3) scenario 1a — closed-loop ramp: achievable throughput + latency
# ---------------------------------------------------------------------------
log "scenario 1a — throughput ramp (closed-loop)"
run_k6 "$BENCH_DIR/k6/lb-load.js" "$RESULTS/lb-load.html" || true

# Derive a fixed rate (~70% of observed max RPS) for the honest-tail-latency run.
MAXRPS="$(grep -E 'http_reqs' "$RESULTS/lb-load.log" | grep -oE '[0-9]+(\.[0-9]+)?/s' | head -1 | tr -d '/s')"
RATE="$(awk -v r="${MAXRPS:-0}" 'BEGIN{ v=int(r*0.7); if(v<500)v=5000; print v }')"
log "observed max ≈ ${MAXRPS:-?} rps -> constant-arrival-rate = ${RATE} rps"

# ---------------------------------------------------------------------------
# 4) scenario 1b — open-loop fixed rate: coordinated-omission-safe tail latency
# ---------------------------------------------------------------------------
log "scenario 1b — fixed rate ${RATE} rps (open-loop)"
run_k6 "$BENCH_DIR/k6/lb-rate.js" "$RESULTS/lb-rate.html" "RATE=$RATE" || true

# ---------------------------------------------------------------------------
# 5) scenario 2 — ejection + fail-closed under steady load
# ---------------------------------------------------------------------------
log "scenario 2 — ejection + fail-closed (drive /toggle on a timeline)"
EJRATE="$(awk -v r="${MAXRPS:-0}" 'BEGIN{ v=int(r*0.4); if(v<500)v=3000; print v }')"
run_k6 "$BENCH_DIR/k6/lb-ejection.js" "$RESULTS/lb-ejection.html" "RATE=$EJRATE" &
K6_PID=$!
sleep 12; log "  t≈12s  toggle b OFF (eject)";   curl -fsS "${BACKENDS[1]}/toggle" || true
sleep 12; log "  t≈24s  toggle b ON  (rejoin)";  curl -fsS "${BACKENDS[1]}/toggle" || true
sleep 12; log "  t≈36s  toggle ALL OFF (fail-closed -> 503)"; for b in "${BACKENDS[@]}"; do curl -fsS "$b/toggle" >/dev/null || true; done
sleep 10; log "  t≈46s  toggle ALL ON (recover)"; for b in "${BACKENDS[@]}"; do curl -fsS "$b/toggle" >/dev/null || true; done
wait "$K6_PID" || true

# ---------------------------------------------------------------------------
log "done"
cat <<EOF

  Grafana dashboard : http://localhost:3000/d/plecto-lb-k6   (anonymous Admin)
  Standalone HTML   : $RESULTS/lb-load.html
                      $RESULTS/lb-rate.html
                      $RESULTS/lb-ejection.html
  k6 text summaries : $RESULTS/lb-*.log
  Plecto log        : $RESULTS/plecto.log

  Stop the stack later with:  bash bench/run-bench.sh --down
EOF
