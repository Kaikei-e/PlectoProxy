# wasm-auth — a real WASM filter: API-key authentication

Plecto's thesis in one runnable file: the per-request *decision* — here, authentication —
is a **sandboxed `plecto:filter` component**, not native proxy code. The filter can touch
only the host-API it was lent (`host-kv`, `host-counter`, `host-log` — no network, no FS),
and returns a **typed decision**: `continue` / `modified` / `short-circuit`.

## Run

```bash
cargo run -p plecto-server --example wasm-auth
# or, visualized end to end:
./examples/try.sh wasm-auth
```

## Try it

Rejected — the filter short-circuits 401 and the upstream is **never reached**:

```bash
curl -i http://localhost:8084/api/data                       # no key
curl -i -H 'x-api-key: nope' http://localhost:8084/api/data  # unknown key
```

```text
HTTP/1.1 401 Unauthorized
```

Accepted — the filter stamps the caller's identity and continues:

```bash
curl -s -H 'x-api-key: alice-secret' http://localhost:8084/api/data
```

```text
hello alice — you reached the protected upstream
```

Anti-spoof — a client-supplied identity header is **overwritten**, not trusted:

```bash
curl -s -H 'x-api-key: alice-secret' -H 'x-authenticated-user: admin' \
     http://localhost:8084/api/data
```

```text
hello alice — you reached the protected upstream    (not admin)
```

## How it works

[`filters/filter-apikey`](../filters/filter-apikey) seeds a demo key→user map into
**host KV** at `init` (filters are stateless; state lives in the host, ADR 000011). On
each request it reads `x-api-key`, looks it up, and either short-circuits 401 or stamps
`x-authenticated-user` (set-replaces, so a spoofed value dies) and bumps a per-user
counter. The component is signed and loaded through the production verify-then-load path.

## Next

[`filter-chain`](../filter-chain) — how decisions compose along a chain, plus
host-native rate limiting.
