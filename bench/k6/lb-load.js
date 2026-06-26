// Scenario 1a — closed-loop throughput + latency.
// Ramps virtual users to find Plecto's achievable RPS and latency under concurrency.
// Each response's X-Instance header feeds a per-instance counter so the round-robin
// split is visible in Grafana (panel "served_by").
import http from 'k6/http';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const TARGET = __ENV.TARGET || 'http://localhost:8080/';
const servedBy = new Counter('served_by');

export const options = {
  discardResponseBodies: false,
  scenarios: {
    ramping: {
      executor: 'ramping-vus',
      startVUs: 0,
      stages: [
        { duration: '15s', target: 50 },
        { duration: '30s', target: 50 },
        { duration: '15s', target: 200 },
        { duration: '30s', target: 200 },
        { duration: '10s', target: 0 },
      ],
      gracefulStop: '5s',
    },
  },
  // Informational only — a failing threshold marks the run but never aborts it.
  thresholds: {
    http_req_failed: ['rate<0.01'],
    http_req_duration: ['p(95)<50', 'p(99)<200'],
  },
};

export default function () {
  const res = http.get(TARGET);
  check(res, { 'status 200': (r) => r.status === 200 });
  const inst = res.headers['X-Instance'];
  if (inst) servedBy.add(1, { instance: inst });
}
