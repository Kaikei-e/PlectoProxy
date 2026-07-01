// A production-representative WEIGHTED traffic profile across several routes on one gateway, instead
// of hammering a single endpoint. Real API-gateway traffic is read-heavy with occasional writes and
// rare large payloads, so a single-endpoint test is synthetic: this mix exercises the router's match
// cost and a realistic blend of the no-filter and body-touching paths at once. Open-loop
// (constant-arrival-rate) so the tail is coordinated-omission-safe.
//
//   80% small read   GET  /baseline  (no filter, small response)
//   15% small write  POST /body      (~1 KB body -> body-hook filter)
//    5% large write  POST /body      (~100 KB body -> body-copy path)
import http from "k6/http";
import { Trend } from "k6/metrics";

const BASE = __ENV.BASE || "http://localhost:8086";
const RATE = Number(__ENV.RATE || 20000);
const DUR = __ENV.DUR || "60s";
const OUT = __ENV.OUT || "mix.json";

const latRead = new Trend("lat_read", true);
const latWrite = new Trend("lat_write", true);
const latLarge = new Trend("lat_large", true);

const SMALL = "x".repeat(1024);
const LARGE = "x".repeat(100 * 1024);

export const options = {
  summaryTrendStats: ["avg", "med", "p(90)", "p(95)", "p(99)", "p(99.9)", "max"],
  scenarios: {
    mix: {
      executor: "constant-arrival-rate",
      rate: RATE, timeUnit: "1s", duration: DUR,
      preAllocatedVUs: 400, maxVUs: 4000,
    },
  },
};

export default function () {
  const r = Math.random();
  if (r < 0.8) {
    latRead.add(http.get(`${BASE}/baseline/api/item/42`).timings.duration);
  } else if (r < 0.95) {
    latWrite.add(http.post(`${BASE}/body/api/item`, SMALL).timings.duration);
  } else {
    latLarge.add(http.post(`${BASE}/body/api/upload`, LARGE).timings.duration);
  }
}

export function handleSummary(data) {
  const g = (m) => (data.metrics[m] ? data.metrics[m].values : {});
  const rd = g("lat_read"), wr = g("lat_write"), lg = g("lat_large");
  const out = {
    offered_rps: data.metrics.http_reqs.values.rate,
    dropped: data.metrics.dropped_iterations ? data.metrics.dropped_iterations.values.count : 0,
    read_p50: rd.med || 0, read_p99: rd["p(99)"] || 0, read_p99_9: rd["p(99.9)"] || 0,
    write_p50: wr.med || 0, write_p99: wr["p(99)"] || 0,
    large_p50: lg.med || 0, large_p99: lg["p(99)"] || 0,
  };
  const line = `\nweighted mix @ ${Math.round(out.offered_rps)} rps: ` +
    `read p99=${out.read_p99.toFixed(2)}ms  write p99=${out.write_p99.toFixed(2)}ms  ` +
    `large p99=${out.large_p99.toFixed(2)}ms  dropped=${out.dropped}\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
