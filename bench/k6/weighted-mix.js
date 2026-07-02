// A production-representative WEIGHTED traffic profile across several routes on one gateway, instead
// of hammering a single endpoint. Real API-gateway traffic is read-heavy, partly edge-checked
// (per-tenant rate limiting), with occasional writes and rare large payloads. The mix exercises the
// router's match cost and a realistic blend of the no-filter, limiter, and body-touching paths at
// once. Open-loop (constant-arrival-rate) so the tail is coordinated-omission-safe.
//
//   60% plain read      GET  /baseline/api/item/42   (no filter)
//   25% edge-check read GET  /ratelimit/api/item/42  (host token bucket, per-tenant key, never-deny)
//   10% small write     POST /body/api/item          (~1 KB body -> body-hook filter)
//    5% large write     POST /body/api/upload        (~100 KB body -> body-copy path)
//
// PROFILE=read-only sends 100% plain reads at the SAME arrival rate — the paired baseline that makes
// the mix's tails attributable to the traffic blend (router + filters + body), not to the rate.
// The first WARMUP_S seconds send load unrecorded; DUR is the measured window.
import http from "k6/http";
import exec from "k6/execution";
import { Counter, Trend } from "k6/metrics";

const BASE = __ENV.BASE || "http://localhost:8086";
const RATE = Number(__ENV.RATE || 20000);
const DUR_S = parseInt(__ENV.DUR || "60", 10);
const WARMUP_S = Number(__ENV.WARMUP_S || 5);
const PROFILE = __ENV.PROFILE || "mix"; // "mix" | "read-only"
const TENANTS = Number(__ENV.TENANTS || 200);
const OUT = __ENV.OUT || "mix.json";

const latRead = new Trend("lat_read", true);
const latAuth = new Trend("lat_auth", true);
const latWrite = new Trend("lat_write", true);
const latLarge = new Trend("lat_large", true);
const reqs = new Counter("reqs_measured");
const limited = new Counter("limited_429");

const SMALL = "x".repeat(1024);
const LARGE = "x".repeat(100 * 1024);

export const options = {
  discardResponseBodies: true,
  summaryTrendStats: ["avg", "med", "p(90)", "p(95)", "p(99)", "p(99.9)", "max"],
  scenarios: {
    mix: {
      executor: "constant-arrival-rate",
      rate: RATE, timeUnit: "1s", duration: `${WARMUP_S + DUR_S}s`,
      preAllocatedVUs: Number(__ENV.PREALLOC || Math.max(500, Math.ceil(RATE * 0.02))),
      maxVUs: Number(__ENV.MAXVUS || Math.max(2000, Math.ceil(RATE * 0.08))),
    },
  },
};

function record(trend, res, measuring) {
  if (!measuring) return;
  trend.add(res.timings.duration);
  reqs.add(1);
  if (res.status === 429) limited.add(1);
}

export default function () {
  const measuring = Date.now() - exec.scenario.startTime >= WARMUP_S * 1000;
  const r = Math.random();
  if (PROFILE === "read-only" || r < 0.6) {
    record(latRead, http.get(`${BASE}/baseline/api/item/42`), measuring);
  } else if (r < 0.85) {
    const tenant = `tenant-${Math.floor(Math.random() * TENANTS)}`;
    const res = http.get(`${BASE}/ratelimit/api/item/42`, {
      headers: { "x-plecto-ratelimit": tenant },
    });
    record(latAuth, res, measuring);
  } else if (r < 0.95) {
    record(latWrite, http.post(`${BASE}/body/api/item`, SMALL), measuring);
  } else {
    record(latLarge, http.post(`${BASE}/body/api/upload`, LARGE), measuring);
  }
}

export function handleSummary(data) {
  const g = (m) => (data.metrics[m] ? data.metrics[m].values : {});
  const rd = g("lat_read"), au = g("lat_auth"), wr = g("lat_write"), lg = g("lat_large");
  const n = data.metrics.reqs_measured ? data.metrics.reqs_measured.values.count : 0;
  const out = {
    profile: PROFILE,
    offered_rps: n / DUR_S,
    dropped: data.metrics.dropped_iterations ? data.metrics.dropped_iterations.values.count : 0,
    limited: data.metrics.limited_429 ? data.metrics.limited_429.values.count : 0,
    read_p50: rd.med || 0, read_p99: rd["p(99)"] || 0, read_p99_9: rd["p(99.9)"] || 0,
    auth_p50: au.med || 0, auth_p99: au["p(99)"] || 0,
    write_p50: wr.med || 0, write_p99: wr["p(99)"] || 0,
    large_p50: lg.med || 0, large_p99: lg["p(99)"] || 0,
  };
  const line = `\nweighted ${PROFILE} @ ${Math.round(out.offered_rps)} rps: ` +
    `read p99=${out.read_p99.toFixed(2)}ms  auth p99=${out.auth_p99.toFixed(2)}ms  ` +
    `write p99=${out.write_p99.toFixed(2)}ms  large p99=${out.large_p99.toFixed(2)}ms  ` +
    `dropped=${out.dropped} 429=${out.limited}\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
