// One closed-loop step of the concurrency sweep (Phase 2.1).
// constant-vus at a fixed VU level; emits a compact JSON summary (rps + percentiles incl p99.9 +
// failed) so the orchestrator can assemble sweep.csv across VU levels. Run once per level.
// The first WARMUP_S seconds send load but are NOT recorded (cold proxy: route tables, upstream
// pools, page cache), so the summary reflects steady state only; DUR is the measured window.
import http from "k6/http";
import exec from "k6/execution";
import { Counter, Trend } from "k6/metrics";

const TARGET = __ENV.TARGET || "http://127.0.0.1:8080/";
const VUS = Number(__ENV.VUS || 50);
const DUR_S = parseInt(__ENV.DUR || "60", 10);
const WARMUP_S = Number(__ENV.WARMUP_S || 5);
const OUT = __ENV.OUT || "sweep_step.json";

const lat = new Trend("lat", true);
const reqs = new Counter("reqs_measured");
const fails = new Counter("fails_measured");
const servedBy = new Counter("served_by");

export const options = {
  discardResponseBodies: true, // status/headers only — keeps the generator's ceiling high
  summaryTrendStats: ["avg", "min", "med", "p(95)", "p(99)", "p(99.9)", "max"],
  scenarios: {
    step: { executor: "constant-vus", vus: VUS, duration: `${WARMUP_S + DUR_S}s` },
  },
};

export default function () {
  const res = http.get(TARGET);
  const inst = res.headers["X-Instance"];
  if (inst) servedBy.add(1, { instance: inst });
  if (Date.now() - exec.scenario.startTime < WARMUP_S * 1000) return; // warmup: send, don't record
  lat.add(res.timings.duration);
  reqs.add(1);
  if (res.status !== 200) fails.add(1);
}

export function handleSummary(data) {
  const d = data.metrics.lat.values;
  const n = data.metrics.reqs_measured ? data.metrics.reqs_measured.values.count : 0;
  const bad = data.metrics.fails_measured ? data.metrics.fails_measured.values.count : 0;
  const out = {
    vus: VUS,
    rps: n / DUR_S,
    reqs: n,
    failed: bad / Math.max(1, n),
    p50: d.med, p95: d["p(95)"], p99: d["p(99)"], p99_9: d["p(99.9)"],
  };
  const line = `\nvu${VUS}: ${out.rps.toFixed(0)} rps  p50=${out.p50.toFixed(2)} ` +
    `p95=${out.p95.toFixed(2)} p99=${out.p99.toFixed(2)} p99.9=${out.p99_9.toFixed(2)}ms ` +
    `failed=${(out.failed * 100).toFixed(2)}%\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
