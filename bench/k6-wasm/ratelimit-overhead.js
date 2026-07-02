// Rate-limit OVERHEAD on the hot path (ADR 000026). With a generous bucket that never denies, measure
// the cost of consulting the host-native token bucket on every request versus the no-filter baseline.
// Closed-loop at fixed concurrency; requests spread across KEYS distinct bucket keys (realistic multi-
// tenant load, low single-key contention). Run once per ROUTE (/baseline vs /ratelimit); the rps / p99
// delta is the limiter's per-request tax. (Pair with a never-deny bucket via RL_* on the example.)
// The first WARMUP_S seconds send load unrecorded; DUR is the measured window.
import http from "k6/http";
import exec from "k6/execution";
import { Counter, Trend } from "k6/metrics";

const BASE = __ENV.BASE || "http://localhost:8086";
const ROUTE = __ENV.ROUTE_PATH || "/baseline";
const KEYS = Number(__ENV.KEYS || 1000);
const VUS = Number(__ENV.VUS || 50);
const DUR_S = parseInt(__ENV.DUR || "30", 10);
const WARMUP_S = Number(__ENV.WARMUP_S || 5);
const OUT = __ENV.OUT || "ratelimit_overhead.json";

const lat = new Trend("lat", true);
const reqs = new Counter("reqs_measured");
const fails = new Counter("fails_measured");

export const options = {
  discardResponseBodies: true,
  summaryTrendStats: ["avg", "min", "med", "p(90)", "p(95)", "p(99)", "max"],
  scenarios: {
    fixed: { executor: "constant-vus", vus: VUS, duration: `${WARMUP_S + DUR_S}s` },
  },
};

export default function () {
  // /baseline ignores the header; /ratelimit consults the bucket at this key. Spreading over KEYS
  // keys keeps per-key contention realistic rather than hammering one bucket's state. The VU offset
  // decorrelates the walk: with plain `__ITER % KEYS` every VU hits k0, k1, ... in lockstep, which
  // synchronises per-key access instead of spreading it.
  const params =
    ROUTE === "/baseline"
      ? {}
      : { headers: { "x-plecto-ratelimit": `k${(__VU * 7919 + __ITER) % KEYS}` } };
  const res = http.get(`${BASE}${ROUTE}/x`, params);
  if (Date.now() - exec.scenario.startTime < WARMUP_S * 1000) return;
  lat.add(res.timings.duration);
  reqs.add(1);
  if (res.status !== 200) fails.add(1);
}

export function handleSummary(data) {
  const d = data.metrics.lat.values;
  const n = data.metrics.reqs_measured ? data.metrics.reqs_measured.values.count : 0;
  const bad = data.metrics.fails_measured ? data.metrics.fails_measured.values.count : 0;
  const out = {
    route: ROUTE,
    vus: VUS,
    keys: ROUTE === "/baseline" ? 0 : KEYS,
    rps: n / DUR_S,
    reqs: n,
    failed_rate: bad / Math.max(1, n),
    p50: d.med, p90: d["p(90)"], p95: d["p(95)"], p99: d["p(99)"],
  };
  const line =
    `\n${ROUTE}: ${out.rps.toFixed(0)} rps  p50=${out.p50.toFixed(3)}ms ` +
    `p99=${out.p99.toFixed(3)}ms  fail=${(out.failed_rate * 100).toFixed(2)}%\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
