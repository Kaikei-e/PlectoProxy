// Measure ONE route at a fixed concurrency with a valid key (or none, for /baseline).
// Emits a compact JSON summary so the cost of each decision path can be compared.
import http from "k6/http";
import { check } from "k6";

const BASE = __ENV.BASE || "http://localhost:8085";
const ROUTE = __ENV.ROUTE_PATH || "/baseline";
const KEY = __ENV.API_KEY || "alice-secret";
const VUS = Number(__ENV.VUS || 50);
const DUR = __ENV.DUR || "30s";
const OUT = __ENV.OUT || "summary.json";

export const options = {
  summaryTrendStats: ["avg", "min", "med", "p(90)", "p(95)", "p(99)", "max"],
  scenarios: { fixed: { executor: "constant-vus", vus: VUS, duration: DUR } },
};

export default function () {
  // /baseline has no filter, so it carries no key; filtered routes send a valid key.
  const params = ROUTE === "/baseline" ? {} : { headers: { "x-api-key": KEY } };
  const res = http.get(`${BASE}${ROUTE}/orders/42`, params);
  check(res, { "status 200": (r) => r.status === 200 });
}

export function handleSummary(data) {
  const d = data.metrics.http_req_duration.values;
  const out = {
    route: ROUTE,
    vus: VUS,
    rps: data.metrics.http_reqs.values.rate,
    reqs: data.metrics.http_reqs.values.count,
    failed_rate: data.metrics.http_req_failed.values.rate,
    p50: d.med, p90: d["p(90)"], p95: d["p(95)"], p99: d["p(99)"],
    avg: d.avg, max: d.max,
  };
  const line = `\n${ROUTE}: ${out.rps.toFixed(0)} rps  p50=${out.p50.toFixed(3)}ms ` +
    `p95=${out.p95.toFixed(3)}ms p99=${out.p99.toFixed(3)}ms  fail=${(out.failed_rate * 100).toFixed(2)}%\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
