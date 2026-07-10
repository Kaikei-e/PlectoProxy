#!/usr/bin/env bash
# Plecto perf runbook — emits performance/data/*.csv consumed by performance/plot.py. Fully local,
# loopback. Plecto (+ its in-process backends) is pinned to one set of CPU cores and every load
# generator to a disjoint set via taskset, so the generator never steals the proxy's cores. No host
# tuning is applied (governor/turbo left as-is), so absolute throughput is bounded by this host and
# the generator; read ratios, shapes and time-constants as the signal.
#
# Every measured window excludes a short warm-up: the k6 scenarios and plecto-loadgen burn it
# in-script (send, don't record), oha runs get a discarded 5 s pre-run (warm_oha). Fixed-rate tail
# measurements use oha's --latency-correction (coordinated-omission-safe); ceiling runs report
# throughput, and their latencies are read as queueing-at-saturation, not service latency.
#
# `ceiling` is the CANONICAL plain-HTTP/1.1 measurement (keep-alive RPS + cold-connection CPS, on
# the single `bench-server` harness's `/baseline` route): `wasm` and `tls` read its numbers instead
# of re-measuring the same thing on their own server instance (the former `wasm-bench` / `edge-bench`
# / `churn` split each ran their own copy of this — see performance/README.md's consolidation note).
#
#   bash bench/perf/run-perf.sh <phase>
#   phase ∈ quick ceiling sweep openloop rr ejection swap wasm tls h3 ws footprint ratelimit body mix
#           industry all
#
# Default ports avoid colliding with other local services (override with *_ADDR env).
# Offline: load traffic stays on loopback. Set REQUIRE_OFFLINE=1 to refuse a default IPv4 route
# (industry-style lab isolation; see bench/methodology.md). INFLUX=1 is opt-in local dashboard only.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$BENCH/.." && pwd)"
WS="$ROOT/plecto"
DATA="$ROOT/performance/data"
mkdir -p "$DATA"

OHA="${OHA:-$HOME/.cargo/bin/oha}"
K6="${K6:-$(command -v k6)}"
# Pin Plecto and the generators to disjoint core sets. Default: split the logical CPUs in half —
# proxy on the lower indices, generators on the upper. Override PROXY_CPUS / GEN_CPUS to match your
# host's topology (e.g. put the proxy on its fastest cores and the generators on the rest).
_NCPU="$(nproc)"; _HALF="$(( _NCPU / 2 ))"
PROXY_CPUS="${PROXY_CPUS:-0-$(( _HALF - 1 ))}"
GEN_CPUS="${GEN_CPUS:-$_HALF-$(( _NCPU - 1 ))}"
export K6_NO_USAGE_REPORT=true
# oha ≥1.14 maps NO_COLOR through clap and only accepts true/false — the conventional NO_COLOR=1
# (common in CI/agent environments) makes every oha invocation fail with an empty JSON file.
if [[ "${NO_COLOR:-}" == "1" ]]; then
  export NO_COLOR=true
fi

assert_offline(){
  # Soft by default: generators already target 127.0.0.1 only. REQUIRE_OFFLINE=1 hard-fails if a
  # default IPv4 route exists (typical laptop), so operators can force a netns lab:
  #   sudo unshare -n -- bash -c 'ip link set lo up; REQUIRE_OFFLINE=1 bash bench/perf/run-perf.sh industry'
  [[ "${REQUIRE_OFFLINE:-}" == "1" ]] || return 0
  if command -v ip >/dev/null 2>&1 && ip -4 route show default 2>/dev/null | grep -q .; then
    echo "REQUIRE_OFFLINE=1: default IPv4 route present — refusing to run (external path exists)."
    echo "  Use an empty netns (unshare -n + lo up) or unset REQUIRE_OFFLINE."
    return 1
  fi
}
assert_offline || exit 1

LB_ADDR="${LB_ADDR:-127.0.0.1:28080}"
BENCH_ADDR="${BENCH_ADDR:-127.0.0.1:28085}"
TLS_ADDR="${TLS_ADDR:-127.0.0.1:28443}"
SWAP_ADDR="${SWAP_ADDR:-127.0.0.1:28087}"

log(){ printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
gen(){ taskset -c "$GEN_CPUS" "$@"; }   # run a generator on the generator core set

# plecto-loadgen (bench/loadgen): the Rust rr / ejection / swap / ws generators. Built lazily on
# first use (a warm rebuild is a no-op); replaced the Python drivers, whose GIL-bound workers melted
# before the proxy did.
LOADGEN="$BENCH/loadgen/target/release/plecto-loadgen"
loadgen(){
  cargo build --release --quiet --manifest-path "$BENCH/loadgen/Cargo.toml" || return 1
  gen "$LOADGEN" "$@"
}

# Optional live dashboard (opt-in): INFLUX=1 brings up the local InfluxDB+Grafana stack
# (bench/docker-compose.yml) and streams every k6 phase to it, so a run is watchable in real time at
# http://localhost:3000. Pulling the two images is a one-time SETUP fetch; the load itself stays
# fully on loopback (generators + proxy + in-process upstreams), telemetry off. Unset (default) =
# no docker, CSV + matplotlib charts only.
INFLUX_OUT=()
influx_down(){ :; }
if [[ "${INFLUX:-}" == "1" ]]; then
  COMPOSE=(docker compose -f "$BENCH/docker-compose.yml")
  influx_down(){ "${COMPOSE[@]}" down -v >/dev/null 2>&1 || true; }
  log "INFLUX=1 — bringing up the local InfluxDB + Grafana dashboard (one-time image pull)"
  "${COMPOSE[@]}" up -d
  for _ in $(seq 60); do curl -fsS "http://localhost:8086/ping" >/dev/null 2>&1 && break; sleep 1; done
  curl -fsS -XPOST "http://localhost:8086/query" --data-urlencode "q=DROP DATABASE k6"   >/dev/null 2>&1 || true
  curl -fsS -XPOST "http://localhost:8086/query" --data-urlencode "q=CREATE DATABASE k6" >/dev/null 2>&1 || true
  INFLUX_OUT=(--out "influxdb=http://localhost:8086/k6")
  echo "  Grafana: http://localhost:3000/d/plecto-lb-k6  (anonymous Admin)"
fi

PROXY_PID=""
BLOG=""
stop_proxy(){ [[ -n "$PROXY_PID" ]] && kill "$PROXY_PID" 2>/dev/null; wait "$PROXY_PID" 2>/dev/null; PROXY_PID=""; }
trap 'stop_proxy; influx_down' EXIT

# launch <example> <addr> <health_url|""> [env KEY=VAL ...]
# health_url empty => skip the http health loop (caller probes itself, e.g. TLS over https).
launch(){
  local ex="$1" addr="$2" health="$3"; shift 3
  BLOG="$(mktemp)"
  env PLECTO_PROXY_ADDR="$addr" RUST_LOG=warn "$@" "$WS/target/release/examples/$ex" >"$BLOG" 2>&1 &
  PROXY_PID=$!
  taskset -cp "$PROXY_CPUS" "$PROXY_PID" >/dev/null 2>&1
  if [[ -n "$health" ]]; then
    local ok=""
    for _ in $(seq 80); do curl -fsS "$health" >/dev/null 2>&1 && { ok=1; break; }; sleep 0.25; done
    [[ -n "$ok" ]] || { echo "proxy $ex did not become healthy"; cat "$BLOG"; return 1; }
  else
    sleep 2
  fi
  echo "launched $ex on $addr (pid $PROXY_PID, pinned $PROXY_CPUS)"
}

# oha JSON (latency in seconds) -> "rps p50 p90 p95 p99 p99_9" in ms
oha_row(){ python3 "$HERE/oha_parse.py" "$1"; }

# 5 s discarded warm-up before a measured oha run (same flags + URL): the k6 scenarios exclude
# their warm-up in-script, oha can't, so the cold-start seconds are burned here instead.
warm_oha(){ gen "$OHA" -z 5s --no-tui "$@" >/dev/null 2>&1; }

# The "keep-alive" row of an already-measured ceiling.csv, as "rps p50 p90 p95 p99" — read by
# `wasm` / `tls` so they never re-measure the plain-h1 ceiling `ceiling` already produced.
ceiling_row(){
  python3 -c '
import csv, sys
rows = {r["variant"]: r for r in csv.DictReader(open(sys.argv[1]))}
r = rows["keep-alive"]
print(r["rps"], r["p50"], r["p90"], r["p95"], r["p99"])
' "$DATA/ceiling.csv"
}

# ---------------------------------------------------------------- Quick smoke (no k6/Docker)
phase_quick(){
  log "Quick — smoke ceiling + idle RSS (~1 min; oha only, no k6/Docker needed)"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" BACKEND_LATENCY_MS=0 || return 1
  local tmp; tmp="$(mktemp -d)"
  warm_oha -c 50 "http://$BENCH_ADDR/baseline/x"
  gen "$OHA" -z 10s -c 50 --no-tui --output-format json "http://$BENCH_ADDR/baseline/x" > "$tmp/ka.json" 2>/dev/null
  echo "  keep-alive ceiling -> $(oha_row "$tmp/ka.json")"
  local idle; idle="$(grep VmRSS /proc/$PROXY_PID/status | awk '{print $2}')"
  echo "  idle RSS: ${idle} kB"
  stop_proxy
  echo "  (this tier is a fast sanity check, not a tracked baseline — no CSV written; run 'all' for the real suite)"
}

# ---------------------------------------------------------------- Phase 1 ceiling (plain h1)
phase_ceiling(){
  log "Phase 1 — plain h1 ceiling: keep-alive RPS + cold-connection CPS (oha) -> ceiling.csv"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" BACKEND_LATENCY_MS=0 || return 1
  local tmp; tmp="$(mktemp -d)"
  warm_oha -c 50 "http://$BENCH_ADDR/baseline/x"
  gen "$OHA" -z 30s -c 50 --no-tui --output-format json "http://$BENCH_ADDR/baseline/x" > "$tmp/ka.json" 2>/dev/null
  echo "  keep-alive        -> $(oha_row "$tmp/ka.json")"
  # Cold-connection churn leaves the client side with one TIME_WAIT socket per request; 30 s at
  # tens of k rps can brush the ephemeral-port range (net.ipv4.ip_local_port_range). Failures from
  # port exhaustion would show as errors here, not latency — check the error count if cold rps
  # collapses versus keep-alive by more than the handshake cost.
  warm_oha -c 50 --disable-keepalive "http://$BENCH_ADDR/baseline/x"
  gen "$OHA" -z 30s -c 50 --no-tui --disable-keepalive --output-format json "http://$BENCH_ADDR/baseline/x" > "$tmp/cold.json" 2>/dev/null
  echo "  cold (TCP/req)    -> $(oha_row "$tmp/cold.json")"
  stop_proxy
  { echo "variant,kpi,rps,p50,p90,p95,p99"
    add(){ read -r rps p50 p90 p95 p99 _ <<<"$(oha_row "$3")"; echo "$1,$2,$rps,$p50,$p90,$p95,$p99"; }
    # RR = Request/Response on a persistent connection; CRR = Connect/Request/Response (RFC 9411 §7.2/7.3 shape).
    add "keep-alive" "RR" "$tmp/ka.json"
    add "cold (TCP/req)" "CRR" "$tmp/cold.json"; } > "$DATA/ceiling.csv"
  cat "$DATA/ceiling.csv"
}

# ---------------------------------------------------------------- Phase 2.1 sweep
phase_sweep(){
  log "Phase 2.1 — closed-loop sweep (k6 constant-vus) -> sweep.csv"
  launch load-balancing "$LB_ADDR" "http://$LB_ADDR/" || return 1
  local tmp; tmp="$(mktemp -d)"
  for vus in 50 100 200 400 800; do
    log "  VU=$vus"
    gen "$K6" run -q "${INFLUX_OUT[@]}" \
      -e TARGET="http://$LB_ADDR/" -e VUS=$vus -e DUR=60s -e OUT="$tmp/vu$vus.json" \
      "$BENCH/k6/lb-sweep-step.js" 2>&1 | tail -1
  done
  stop_proxy
  python3 -c '
import json,glob,csv,os,sys
rows=[]
for f in sorted(glob.glob(os.path.join(sys.argv[1],"vu*.json")), key=lambda p:int(p.split("vu")[-1].split(".")[0])):
    d=json.load(open(f)); rows.append(d)
with open(sys.argv[2],"w",newline="") as o:
    w=csv.writer(o); w.writerow(["vus","rps","p50","p95","p99","p99_9","failed"])
    for d in rows:
        w.writerow([d["vus"],round(d["rps"],1),round(d["p50"],3),round(d["p95"],3),
                    round(d["p99"],3),round(d["p99_9"],3),round(d["failed"],5)])
print("wrote sweep.csv")
' "$tmp" "$DATA/sweep.csv"
  cat "$DATA/sweep.csv"
}

# ---------------------------------------------------------------- Phase 2.2 open-loop tail (authoritative: plecto-loadgen)
phase_openloop(){
  log "Phase 2.2 — open-loop tail (schedule-latency / wrk2 model) -> openloop.json"
  local peak rate
  peak="$(python3 -c 'import json,csv,sys; rows=list(csv.DictReader(open(sys.argv[1]))); print(int(max(float(r["rps"]) for r in rows)))' "$DATA/sweep.csv" 2>/dev/null || echo 0)"
  # Prefer a rate the *proxy* can serve with headroom. Default 70% of closed-loop peak; pin with
  # OPENLOOP_RATE when the closed-loop peak is generator-bound (k6 VU ceiling), not proxy-bound.
  rate="${OPENLOOP_RATE:-$(python3 -c "print(max(500,int($peak*0.7)))")}"
  echo "  closed-loop peak≈${peak} rps -> open-loop rate=${rate} rps (generator=${OPENLOOP_GEN:-loadgen})"
  launch load-balancing "$LB_ADDR" "http://$LB_ADDR/" || return 1
  case "${OPENLOOP_GEN:-loadgen}" in
    k6)
      # Legacy path: k6 constant-arrival-rate. Kept for A/B; often generator-bound above ~60k/s.
      gen "$K6" run -q "${INFLUX_OUT[@]}" \
        -e TARGET="http://$LB_ADDR/" -e RATE=$rate -e DUR=90s -e OUT="$DATA/openloop.json" \
        "$BENCH/k6/lb-openloop.js" 2>&1 | tail -2
      ;;
    loadgen|*)
      loadgen openloop --target "http://$LB_ADDR/" --rate "$rate" --duration 90 --warmup 5 \
        --workers "${OPENLOOP_WORKERS:-64}" --out "$DATA/openloop.json"
      ;;
  esac
  stop_proxy
  cat "$DATA/openloop.json"
}

# ---------------------------------------------------------------- Industry core KPIs (RFC 9411-shaped, fully local)
phase_industry(){
  # Throughput (RR+CRR) + CO-safe transaction latency at fixed arrival + application mix.
  # Skips resilience timelines / WASM ladder / TLS decomposition — those stay in `all`.
  log "Industry core — ceiling (RR/CRR) + open-loop latency + traffic mix"
  phase_ceiling
  if command -v k6 >/dev/null 2>&1 || [[ -n "${K6:-}" && -x "${K6}" ]]; then
    phase_sweep
  elif [[ -z "${OPENLOOP_RATE:-}" ]]; then
    echo "industry: k6 not found and OPENLOOP_RATE unset — set OPENLOOP_RATE=<rps> or install k6 for sweep"
    return 1
  else
    echo "industry: skipping sweep (no k6); using OPENLOOP_RATE=${OPENLOOP_RATE}"
    echo "vus,rps,p50,p95,p99,p99_9,failed" > "$DATA/sweep.csv"
    echo "0,${OPENLOOP_RATE},0,0,0,0,0" >> "$DATA/sweep.csv"
  fi
  phase_openloop
  if command -v k6 >/dev/null 2>&1 || [[ -n "${K6:-}" && -x "${K6}" ]]; then
    phase_mix
  else
    echo "industry: skipping mix (no k6)"
  fi
}

# ---------------------------------------------------------------- Phase 2.3 round-robin
phase_rr(){
  log "Phase 2.3 — round-robin distribution -> rr.csv"
  launch load-balancing "$LB_ADDR" "http://$LB_ADDR/" || return 1
  loadgen rr --target "http://$LB_ADDR/" --total 120000 --workers 48 --out "$DATA/rr.csv"
  stop_proxy
  cat "$DATA/rr.csv"
}

# ---------------------------------------------------------------- Phase 2.4 resilience
phase_ejection(){
  log "Phase 2.4 — fault injection timeline -> ejection_timeline.csv + ejection_events.csv"
  launch load-balancing "$LB_ADDR" "http://$LB_ADDR/" || return 1
  local be; be=($(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$BLOG" | head -3))
  [[ "${#be[@]}" -eq 3 ]] || { echo "expected 3 backends, got ${be[*]:-none}"; stop_proxy; return 1; }
  echo "  backends: a=${be[0]} b=${be[1]} c=${be[2]}"
  loadgen ejection --target "http://$LB_ADDR/" --rate 4000 --duration 75 --warmup 5 --workers 64 \
    --toggle "a=${be[0]}/toggle" "b=${be[1]}/toggle" "c=${be[2]}/toggle" \
    --out "$DATA/ejection_timeline.csv" --events-out "$DATA/ejection_events.csv"
  stop_proxy
  echo "--- events ---"; cat "$DATA/ejection_events.csv"
  echo "--- timeline head/tail ---"; head -3 "$DATA/ejection_timeline.csv"; tail -3 "$DATA/ejection_timeline.csv"
}

# ---------------------------------------------------------------- Phase 2.5 endpoint-set swap (ADR 000044)
phase_swap(){
  log "Phase 2.5 — endpoint-set swap under load (ADR 000044: reload changes the address SET, not just health) -> swap.csv + swap_events.csv"
  launch swap-bench "$SWAP_ADDR" "http://$SWAP_ADDR/" || return 1
  # Backends print as http://host:port in the banner (for curl-ability); the manifest's
  # `addresses` list wants bare host:port (SocketAddr), like swap-bench's own generated manifest.
  local be; be=($(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$BLOG" | head -4))
  [[ "${#be[@]}" -eq 4 ]] || { echo "expected 4 backends (a,b,c,d), got ${be[*]:-none}"; stop_proxy; return 1; }
  local bare=("${be[@]#http://}")
  local manifest; manifest="$(grep -oE '[^[:space:]]+/plecto\.toml' "$BLOG" | head -1)"
  [[ -n "$manifest" ]] || { echo "could not find the manifest path in the banner"; stop_proxy; return 1; }
  echo "  backends: a=${be[0]} b=${be[1]} c=${be[2]} d=${be[3]} (spare)"
  echo "  manifest: $manifest"
  # At t=15s (post-warmup): drop c, add the spare d — a genuinely different address set, the shape
  # a periodic-DNS re-resolution swap takes (ADR 000044), not a health-based ejection ([[000017]]).
  local swapped; swapped="$(mktemp)"
  cat > "$swapped" <<EOF
[[upstream]]
name = "pool"
addresses = ["${bare[0]}", "${bare[1]}", "${bare[3]}"]
[upstream.health]
path = "/healthz"
interval_ms = 500
timeout_ms = 300
healthy_threshold = 2
unhealthy_threshold = 2

[[route]]
upstream = "pool"
[route.match]
path_prefix = "/"
EOF
  loadgen swap --target "http://$SWAP_ADDR/" --rate 4000 --duration 60 --warmup 5 --workers 64 \
    --exec-at "15=cp '$swapped' '$manifest' && kill -HUP $PROXY_PID" \
    --out "$DATA/swap.csv" --events-out "$DATA/swap_events.csv"
  stop_proxy
  rm -f "$swapped"
  echo "--- events ---"; cat "$DATA/swap_events.csv"
  echo "--- timeline head/tail ---"; head -3 "$DATA/swap.csv"; tail -3 "$DATA/swap.csv"
}

# ---------------------------------------------------------------- Phase 3 WASM
phase_wasm(){
  [[ -f "$DATA/ceiling.csv" ]] || phase_ceiling
  log "Phase 3.1 — WASM cost ladder (oha 50c, 0 ms backend) -> wasm_overhead.csv"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" BACKEND_LATENCY_MS=0 || return 1
  local tmp; tmp="$(mktemp -d)"
  # The isolated cost ladder over one backend; adjacent deltas isolate one cost each:
  #   baseline(native) -> noop-pooled(dispatch+acquire) -> noop-fresh(instantiation) ->
  #   trusted(a real filter's work) -> ondemand(that filter fresh-per-request).
  # `baseline` is NOT re-measured here — it's the exact same route/config `ceiling` already
  # measured, so its row comes from ceiling.csv (see the file header's consolidation note).
  local ladder=(noop-pooled noop-fresh trusted ondemand)
  read -r base_rps base_p50 base_p90 base_p95 base_p99 <<<"$(ceiling_row)"
  python3 -c 'import json,sys; json.dump({"summary":{"requestsPerSec":float(sys.argv[2])}}, open(sys.argv[1],"w"))' \
    "$tmp/baseline.json" "$base_rps"
  echo "  /baseline -> ${base_rps} rps  p50=${base_p50} p90=${base_p90} p95=${base_p95} p99=${base_p99}  (from ceiling.csv, not re-measured)"
  for r in "${ladder[@]}"; do
    local hdr=(); [[ "$r" == trusted || "$r" == ondemand ]] && hdr=(-H "x-api-key: alice-secret")
    warm_oha -c 50 "${hdr[@]}" "http://$BENCH_ADDR/$r/x"
    gen "$OHA" -z 60s -c 50 --no-tui --output-format json "${hdr[@]}" \
      "http://$BENCH_ADDR/$r/x" > "$tmp/$r.json" 2>/dev/null
    echo "  /$r -> $(oha_row "$tmp/$r.json")"
  done

  # Phase 3.2 — the ceiling runs above saturate the proxy, so their tails are queueing at max load
  # (not meaningful latency). Re-run each rung at ONE fixed below-knee rate — 60% of the SLOWEST
  # rung's ceiling, so every rung sees identical offered load — with oha's --latency-correction
  # (coordinated-omission-safe). These tails are the honest per-rung latency comparison. Unlike the
  # ceiling row above, `baseline`'s TAIL genuinely cannot be borrowed (ceiling.csv has no fixed-rate
  # tail measurement — the rate itself is derived from this ladder's own floor), so it is measured
  # live here like every other rung.
  local floor qlat
  floor="$(python3 -c '
import json,sys
print(int(min(json.load(open(f))["summary"]["requestsPerSec"] for f in sys.argv[1:])))
' "$tmp/baseline.json" "$tmp/noop-pooled.json" "$tmp/noop-fresh.json" "$tmp/trusted.json" "$tmp/ondemand.json")"
  qlat="${WASM_QLAT:-$(( floor * 60 / 100 ))}"
  log "Phase 3.2 — cost-ladder tails at fixed ${qlat} rps (oha -q --latency-correction) -> wasm_overhead_tail.csv"
  { echo "route,rate,rps,p50,p90,p95,p99"
    for r in baseline "${ladder[@]}"; do
      local hdr=(); [[ "$r" == trusted || "$r" == ondemand ]] && hdr=(-H "x-api-key: alice-secret")
      gen "$OHA" -z 30s -c 50 -q "$qlat" --latency-correction --no-tui --output-format json "${hdr[@]}" \
        "http://$BENCH_ADDR/$r/x" > "$tmp/tail_$r.json" 2>/dev/null
      read -r rps p50 p90 p95 p99 _ <<<"$(oha_row "$tmp/tail_$r.json")"
      echo "$r,$qlat,$rps,$p50,$p90,$p95,$p99"
    done; } > "$DATA/wasm_overhead_tail.csv"
  stop_proxy
  { echo "route,rps,p50,p90,p95,p99"
    echo "baseline,$base_rps,$base_p50,$base_p90,$base_p95,$base_p99"
    for r in "${ladder[@]}"; do
      read -r rps p50 p90 p95 p99 _ <<<"$(oha_row "$tmp/$r.json")"
      echo "$r,$rps,$p50,$p90,$p95,$p99"
    done; } > "$DATA/wasm_overhead.csv"
  cat "$DATA/wasm_overhead.csv"
  echo "--- tails at ${qlat} rps ---"
  cat "$DATA/wasm_overhead_tail.csv"

  log "Phase 3.3 — short-circuit mixed (k6 2000 rps, 15 ms backend, 90/10) -> wasm_mixed.csv"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" BACKEND_LATENCY_MS=15 || return 1
  gen "$K6" run -q "${INFLUX_OUT[@]}" -e BASE="http://$BENCH_ADDR" -e RATE=2000 -e DUR=60s -e OUT="$tmp/mixed.json" \
    "$BENCH/k6-wasm/mixed.js" 2>&1 | tail -2
  stop_proxy
  python3 -c '
import json,csv,sys
d=json.load(open(sys.argv[1])); dur=float(sys.argv[3])
with open(sys.argv[2],"w",newline="") as o:
    w=csv.writer(o); w.writerow(["outcome","rps","p50","p95","p99","count"])
    w.writerow(["accept",round(d["accepted"]/dur,1),round(d["accept_p50"],3),round(d["accept_p95"],3),round(d["accept_p99"],3),d["accepted"]])
    w.writerow(["reject",round(d["rejected"]/dur,1),round(d["reject_p50"],3),round(d["reject_p95"],3),round(d["reject_p99"],3),d["rejected"]])
print("wrote wasm_mixed.csv")
' "$tmp/mixed.json" "$DATA/wasm_mixed.csv" 60
  cat "$DATA/wasm_mixed.csv"
}

# ---------------------------------------------------------------- Phase 4 TLS
phase_tls(){
  [[ -f "$DATA/ceiling.csv" ]] || phase_ceiling
  log "Phase 4 — TLS decomposition (oha) -> tls.csv"
  local tmp; tmp="$(mktemp -d)"
  # plain h1 baseline: read from ceiling.csv (same route, same server, same oha flags — measuring
  # it again here would be the exact redundant run the harness merge eliminated).
  read -r plain_rps plain_p50 _ _ plain_p99 <<<"$(ceiling_row)"
  echo "  plain (h1)        -> ${plain_rps} rps  p50=${plain_p50} p99=${plain_p99}  (from ceiling.csv, not re-measured)"
  # tls-http: /api/hello over TLS. B=h1 keepalive, C=h1 handshake-per-request, D=h2.
  launch tls-http "$TLS_ADDR" "" || return 1
  # health over https (self-signed): tolerant probe before measuring.
  for _ in $(seq 40); do gen "$OHA" -n 1 --insecure --no-tui "https://$TLS_ADDR/api/hello" >/dev/null 2>&1 && break; sleep 0.25; done
  warm_oha -c 50 --insecure "https://$TLS_ADDR/api/hello"
  gen "$OHA" -z 30s -c 50 --no-tui --insecure --output-format json "https://$TLS_ADDR/api/hello" > "$tmp/tls_h1_ka.json" 2>/dev/null
  echo "  tls h1 keepalive  -> $(oha_row "$tmp/tls_h1_ka.json")"
  warm_oha -c 50 --insecure --disable-keepalive "https://$TLS_ADDR/api/hello"
  gen "$OHA" -z 30s -c 50 --no-tui --insecure --disable-keepalive --output-format json "https://$TLS_ADDR/api/hello" > "$tmp/tls_h1_hs.json" 2>/dev/null
  echo "  tls h1 handshake  -> $(oha_row "$tmp/tls_h1_hs.json")"
  warm_oha -c 50 --insecure --http2 "https://$TLS_ADDR/api/hello"
  gen "$OHA" -z 30s -c 50 --no-tui --insecure --http2 --output-format json "https://$TLS_ADDR/api/hello" > "$tmp/tls_h2.json" 2>/dev/null
  echo "  tls (h2)          -> $(oha_row "$tmp/tls_h2.json")"
  stop_proxy
  { echo "variant,rps,p50,p99"
    echo "plain (h1),$plain_rps,$plain_p50,$plain_p99"
    add(){ read -r rps p50 _ _ p99 _ <<<"$(oha_row "$2")"; echo "$1,$rps,$p50,$p99"; }
    add "tls h1 keepalive" "$tmp/tls_h1_ka.json"
    add "tls h1 handshake" "$tmp/tls_h1_hs.json"
    add "tls (h2)" "$tmp/tls_h2.json"; } > "$DATA/tls.csv"
  cat "$DATA/tls.csv"
}

# ---------------------------------------------------------------- Phase 5 footprint
phase_footprint(){
  log "Phase 5 — footprint (idle RSS, bytes/conn) -> footprint.txt"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" BACKEND_LATENCY_MS=0 || return 1
  sleep 2
  local idle; idle="$(grep VmRSS /proc/$PROXY_PID/status | awk '{print $2}')"  # kB
  echo "idle VmRSS: ${idle} kB" | tee "$DATA/footprint.txt"
  # hold K steady keep-alive connections and re-read RSS
  loadgen hold --target "http://$BENCH_ADDR/baseline/x" --conns 1000 --seconds 6 &
  HOLD=$!
  sleep 3
  local busy; busy="$(grep VmRSS /proc/$PROXY_PID/status | awk '{print $2}')"
  wait $HOLD 2>/dev/null
  echo "RSS with ~1000 conns: ${busy} kB" | tee -a "$DATA/footprint.txt"
  python3 -c "print(f'bytes/conn ≈ {($busy-$idle)*1024/1000:.0f}')" | tee -a "$DATA/footprint.txt"
  stop_proxy
}

# ---------------------------------------------------------------- Phase 6 rate limit (ADR 000026)
phase_ratelimit(){
  local tmp; tmp="$(mktemp -d)"
  # -- 6.1 overhead: a generous (never-deny) bucket isolates the limiter's hot-path cost. Spread the
  # load across many keys (realistic multi-tenant), compare /ratelimit vs the no-filter /baseline. --
  log "Phase 6.1 — rate-limit overhead (generous bucket, multi-key) -> ratelimit_overhead.csv"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" || return 1
  for rt in baseline ratelimit; do
    gen "$K6" run -q "${INFLUX_OUT[@]}" -e BASE="http://$BENCH_ADDR" -e ROUTE_PATH="/$rt" \
      -e KEYS=1000 -e VUS=50 -e DUR=30s -e OUT="$tmp/ov_$rt.json" \
      "$BENCH/k6-wasm/ratelimit-overhead.js" 2>&1 | tail -1
  done
  stop_proxy
  python3 -c '
import json,csv,sys
with open(sys.argv[3],"w",newline="") as o:
    w=csv.writer(o); w.writerow(["route","rps","p50","p99"])
    for f in sys.argv[1:3]:
        d=json.load(open(f)); w.writerow([d["route"],round(d["rps"],1),round(d["p50"],3),round(d["p99"],3)])
' "$tmp/ov_baseline.json" "$tmp/ov_ratelimit.json" "$DATA/ratelimit_overhead.csv"
  cat "$DATA/ratelimit_overhead.csv"

  # -- 6.2 enforcement + fairness: a TIGHT bucket (refill 1000/s, burst 2000) host-set in the manifest.
  # Offer well above the limit and watch the allowed rate converge to the refill rate; a hot key must
  # not starve a light one (independent per-key state). --
  log "Phase 6.2 — enforcement + fairness (tight bucket, refill 1000/s) -> ratelimit_{enforce,fairness}.csv"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" \
    RL_CAPACITY=2000 RL_REFILL_TOKENS=1000 RL_REFILL_INTERVAL_MS=1000 || return 1
  gen "$K6" run -q "${INFLUX_OUT[@]}" -e BASE="http://$BENCH_ADDR" -e RATE=5000 -e DUR=30s \
    -e OUT="$tmp/enforce.json" "$BENCH/k6-wasm/ratelimit-enforce.js" 2>&1 | tail -1
  gen "$K6" run -q "${INFLUX_OUT[@]}" -e BASE="http://$BENCH_ADDR" -e HOT_RATE=4000 -e LIGHT_RATE=500 \
    -e DUR=30s -e OUT="$tmp/fairness.json" "$BENCH/k6-wasm/ratelimit-fairness.js" 2>&1 | tail -1
  stop_proxy
  python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
print("metric,value")
for k in ("target_rps","achieved_rps","allowed_rps","limited_frac","accept_p50","accept_p99","limit_p99","accepted","limited"):
    print(f"{k},{round(d[k],3)}")
' "$tmp/enforce.json" > "$DATA/ratelimit_enforce.csv"
  python3 -c '
import json,csv,sys
d=json.load(open(sys.argv[1]))
with open(sys.argv[2],"w",newline="") as o:
    w=csv.writer(o); w.writerow(["key","offered_rps","allowed_rps","shed_frac"])
    hot_shed=d["hot_429"]/max(1,d["hot_ok"]+d["hot_429"])
    w.writerow(["hot",d["hot_offered_rps"],round(d["hot_allowed_rps"],1),round(hot_shed,4)])
    w.writerow(["light",d["light_offered_rps"],round(d["light_allowed_rps"],1),round(d["light_429_frac"],4)])
' "$tmp/fairness.json" "$DATA/ratelimit_fairness.csv"
  echo "--- enforce ---"; cat "$DATA/ratelimit_enforce.csv"
  echo "--- fairness ---"; cat "$DATA/ratelimit_fairness.csv"
}

# ---------------------------------------------------------------- Phase 7 request body (ADR 000025 / 000038)
phase_body(){
  log "Phase 7 — request-body hook: payload sweep + zero-copy bypass + arena cap (ADR 000038) -> body.csv"
  # As-shipped allocator default (Fix 1): cap glibc arenas the way the `plecto` bin does at startup
  # (glibc reads MALLOC_ARENA_MAX at process start; equivalent to the in-process mallopt(M_ARENA_MAX,4)).
  # Routes: baseline (no filter) / body (filter-hello, reads the body → buffers) / body-headeronly
  # (filter-quickstart, header-only → the body streams through, ADR 000038 zero-copy bypass).
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" MALLOC_ARENA_MAX=4 || return 1
  local tmp; tmp="$(mktemp -d)"
  { echo "size,route,rps,req_mbps,p50,p99"
    for size in 1024 102400 1048576; do
      for rt in baseline body body-headeronly; do
        gen "$K6" run -q "${INFLUX_OUT[@]}" -e BASE="http://$BENCH_ADDR" -e ROUTE_PATH="/$rt" \
          -e SIZE=$size -e VUS=50 -e DUR=20s -e OUT="$tmp/b_${size}_$rt.json" \
          "$BENCH/k6-wasm/body-transform.js" >/dev/null 2>&1
        python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
print("%d,%s,%.1f,%.2f,%.3f,%.3f"%(d["size"],d["route"],d["rps"],d["req_mbps"],d["p50"],d["p99"]))
' "$tmp/b_${size}_$rt.json"
      done
    done; } > "$DATA/body.csv"
  stop_proxy
  cat "$DATA/body.csv"

  # RSS at 1 MB × 50 VUs, sampled mid-load — a FRESH proxy per route so a prior route's grown linear
  # memory / arena state can't contaminate the next (the single-long-lived-proxy flaw that the
  # mem_matrix investigation fixed). Combined proxy+in-process-upstream, MALLOC_ARENA_MAX=4 (shipped).
  # For the time-series peak/settled decomposition + allocator sweep see bench/perf/mem_matrix.py.
  { echo "route,rss_kb"
    for rt in baseline body body-headeronly; do
      launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" MALLOC_ARENA_MAX=4 >/dev/null || return 1
      gen "$K6" run -q -e BASE="http://$BENCH_ADDR" -e ROUTE_PATH="/$rt" -e SIZE=1048576 -e VUS=50 \
        -e DUR=15s -e OUT="$tmp/rss_$rt.json" "$BENCH/k6-wasm/body-transform.js" >/dev/null 2>&1 &
      local kpid=$!
      sleep 12
      echo "$rt,$(grep VmRSS /proc/$PROXY_PID/status | awk '{print $2}')"
      wait $kpid 2>/dev/null
      stop_proxy
    done; } > "$DATA/body_rss.csv"
  echo "--- RSS at 1MB x 50 VUs (fresh proxy per route, MALLOC_ARENA_MAX=4) ---"
  cat "$DATA/body_rss.csv"
}

# ---------------------------------------------------------------- Phase 4b HTTP/3 functional check
phase_h3(){
  # HTTP/3 is a first-class server feature (tls-http serves it over QUIC). A rigorous, CO-safe H3
  # LOAD benchmark needs an H3-capable open-loop generator (e.g. h2load --npn-list h3);
  # oha and k6 lack native H3, so we VERIFY H3 works end-to-end here and defer the load
  # numbers (see performance/README.md).
  log "Phase 4b — HTTP/3 functional check (curl --http3-only over QUIC) -> h3.txt"
  curl --version 2>/dev/null | grep -qi HTTP3 || { echo "curl lacks HTTP/3; skipping" | tee "$DATA/h3.txt"; return 0; }
  launch tls-http "$TLS_ADDR" "" || return 1
  for _ in $(seq 40); do curl -fsS -k "https://$TLS_ADDR/api/hello" >/dev/null 2>&1 && break; sleep 0.25; done
  local out
  out=$(curl -k -s -o /dev/null -w 'status=%{http_code} http_version=%{http_version}' --http3-only "https://$TLS_ADDR/api/hello" 2>/dev/null)
  stop_proxy
  echo "curl --http3-only /api/hello -> $out" | tee "$DATA/h3.txt"
}

# ---------------------------------------------------------------- Phase 9 weighted request mix
phase_mix(){
  log "Phase 9 — weighted request mix (60/25/10/5 read/auth/write/large) + paired same-rate read-only baseline -> mix.csv"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" RESP_BYTES=1024 || return 1
  local tmp; tmp="$(mktemp -d)"
  # read-only first (the control), then the mix, both at the SAME arrival rate against the same
  # proxy: the per-class deltas are attributable to the traffic blend, not the offered load.
  for prof in read-only mix; do
    gen "$K6" run -q "${INFLUX_OUT[@]}" -e BASE="http://$BENCH_ADDR" -e RATE="${MIX_RATE:-20000}" \
      -e DUR=60s -e PROFILE="$prof" -e OUT="$tmp/mix_$prof.json" \
      "$BENCH/k6/weighted-mix.js" 2>&1 | tail -2
  done
  stop_proxy
  python3 -c '
import json,csv,sys
with open(sys.argv[3],"w",newline="") as o:
    w=csv.writer(o); w.writerow(["profile","class","p50","p99","p99_9"])
    for f in (sys.argv[1],sys.argv[2]):
        d=json.load(open(f)); p=d["profile"]
        w.writerow([p,"read",round(d["read_p50"],3),round(d["read_p99"],3),round(d["read_p99_9"],3)])
        if p=="mix":
            w.writerow([p,"auth",round(d["auth_p50"],3),round(d["auth_p99"],3),""])
            w.writerow([p,"write",round(d["write_p50"],3),round(d["write_p99"],3),""])
            w.writerow([p,"large",round(d["large_p50"],3),round(d["large_p99"],3),""])
        print("%s: offered %.0f rps, dropped %d, 429s %d"%(p,d["offered_rps"],d["dropped"],d.get("limited",0)))
' "$tmp/mix_read-only.json" "$tmp/mix_mix.json" "$DATA/mix.csv"
  cat "$DATA/mix.csv"
}

# ---------------------------------------------------------------- Phase 10 WebSocket upgrade (ADR 000048)
phase_ws(){
  log "Phase 10 — WebSocket Upgrade tunnel (ADR 000048): handshake rate + tunnel footprint + echo throughput -> ws_*.csv"
  launch bench-server "$BENCH_ADDR" "http://$BENCH_ADDR/baseline/x" BACKEND_LATENCY_MS=0 || return 1
  local tmp; tmp="$(mktemp -d)"

  log "  10.1 — handshake rate (open-loop, paced) -> ws_handshake.csv"
  loadgen ws --mode handshake --target "ws://$BENCH_ADDR/ws" --rate 500 --duration 20 --warmup 5 --workers 32 \
    --out "$DATA/ws_handshake.csv"
  cat "$DATA/ws_handshake.csv"

  log "  10.2 — tunnel footprint: 1000 held tunnels, RSS before/after -> ws_footprint.csv"
  local idle; idle="$(grep VmRSS /proc/$PROXY_PID/status | awk '{print $2}')"
  loadgen ws --mode hold --target "ws://$BENCH_ADDR/ws" --conns 1000 --seconds 6 &
  local HOLD=$!
  sleep 3
  local busy; busy="$(grep VmRSS /proc/$PROXY_PID/status | awk '{print $2}')"
  wait $HOLD 2>/dev/null
  { echo "metric,value_kb"
    echo "idle_rss,$idle"
    echo "rss_with_1000_tunnels,$busy"
    python3 -c "print(f'bytes_per_tunnel,{($busy-$idle)*1024/1000:.0f}')"
  } > "$DATA/ws_footprint.csv"
  cat "$DATA/ws_footprint.csv"

  log "  10.3 — echo throughput: 50 conns, message-size sweep -> ws_echo.csv"
  echo "conns,size_bytes,duration_s,messages,messages_per_sec,mb_per_sec,p50_ms,p90_ms,p99_ms" > "$DATA/ws_echo.csv"
  for size in 1024 65536; do
    loadgen ws --mode echo --target "ws://$BENCH_ADDR/ws" --conns 50 --size "$size" --duration 20 --warmup 3 \
      --out "$tmp/ws_echo_$size.csv"
    tail -1 "$tmp/ws_echo_$size.csv" >> "$DATA/ws_echo.csv"
  done
  stop_proxy
  cat "$DATA/ws_echo.csv"
}

case "${1:-all}" in
  quick) phase_quick;; ceiling) phase_ceiling;;
  sweep) phase_sweep;; openloop) phase_openloop;; rr) phase_rr;; ejection) phase_ejection;; swap) phase_swap;;
  wasm) phase_wasm;; tls) phase_tls;; h3) phase_h3;; ws) phase_ws;; footprint) phase_footprint;;
  ratelimit) phase_ratelimit;; body) phase_body;; mix) phase_mix;; industry) phase_industry;;
  all) phase_ceiling; phase_sweep; phase_openloop; phase_rr; phase_ejection; phase_swap; phase_wasm; \
       phase_tls; phase_h3; phase_ws; phase_ratelimit; phase_body; phase_mix; phase_footprint;;
  *) echo "unknown phase: $1"; exit 2;;
esac
log "done: $1"
