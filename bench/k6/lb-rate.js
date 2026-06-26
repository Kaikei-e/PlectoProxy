// Scenario 1b — open-loop, fixed arrival rate -> honest tail latency.
// constant-arrival-rate keeps sending at RATE req/s regardless of how slow responses get,
// which is the coordinated-omission-safe way to read p99/p99.9. RATE is set by the
// orchestrator to ~70% of the closed-loop max found in scenario 1a.
import http from 'k6/http';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const TARGET = __ENV.TARGET || 'http://localhost:8080/';
const RATE = Number(__ENV.RATE || 5000);
const servedBy = new Counter('served_by');

export const options = {
  discardResponseBodies: false,
  scenarios: {
    constant_rate: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: '45s',
      preAllocatedVUs: Math.max(200, Math.ceil(RATE / 20)),
      maxVUs: Math.max(1000, RATE),
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.01'],
    http_req_duration: ['p(95)<100', 'p(99)<500', 'p(99.9)<2000'],
  },
};

export default function () {
  const res = http.get(TARGET);
  check(res, { 'status 200': (r) => r.status === 200 });
  const inst = res.headers['X-Instance'];
  if (inst) servedBy.add(1, { instance: inst });
}
