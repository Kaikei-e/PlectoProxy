// One closed-loop step of the concurrency sweep (Phase 2.1).
// constant-vus at a fixed VU level for DUR; emits a compact JSON summary (rps + percentiles incl
// p99.9 + failed) so the orchestrator can assemble sweep.csv across VU levels. Run once per level.
import http from "k6/http";
import { check } from "k6";
import { Counter } from "k6/metrics";

const TARGET = __ENV.TARGET || "http://127.0.0.1:8080/";
const VUS = Number(__ENV.VUS || 50);
const DUR = __ENV.DUR || "60s";
const OUT = __ENV.OUT || "sweep_step.json";
const servedBy = new Counter("served_by");

export const options = {
  discardResponseBodies: false,
  summaryTrendStats: ["avg", "min", "med", "p(95)", "p(99)", "p(99.9)", "max"],
  scenarios: { step: { executor: "constant-vus", vus: VUS, duration: DUR } },
};

export default function () {
  const res = http.get(TARGET);
  check(res, { "status 200": (r) => r.status === 200 });
  const inst = res.headers["X-Instance"];
  if (inst) servedBy.add(1, { instance: inst });
}

export function handleSummary(data) {
  const d = data.metrics.http_req_duration.values;
  const out = {
    vus: VUS,
    rps: data.metrics.http_reqs.values.rate,
    reqs: data.metrics.http_reqs.values.count,
    failed: data.metrics.http_req_failed.values.rate,
    p50: d.med, p95: d["p(95)"], p99: d["p(99)"], p99_9: d["p(99.9)"],
  };
  const line = `\nvu${VUS}: ${out.rps.toFixed(0)} rps  p50=${out.p50.toFixed(2)} ` +
    `p95=${out.p95.toFixed(2)} p99=${out.p99.toFixed(2)} p99.9=${out.p99_9.toFixed(2)}ms ` +
    `failed=${(out.failed * 100).toFixed(2)}%\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
