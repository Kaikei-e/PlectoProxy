// Open-loop tail authority (Phase 2.2). constant-arrival-rate holds RATE req/s regardless of how
// slow responses get (coordinated-omission-safe), so queueing surfaces in the tail. Emits a JSON
// summary with p50/p95/p99/p99.9, dropped iterations, and failed rate.
import http from "k6/http";
import { check } from "k6";

const TARGET = __ENV.TARGET || "http://127.0.0.1:8080/";
const RATE = Number(__ENV.RATE || 11800);
const DUR = __ENV.DUR || "90s";
const OUT = __ENV.OUT || "openloop.json";

export const options = {
  discardResponseBodies: false,
  summaryTrendStats: ["avg", "min", "med", "p(95)", "p(99)", "p(99.9)", "max"],
  scenarios: {
    tail: {
      executor: "constant-arrival-rate",
      rate: RATE, timeUnit: "1s", duration: DUR,
      preAllocatedVUs: Math.max(2000, Math.ceil(RATE / 5)),
      maxVUs: Math.max(4000, RATE),
    },
  },
};

export default function () {
  const res = http.get(TARGET);
  check(res, { "status 200": (r) => r.status === 200 });
}

export function handleSummary(data) {
  const d = data.metrics.http_req_duration.values;
  const drop = data.metrics.dropped_iterations
    ? data.metrics.dropped_iterations.values.count : 0;
  const out = {
    target_rps: RATE,
    achieved_rps: data.metrics.http_reqs.values.rate,
    reqs: data.metrics.http_reqs.values.count,
    failed: data.metrics.http_req_failed.values.rate,
    dropped: drop,
    p50: d.med, p95: d["p(95)"], p99: d["p(99)"], p99_9: d["p(99.9)"],
  };
  const line = `\nopen-loop ${RATE}/s -> ${out.achieved_rps.toFixed(0)} rps  ` +
    `p50=${out.p50.toFixed(2)} p95=${out.p95.toFixed(2)} p99=${out.p99.toFixed(2)} ` +
    `p99.9=${out.p99_9.toFixed(2)}ms  dropped=${out.dropped} failed=${(out.failed * 100).toFixed(3)}%\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
