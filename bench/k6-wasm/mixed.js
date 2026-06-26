// Realistic auth-gateway traffic: a fixed arrival rate against the pooled filter route, with a
// ~90% valid / ~10% invalid-or-missing key mix (expired tokens, scanners, misconfigured clients).
// Accepted requests reach the (latency-injected) backend; rejected ones are short-circuited 401 at
// the edge and never touch it — so we track the two paths' latencies separately.
import http from "k6/http";
import { Trend, Counter } from "k6/metrics";

const BASE = __ENV.BASE || "http://localhost:8085";
const RATE = Number(__ENV.RATE || 2000);
const DUR = __ENV.DUR || "40s";
const OUT = __ENV.OUT || "mixed.json";
const VALID = ["alice-secret", "bob-secret"];

const latAccept = new Trend("lat_accept", true);
const latReject = new Trend("lat_reject", true);
const accepted = new Counter("accepted");
const rejected = new Counter("rejected");

export const options = {
  summaryTrendStats: ["avg", "min", "med", "p(90)", "p(95)", "p(99)", "max"],
  scenarios: {
    mix: {
      executor: "constant-arrival-rate",
      rate: RATE, timeUnit: "1s", duration: DUR,
      preAllocatedVUs: 300, maxVUs: 3000,
    },
  },
};

export default function () {
  const roll = Math.floor(Math.random() * 10); // 0 => ~10% bad, 1..9 => valid
  let headers;
  if (roll === 0) {
    headers = Math.random() < 0.5 ? {} : { "x-api-key": "expired-or-bogus" };
  } else {
    headers = { "x-api-key": VALID[roll % VALID.length] };
  }
  const res = http.get(`${BASE}/trusted/orders/42`, { headers });
  if (res.status === 200) { latAccept.add(res.timings.duration); accepted.add(1); }
  else { latReject.add(res.timings.duration); rejected.add(1); }
}

export function handleSummary(data) {
  const a = data.metrics.lat_accept ? data.metrics.lat_accept.values : {};
  const rj = data.metrics.lat_reject ? data.metrics.lat_reject.values : {};
  const out = {
    offered_rps: data.metrics.http_reqs.values.rate,
    accepted: data.metrics.accepted ? data.metrics.accepted.values.count : 0,
    rejected: data.metrics.rejected ? data.metrics.rejected.values.count : 0,
    accept_p50: a.med || 0, accept_p95: a["p(95)"] || 0, accept_p99: a["p(99)"] || 0,
    reject_p50: rj.med || 0, reject_p95: rj["p(95)"] || 0, reject_p99: rj["p(99)"] || 0,
  };
  const line = `\nmixed: ${out.accepted} accepted / ${out.rejected} rejected  ` +
    `accept p95=${out.accept_p95.toFixed(2)}ms  reject p95=${out.reject_p95.toFixed(2)}ms\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
