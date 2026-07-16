# load-balancing — round-robin + active health checks + Maglev affinity

The native fast path, no filter involved: one upstream (`pool`) over **three instances**,
round-robin across the *healthy* set, a background prober ejecting/restoring instances,
and **fail-closed 503** when nothing healthy remains (ADR 000017). A second upstream
(`pool-sticky`, same instances) balances with **Maglev consistent hashing** (ADR 000035)
keyed on the `x-session` header, behind the `/sticky` route.

## Run

```bash
cargo run -p plecto-server --example load-balancing
# or, visualized with per-instance distribution bars:
./examples/try.sh load-balancing
```

## Try it

Round-robin — repeat and watch the instance cycle:

```bash
for i in $(seq 6); do curl -s http://localhost:8080/; done
```

```text
served by instance a
served by instance b
served by instance c
served by instance a
...
```

Eject and restore — each instance exposes `/toggle` to flip its own health; the prober
(500ms interval, threshold 2) reacts within ~1s:

```bash
curl -s http://<instance-b>/toggle   # b now fails its /healthz probe
# …a second later, traffic flows only to a and c
curl -s http://<instance-b>/toggle   # b recovers and rejoins
```

Total outage — toggle all three off:

```bash
curl -s -i http://localhost:8080/ | head -1
```

```text
HTTP/1.1 503 Service Unavailable          x-plecto-fault: no-healthy-upstream
```

Instances start **pessimistic** (unhealthy) and enter rotation only after a passing
probe — so a cold start never forwards into the void.

Session affinity — on `/sticky` the `x-session` header value is the Maglev hash key:
the same key lands on the same instance every time, a different key spreads independently,
and a request without the header falls back to round-robin:

```bash
for i in $(seq 4); do curl -s -H 'x-session: alice' http://localhost:8080/sticky; done
for i in $(seq 4); do curl -s -H 'x-session: bob'   http://localhost:8080/sticky; done
```

```text
served by instance c     # alice × 4 — always c
...
served by instance a     # bob × 4 — always a
```

Consistent remap — `/toggle` the instance serving `alice`: within ~1s only alice's keys
move to a surviving instance; `bob` keeps his. That difference from round-robin (where an
ejection reshuffles everyone) is the consistent-hashing claim, observable by hand.

## Next

- [`canary`](../canary) — the layer *above* this: weighted traffic split across upstreams.
- [`resilience`](../resilience) — what happens when instances misbehave on live traffic
  (retry, timeouts, circuit breaker, outlier detection).
- The `least_request` algorithm (ADR 000035) appears in [`production`](../production).
