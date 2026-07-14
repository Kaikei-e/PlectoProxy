# production — the real binary + a manifest on disk

Every other demo wires the proxy in-process so one file tells one story. This one shows
**the shape you actually operate**: a deploy directory holding `manifest.toml`, a trust
root, and a signed, digest-pinned OCI layout of the auth filter — served by the real
`plecto` binary in a second terminal.

The manifest is a realistic composition rather than a single-feature demo: a signed WASM
auth filter, a **native rate-limit floor** (ADR 000033), `least_request` load balancing
(ADR 000035), a circuit breaker + outlier detection, and the `[observability]` admin
endpoint with a structured access log (ADR 000009).

## Run

Terminal 1 — prepare the deploy dir and keep the backend fleet alive:

```bash
cargo run -p plecto-server --example production
```

Terminal 2 — start the real gateway on it:

```bash
cargo run -q -p plecto -- target/production-demo/manifest.toml 127.0.0.1:8086
```

(Or scripted end to end: `./examples/try.sh production`.)

## Try it

Auth — the signed WASM filter gates `/api`:

```bash
curl -s -o /dev/null -w 'HTTP %{http_code}\n' http://127.0.0.1:8086/api/data
# HTTP 401
curl -s -H 'x-api-key: alice-secret' http://127.0.0.1:8086/api/data
# hello alice — served by api-2        (least_request spreads across api-1/2/3)
```

The native rate-limit floor — consulted *before* the chain, per client IP:

```bash
for i in $(seq 14); do curl -s -o /dev/null -w '%{http_code} ' \
    -H 'x-api-key: alice-secret' http://127.0.0.1:8086/api/data; done; echo
# whatever is left of the burst passes (200), then 429s with
# x-plecto-fault: rate-limited + retry-after — note the auth curls above already
# spent bucket tokens: the floor counts every request, even ones the filter 401s
```

The admin endpoint — never on the data-plane port:

```bash
curl -s http://127.0.0.1:9099/metrics | grep -E '^plecto_' | head
curl -s -o /dev/null -w 'readyz: HTTP %{http_code}\n' http://127.0.0.1:9099/readyz
```

Ops — the binary answers signals like any supervised process:

```bash
kill -HUP  <plecto pid>   # edit manifest.toml first → zero-downtime reload, fail-closed
kill -TERM <plecto pid>   # graceful shutdown: stop accepting, drain in-flight, exit 0
```

## About the signing

The example signs with a throwaway key so the run is self-contained. **Production signs
out of band** with `cosign sign-blob` and pins the digest in the manifest — the deploy
dir this example writes (`trust.pem`, `filters/apikey` layout, `manifest.toml`) is
exactly what that flow produces. See
[docs/writing-a-filter.md](../../../docs/writing-a-filter.md) §5.

## Files

- `target/production-demo/manifest.toml` — open it; every stanza is commented. It stays
  on disk after the demo for inspection (`cargo clean` reclaims it).
