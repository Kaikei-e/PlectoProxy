# filter-jwt

Resource-Server JWT verification reference filter ([ADR 000070](../../../../docs/ADR/000070.md)).

Verifies `Authorization: Bearer` JWTs with **ES256** or **RS256** only (`none` / HS\* rejected).
Requires `isolation = "trusted"` so missing/invalid `[filter.config]` fails at **load**, not per request.

Built for `wasm32-wasip2` (JWKS path uses `wasi:http/outgoing-handler`). Crypto is
verify-only (`p256` / `rsa`) following RFC 7515 compact JWS.

## Config (`[filter.config]`)

| Key | Required | Meaning |
|-----|----------|---------|
| `issuer` | yes | Expected `iss` |
| `audience` | yes | Expected `aud` |
| `realm` | no | `WWW-Authenticate` realm (default `plecto`) |
| `public_key_pem` | XOR | SPKI public key PEM (static path) |
| `jwk` | XOR | Single JWK JSON object (static path) |
| `jwks_url` | XOR | JWKS URL fetched once in `init` |

Exactly one of `public_key_pem` / `jwk` / `jwks_url` must be set.

## Manifest requirements

Always declare `[filter.outbound_http]` (host feature `outbound-http`):

- **Static PEM/JWK:** `allow = []` is valid — links WASI HTTP with deny-all (guest imports the
  interface even when unused).
- **JWKS:** list the IdP host in `allow` (and `allow_private` only if you intentionally opt into
  private ranges; loopback stays blocked by the SSRF floor).

```toml
[[filter]]
id = "jwt"
source = "filters/jwt"
digest = "…"
isolation = "trusted"

[filter.config]
issuer = "https://idp.example.test"
audience = "plecto-api"
public_key_pem = """
-----BEGIN PUBLIC KEY-----
…
-----END PUBLIC KEY-----
"""

[filter.outbound_http]
allow = []
```

## Decisions

- Missing Bearer → `401` + `WWW-Authenticate: Bearer realm="…"` (no `error=`)
- Invalid / expired / bad alg → `401` + `error="invalid_token"`
- Success → `modified` stamps `x-authenticated-user` (`sub`) and `x-jwt-issuer` (`iss`)
