// Realistic auth-gateway traffic: a fixed arrival rate against the pooled filter route, with a
// ~90% valid / ~10% invalid-or-missing key mix (expired tokens, scanners, misconfigured clients).
// Accepted requests reach the (latency-injected) backend; rejected ones are short-circuited 401 at
// the edge and never touch it — so we track the two paths' latencies separately.
// The first WARMUP_S seconds send load unrecorded; DUR is the measured window.
import http from "k6/http";
import exec from "k6/execution";
import { Counter, Trend } from "k6/metrics";

const BASE = __ENV.BASE || "http://localhost:8085";
const RATE = Number(__ENV.RATE || 2000);
const DUR_S = parseInt(__ENV.DUR || "40", 10);
const WARMUP_S = Number(__ENV.WARMUP_S || 5);
const OUT = __ENV.OUT || "mixed.json";
const VALID = ["alice-secret", "bob-secret"];

const latAccept = new Trend("lat_accept", true);
const latReject = new Trend("lat_reject", true);
const accepted = new Counter("accepted");
const rejected = new Counter("rejected");
const noStatus = new Counter("no_status");

// The VU pool stays under the fast path's per-source-IP connection cap (ADR 000092): every VU holds
// its own keep-alive connection and the whole generator shares one loopback source address, so a
// larger pool has its excess connections refused at accept. Little's law makes the small pool ample
// anyway — at this arrival rate against a 15 ms backend only ~RATE x 0.015 requests are in flight.
const POOL = Number(__ENV.PREALLOC || 200);

export const options = {
  discardResponseBodies: true,
  summaryTrendStats: ["avg", "min", "med", "p(90)", "p(95)", "p(99)", "max"],
  scenarios: {
    mix: {
      executor: "constant-arrival-rate",
      rate: RATE, timeUnit: "1s", duration: `${WARMUP_S + DUR_S}s`,
      preAllocatedVUs: POOL, maxVUs: POOL,
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
  if (Date.now() - exec.scenario.startTime < WARMUP_S * 1000) return;
  if (res.status === 200) { latAccept.add(res.timings.duration); accepted.add(1); }
  else if (res.status === 401) { latReject.add(res.timings.duration); rejected.add(1); }
  // A connection the proxy never accepted carries no status at all. Counting it as a rejection
  // would let a generator-side artifact masquerade as short-circuit signal, so it gets its own
  // bucket and is reported: a non-zero count invalidates the accept/reject split.
  else { noStatus.add(1); }
}

export function handleSummary(data) {
  const a = data.metrics.lat_accept ? data.metrics.lat_accept.values : {};
  const rj = data.metrics.lat_reject ? data.metrics.lat_reject.values : {};
  const acc = data.metrics.accepted ? data.metrics.accepted.values.count : 0;
  const rej = data.metrics.rejected ? data.metrics.rejected.values.count : 0;
  const nost = data.metrics.no_status ? data.metrics.no_status.values.count : 0;
  const out = {
    offered_rps: (acc + rej + nost) / DUR_S,
    accepted: acc,
    rejected: rej,
    no_status: nost,
    accept_p50: a.med || 0, accept_p95: a["p(95)"] || 0, accept_p99: a["p(99)"] || 0,
    reject_p50: rj.med || 0, reject_p95: rj["p(95)"] || 0, reject_p99: rj["p(99)"] || 0,
  };
  const line = `\nmixed: ${out.accepted} accepted / ${out.rejected} rejected / ${nost} no-status  ` +
    `accept p95=${out.accept_p95.toFixed(2)}ms  reject p95=${out.reject_p95.toFixed(2)}ms\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
