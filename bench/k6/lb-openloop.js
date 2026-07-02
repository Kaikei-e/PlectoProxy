// Open-loop tail authority (Phase 2.2). constant-arrival-rate holds RATE req/s regardless of how
// slow responses get (coordinated-omission-safe), so queueing surfaces in the tail. Emits a JSON
// summary with p50/p95/p99/p99.9, dropped iterations, and failed rate.
//
// VU allocation follows Little's law (needed VUs ≈ rate × in-flight latency): on loopback that is
// tens of VUs, so preAllocatedVUs defaults to ~2% of RATE and maxVUs is capped at 4× that. When
// the SUT (or the generator) can't keep the schedule, the overflow shows up as dropped_iterations
// — an honest open-loop shed signal — instead of ballooning VU counts melting the generator.
// The first WARMUP_S seconds send load but are not recorded; DUR is the measured window.
import http from "k6/http";
import exec from "k6/execution";
import { Counter, Trend } from "k6/metrics";

const TARGET = __ENV.TARGET || "http://127.0.0.1:8080/";
const RATE = Number(__ENV.RATE || 11800);
const DUR_S = parseInt(__ENV.DUR || "90", 10);
const WARMUP_S = Number(__ENV.WARMUP_S || 5);
const OUT = __ENV.OUT || "openloop.json";
const PREALLOC = Number(__ENV.PREALLOC || Math.max(500, Math.ceil(RATE * 0.02)));
const MAXVUS = Number(__ENV.MAXVUS || PREALLOC * 4);

const lat = new Trend("lat", true);
const reqs = new Counter("reqs_measured");
const fails = new Counter("fails_measured");

export const options = {
  discardResponseBodies: true,
  summaryTrendStats: ["avg", "min", "med", "p(95)", "p(99)", "p(99.9)", "max"],
  scenarios: {
    tail: {
      executor: "constant-arrival-rate",
      rate: RATE, timeUnit: "1s", duration: `${WARMUP_S + DUR_S}s`,
      preAllocatedVUs: PREALLOC,
      maxVUs: MAXVUS,
    },
  },
};

export default function () {
  const res = http.get(TARGET);
  if (Date.now() - exec.scenario.startTime < WARMUP_S * 1000) return; // warmup: send, don't record
  lat.add(res.timings.duration);
  reqs.add(1);
  if (res.status !== 200) fails.add(1);
}

export function handleSummary(data) {
  const d = data.metrics.lat.values;
  const n = data.metrics.reqs_measured ? data.metrics.reqs_measured.values.count : 0;
  const bad = data.metrics.fails_measured ? data.metrics.fails_measured.values.count : 0;
  // dropped_iterations spans the whole run (warmup included) — k6 counters can't be time-sliced.
  const drop = data.metrics.dropped_iterations
    ? data.metrics.dropped_iterations.values.count : 0;
  const out = {
    target_rps: RATE,
    achieved_rps: n / DUR_S,
    reqs: n,
    failed: bad / Math.max(1, n),
    dropped: drop,
    p50: d.med, p95: d["p(95)"], p99: d["p(99)"], p99_9: d["p(99.9)"],
  };
  const line = `\nopen-loop ${RATE}/s -> ${out.achieved_rps.toFixed(0)} rps  ` +
    `p50=${out.p50.toFixed(2)} p95=${out.p95.toFixed(2)} p99=${out.p99.toFixed(2)} ` +
    `p99.9=${out.p99_9.toFixed(2)}ms  dropped=${out.dropped} failed=${(out.failed * 100).toFixed(3)}%\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
