# load-balancing — round-robin + active health checks

The native fast path, no filter involved: one upstream (`pool`) over **three instances**,
round-robin across the *healthy* set, a background prober ejecting/restoring instances,
and **fail-closed 503** when nothing healthy remains (ADR 000017).

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

## Next

- [`canary`](../canary) — the layer *above* this: weighted traffic split across upstreams.
- [`resilience`](../resilience) — what happens when instances misbehave on live traffic
  (retry, timeouts, circuit breaker, outlier detection).
- The `least_request` / `maglev` algorithms (ADR 000035) appear in
  [`production`](../production).
