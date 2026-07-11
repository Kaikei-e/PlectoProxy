# filter-cors

CORS reference filter (F2 shelf / ADR 000073) — the living proof that `on-response` can read the
**as-forwarded** request snapshot (dynamic `Origin` echo) without guest globals or a host-API
query.

## Policy (`[filter.config]`)

| Key | Meaning |
| --- | --- |
| `allowed-origins` | Comma-separated allowlist. Exact match, or `*` when credentials are off. |
| `allow-methods` | Preflight `Access-Control-Allow-Methods` (default `GET, POST, OPTIONS`). |
| `allow-headers` | Preflight `Access-Control-Allow-Headers` (default: echo the request's ACRH). |
| `allow-credentials` | `"true"` adds `Access-Control-Allow-Credentials`. |
| `max-age` | Preflight `Access-Control-Max-Age` seconds. |

### Do not combine `allowed-origins = "*"` with `allow-credentials = "true"`

That pair would otherwise echo **every** `Origin` under credentialed CORS — a common operator
footgun. This filter **ignores** the `*` entry when credentials are enabled; list concrete origins
instead. A misconfigured pair grants nothing (fail-safe), not a wildcard credentialed echo.

## Chain order

Response filters run in reverse request order. A later filter's `replace` skips earlier ones on
the response path — see [Writing a filter](../../../../docs/writing-a-filter.md#response-side-decisions-and-chain-order).
