// Fixed-rate arm of the gate's CPU-µs/req invariant (bench/perf/run-perf.sh phase_gate). Offers a
// CONSTANT arrival rate, well below saturation, to one route (/baseline or /ratelimit) so both arms
// of a pair complete the same number of requests in the same wall time. run-perf.sh snapshots the
// proxy's CPU time (proc_cpu.py) around this run and gate_verdict.py divides the delta by `reqs`.
// Because the rate is fixed, the generator is never the bottleneck and the proxy's CPU per request
// does not depend on how fast the generator can turn requests around — unlike a closed-loop delta.
//
// No warm-up exclusion: the CPU window spans the WHOLE run, so the divisor must be every completed
// request (`http_reqs`), not a post-warm-up subset. The proxy is already warm (the closed-loop
// tax pairs ran on the same launch just before).
import http from "k6/http";

const BASE = __ENV.BASE || "http://localhost:8086";
const ROUTE = __ENV.ROUTE_PATH || "/baseline";
const RATE = Number(__ENV.RATE || 20000);
const DUR = __ENV.DUR || "3s";
const KEYS = Number(__ENV.KEYS || 1000);
const OUT = __ENV.OUT || "ratelimit_cpu.json";

// Every VU holds its own keep-alive connection from ONE loopback source address, so the pool must
// stay under the fast path's per-source-IP connection cap (256, ADR 000092): connections past it
// are refused at accept and would show up as failures, not as load. At loopback service times
// Little's law needs only a handful of VUs for this rate.
const PREALLOC = Number(__ENV.PREALLOC || 50);
const MAX_VUS = Math.min(Number(__ENV.MAX_VUS || 200), 250);

export const options = {
  discardResponseBodies: true,
  summaryTrendStats: ["avg", "med", "p(99)"],
  scenarios: {
    fixed: {
      executor: "constant-arrival-rate",
      rate: RATE,
      timeUnit: "1s",
      duration: DUR,
      preAllocatedVUs: PREALLOC,
      maxVUs: MAX_VUS,
    },
  },
};

export default function () {
  // Same key walk as ratelimit-overhead.js (1000 keys, VU-decorrelated), so the bucket-lookup
  // pattern matches the closed-loop tax measurement.
  const params =
    ROUTE === "/baseline"
      ? {}
      : { headers: { "x-plecto-ratelimit": `k${(__VU * 7919 + __ITER) % KEYS}` } };
  http.get(`${BASE}${ROUTE}/x`, params);
}

export function handleSummary(data) {
  const m = data.metrics;
  const reqs = m.http_reqs ? m.http_reqs.values.count : 0;
  const out = {
    route: ROUTE,
    target_rps: RATE,
    achieved_rps: m.http_reqs ? m.http_reqs.values.rate : 0,
    reqs,
    failed: m.http_req_failed ? Math.round(m.http_req_failed.values.rate * reqs) : 0,
    dropped: m.dropped_iterations ? m.dropped_iterations.values.count : 0,
  };
  const line =
    `\n${ROUTE} @${RATE}/s: ${reqs} reqs (${out.achieved_rps.toFixed(0)} rps)  ` +
    `dropped=${out.dropped} failed=${out.failed}\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
