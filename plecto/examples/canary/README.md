# canary — weighted traffic split + header-match routing

Rolling out `checkout` v2, the way you actually do it (ADR 000034):

- one route splits public traffic **90/10** between `checkout-v1` and `checkout-v2`
  (`[[route.backends]]` weights);
- a second route sends anyone with `x-canary: always` **straight to v2** — a header
  match makes it more specific than the split (host > longest prefix > method > headers
  > query), so internal testers exercise the canary at will;
- when the rollout looks bad, edit the weight to `0` and SIGHUP: the canary is
  **drained instantly, zero downtime** — while the tester route still reaches v2.

## Run

```bash
cargo run -p plecto-server --example canary
# or, fully scripted (split tally, force-header, drain):
./examples/try.sh canary
```

The banner prints the proxy's **pid** and the on-disk **manifest path**.

## Try it

The split is deterministic error-diffusion apportionment, not randomness — 20 requests
land **exactly** 18/2, evenly interleaved (every 10th request is v2):

```bash
for i in $(seq 20); do curl -s http://localhost:8083/checkout; done | sort | uniq -c
```

```text
     18 checkout v1 handled /checkout
      2 checkout v2 handled /checkout
```

Force the canary as a tester:

```bash
curl -s -H 'x-canary: always' http://localhost:8083/checkout
# checkout v2 handled /checkout        (always)
```

Drain it — edit the manifest (`weight = 10` → `weight = 0`), then:

```bash
kill -HUP <pid>
# public traffic: 100% v1.  The tester route still hits v2 — debug the bad canary
# while users are safe.
```

Promote it instead: v1 weight → `0`, v2 weight → `100`, SIGHUP again.

## How it works

A weighted route compiles to a precomputed apportionment table walked by a lock-free
cursor — the split costs one atomic increment per request. `weight = 0` empties a
backend out of the *split*; explicit routes to the same upstream are untouched. The
split picks an upstream *group*; the group then picks a healthy instance with its own
load balancer ([`load-balancing`](../load-balancing) is that layer).

## Next

[`resilience`](../resilience) — when instances fail mid-flight: retry, timeouts,
circuit breaker, outlier detection.
