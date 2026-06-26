// Scenario 2 — resilience under steady load.
// Holds a fixed arrival rate for ~70s while the orchestrator drives instances unhealthy
// via their /toggle endpoints on a timeline. In Grafana you then see:
//   * served_by{instance=b} drop to ~0 within ~1s when b is ejected, traffic shifting to a+c,
//     and return ~1s after b recovers  (active health check, ADR 000017);
//   * a burst of http_req_failed (HTTP 503 "no-healthy-upstream") during the all-off window,
//     clearing on recovery  (fail-closed tenet).
// RATE is kept well below saturation so transitions, not load, dominate the picture.
import http from 'k6/http';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const TARGET = __ENV.TARGET || 'http://localhost:8080/';
const RATE = Number(__ENV.RATE || 3000);
const servedBy = new Counter('served_by');

export const options = {
  discardResponseBodies: false,
  scenarios: {
    steady: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: '70s',
      preAllocatedVUs: Math.max(200, Math.ceil(RATE / 20)),
      maxVUs: Math.max(1000, RATE),
    },
  },
  // No thresholds: 503s during the all-off window are expected and are the point.
};

export default function () {
  const res = http.get(TARGET);
  check(res, { 'status 200': (r) => r.status === 200 });
  const inst = res.headers['X-Instance'];
  if (inst) servedBy.add(1, { instance: inst });
}
