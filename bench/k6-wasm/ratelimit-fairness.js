// Rate-limit FAIRNESS across keys (ADR 000026). The host bucket is per-filter, per-key: each distinct
// `x-plecto-ratelimit` value gets its OWN independent token state. So a heavy key that exceeds the
// limit must be throttled to its own refill rate WITHOUT starving a light key sharing the same filter.
// Two concurrent open-loop streams — a HOT key offered far above the limit and a LIGHT key offered
// below it — prove the isolation: HOT is shed to ~refill rate (many 429), LIGHT passes cleanly (~0 429).
import http from "k6/http";
import { Counter } from "k6/metrics";

const BASE = __ENV.BASE || "http://localhost:8086";
const HOT_RATE = Number(__ENV.HOT_RATE || 4000);
const LIGHT_RATE = Number(__ENV.LIGHT_RATE || 500);
const DUR = __ENV.DUR || "30s";
const OUT = __ENV.OUT || "ratelimit_fairness.json";

const hotOk = new Counter("hot_ok");
const hot429 = new Counter("hot_429");
const lightOk = new Counter("light_ok");
const light429 = new Counter("light_429");
const hotNoStatus = new Counter("hot_no_status");
const lightNoStatus = new Counter("light_no_status");

// Both streams share one generator process and one loopback source address, so their VU pools are
// sized TOGETHER to stay under the fast path's per-source-IP connection cap (ADR 000092): past it,
// excess connections are refused at accept and the light key's requests vanish without a status,
// which reads as "the light key was starved" — the exact opposite of what the run proves. Little's
// law keeps these pools ample at loopback service times.
const HOT_POOL = Number(__ENV.HOT_PREALLOC || 150);
const LIGHT_POOL = Number(__ENV.LIGHT_PREALLOC || 50);

// No warmup exclusion, deliberately (see ratelimit-enforce.js): the hot key's initial burst is signal.
export const options = {
  discardResponseBodies: true,
  scenarios: {
    hot: {
      executor: "constant-arrival-rate",
      rate: HOT_RATE, timeUnit: "1s", duration: DUR,
      preAllocatedVUs: HOT_POOL, maxVUs: HOT_POOL,
      exec: "hot",
    },
    light: {
      executor: "constant-arrival-rate",
      rate: LIGHT_RATE, timeUnit: "1s", duration: DUR,
      preAllocatedVUs: LIGHT_POOL, maxVUs: LIGHT_POOL,
      exec: "light",
    },
  },
};

function hit(key, ok, ko, ns) {
  const res = http.get(`${BASE}/ratelimit/x`, { headers: { "x-plecto-ratelimit": key } });
  if (res.status === 200) ok.add(1);
  else if (res.status === 429) ko.add(1);
  // No status at all: a connection the proxy never accepted, which must never read as "shed".
  else ns.add(1);
}

export function hot() { hit("tenant-hot", hotOk, hot429, hotNoStatus); }
export function light() { hit("tenant-light", lightOk, light429, lightNoStatus); }

export function handleSummary(data) {
  const secs = (data.state.testRunDurationMs || 1) / 1000;
  const c = (m) => (data.metrics[m] ? data.metrics[m].values.count : 0);
  const out = {
    duration_s: secs,
    hot_offered_rps: HOT_RATE,
    light_offered_rps: LIGHT_RATE,
    hot_ok: c("hot_ok"), hot_429: c("hot_429"),
    light_ok: c("light_ok"), light_429: c("light_429"),
    hot_no_status: c("hot_no_status"), light_no_status: c("light_no_status"),
    hot_allowed_rps: c("hot_ok") / secs,
    light_allowed_rps: c("light_ok") / secs,
    light_429_frac: c("light_429") / Math.max(1, c("light_ok") + c("light_429")),
  };
  const line =
    `\nfairness: HOT ${HOT_RATE}/s -> allowed ${out.hot_allowed_rps.toFixed(0)}/s (${out.hot_429} x429)  |  ` +
    `LIGHT ${LIGHT_RATE}/s -> allowed ${out.light_allowed_rps.toFixed(0)}/s (${out.light_429} x429, ` +
    `${(out.light_429_frac * 100).toFixed(2)}% shed)  |  no-status ` +
    `${out.hot_no_status}/${out.light_no_status}\n`;
  return { [OUT]: JSON.stringify(out, null, 2), stdout: line };
}
