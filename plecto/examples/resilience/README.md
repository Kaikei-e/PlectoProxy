# resilience — retry, timeouts, circuit breaker, outlier detection

One upstream (`orders`) over three instances whose failure mode you flip **at runtime**
(`/mode/ok`, `/mode/slow`, `/mode/fail` on each instance). Four independent resilience
axes, each visible from curl:

| Axis | Knob | You see |
|------|------|---------|
| retry + per-try timeout (ADR 000023/000030) | `request_timeout_ms = 500`, `max_retries = 1` | a slow instance times out; the request is re-sent to a healthy one — still 200, ~0.5s late |
| overall timeout (ADR 000031) | `overall_timeout_ms = 800` | all-slow → 504, `x-plecto-fault: request-timeout` |
| circuit breaker (ADR 000028) | `max_requests = 2` | excess concurrent requests shed **instantly**: 503, `x-plecto-fault: circuit-open` |
| outlier detection (ADR 000032) | `consecutive_gateway_failures = 3` | a 503-ing instance is silently ejected — while clients keep seeing 200 |

## Run

```bash
cargo run -p plecto-server --example resilience
# or, all four scenarios scripted:
./examples/try.sh resilience
```

## Try it

Retry rescues a slow instance (per-try timeout 500ms, re-sent to a different instance —
GET is idempotent, so a timed-out attempt is safe to re-send):

```bash
curl -s http://<instance-a>/mode/slow
curl -s -w '  (%{time_total}s)\n' http://localhost:8087/orders
# instance b served /orders  (0.52s)     ← 200, one per-try timeout late
```

All instances slow — retrying can't help; the overall deadline fails the transaction:

```bash
curl -s -i http://localhost:8087/orders | grep -E 'HTTP/|x-plecto-fault'
# HTTP/1.1 504 Gateway Timeout
# x-plecto-fault: request-timeout
```

Circuit breaker — with the upstream saturated, extra concurrency sheds instead of queueing:

```bash
for i in 1 2 3 4; do curl -s -o /dev/null -w '%{http_code} %{time_total}s\n' \
    http://localhost:8087/orders & done; wait
# 503 0.01s   ← circuit-open, shed instantly
# 503 0.01s
# 504 0.80s   ← the two admitted requests ride to the overall deadline
# 504 0.80s
```

Outlier detection — the quiet one. Make `a` answer 503: **clients keep seeing 200**
(each 503 is retried around) while `a`'s failure streak builds; after 3 consecutive it
is ejected for 5s. Its `/stats` hit counter freezes — even though its `/healthz` stayed
green the whole time (outlier is a *different axis* than active health):

```bash
curl -s http://<instance-a>/mode/fail
for i in $(seq 9); do curl -s -o /dev/null -w '%{http_code} ' http://localhost:8087/orders; done
# 200 200 200 200 200 200 200 200 200
curl -s http://<instance-a>/stats    # note the count…
for i in $(seq 9); do curl -s -o /dev/null -w '%{http_code} ' http://localhost:8087/orders; done
curl -s http://<instance-a>/stats    # …frozen: a is out of rotation
```

## How it works

The three axes are deliberately separate: **health** asks "is the instance reachable?",
the **circuit breaker** "is the upstream saturated?", **outlier detection** "is the
instance misbehaving on live traffic?". Only real gateway-class responses (502/503/504)
feed the outlier streak — a breaker-shed 503 or a timeout never eject an instance. And
`max_ejection_percent` caps how much of the pool outlier ejection may remove, so
fail-closed never becomes a self-inflicted total outage.

## Next

[`production`](../production) — all of this composed in one real manifest, served by the
real `plecto` binary, with metrics to watch it happen.
