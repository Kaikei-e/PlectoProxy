# filter-chain — the typed decision, end to end

What a `plecto:filter` component does to a request as it passes through the chain: the
typed `decision` variant (`continue` / `modified` / `short-circuit`) plus the
**host-ratelimit capability** (a host-side token bucket the filter can only *consult*,
never configure — the operator owns the bucket, ADR 000026).

## Run

```bash
cargo run -p plecto-server --example filter-chain
# or, visualized end to end:
./examples/try.sh filter-chain
```

## Try it

The bundled `filter-hello` reacts to request headers:

```bash
curl -i http://localhost:8081/api/hello
# continue → forwarded (/api stripped); the response gains x-plecto-respadded

curl -s -H 'x-plecto-addheader: 1' http://localhost:8081/api/hello
# modified → the filter adds x-plecto-added: 1, the upstream echoes it back

curl -i -H 'x-plecto-block: 1' http://localhost:8081/api/hello
# short-circuit → HTTP/1.1 403; the upstream is never reached

for i in 1 2 3; do
  curl -s -o /dev/null -w '%{http_code}\n' -H 'x-plecto-ratelimit: 1' \
       http://localhost:8081/api/hello
done
# host token bucket (capacity 2) → 200, 200, 429
```

## How it works

The manifest pins one signed filter on a `/api` route with `strip_prefix = "/api"` (a
host-native rewrite — the filter still sees the original path). The filter's bucket is
declared **in the manifest**, not by the filter:

```toml
[[filter]]
id = "hello"
ratelimit = { capacity = 2, refill_tokens = 1, refill_interval_ms = 60000 }
```

Note this is the *per-filter capability* limiter. The **native route floor**
(`[route.rate_limit]`, no WASM involved) is a different mechanism — see the
[`production`](../production) example.

## Next

[`load-balancing`](../load-balancing) — the native fast path: instances, health, fail-closed.
