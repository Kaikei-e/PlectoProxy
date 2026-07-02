// Request-body hook overhead + payload-size scaling (ADR 000025). POST a SIZE-byte body to a route and
// measure throughput / tail. On /body the host buffers the whole body and runs `on-request-body`
// (filter-hello uppercases it) before forwarding; on /baseline the body streams straight through. The
// /body-vs-/baseline delta at each SIZE is the buffer-then-decide cost, and how it scales with payload.
// The first WARMUP_S seconds send load unrecorded (allocator/arena state and linear memories settle);
// DUR is the measured window.
import http from "k6/http";
import exec from "k6/execution";
import { Counter, Trend } from "k6/metrics";

const BASE = __ENV.BASE || "http://localhost:8086";
const ROUTE = __ENV.ROUTE_PATH || "/baseline";
const SIZE = Number(__ENV.SIZE || 1024);
const VUS = Number(__ENV.VUS || 50);
const DUR_S = parseInt(__ENV.DUR || "20", 10);
const WARMUP_S = Number(__ENV.WARMUP_S || 5);
const OUT = __ENV.OUT || "body.json";

// Lowercase payload so the transform on /body is real work (uppercasing) rather than a no-op.
const PAYLOAD = "a".repeat(SIZE);

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
  const res = http.post(`${BASE}${ROUTE}/x`, PAYLOAD);
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
    size: SIZE,
    vus: VUS,
    rps: n / DUR_S,
    // request-body throughput in MB/s (the buffered/streamed payload), the I/O-bound signal at scale.
    req_mbps: (n * SIZE) / DUR_S / 1e6,
    failed_rate: bad / Math.max(1, n),
    p50: d.med, p95: d["p(95)"], p99: d["p(99)"],
  };
  const line =
    `\n${ROUTE} ${SIZE}B: ${out.rps.toFixed(0)} rps  ${out.req_mbps.toFixed(1)} MB/s  ` +
    `p50=${out.p50.toFixed(3)}ms p99=${out.p99.toFixed(3)}ms  fail=${(out.failed_rate * 100).toFixed(2)}%\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
