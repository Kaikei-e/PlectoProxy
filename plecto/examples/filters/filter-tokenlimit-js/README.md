# filter-tokenlimit-js

A **token-cost rate limiter** for LLM traffic, in JavaScript. Copy this directory and edit it —
it is a starter, not a shipped reference (see the note at the bottom).

Counting requests is the wrong unit in front of an LLM upstream: one request can cost a thousand
times another. This filter charges each request against a host-side token bucket by an estimate of
what it will cost — the input text plus the output the caller reserved — so big prompts drain a
caller's budget fast and small ones keep flowing.

The counting itself stays host-native (ADR 000005): the bucket's capacity and refill live in the
operator's manifest, and the filter only decides **which key to charge** and **how much**.

The same limiter exists in three languages — [MoonBit](../filter-tokenlimit-moonbit) and
[Go/TinyGo](../filter-tokenlimit-go) alongside this one. **Maintenance policy: this JS guest is the
canonical copy-target**, and the complete walkthrough below (build → sign → serve → curl) is the one
the other two link to; they document only their toolchain and tier deltas.

## What it does

| Hook | Behaviour |
| --- | --- |
| `init` | Reads `[filter.config]`, validates it, logs the effective settings once. Invalid value → traps (see below). |
| `on-request` | Key header missing, empty, or not valid UTF-8 → `401` `{"error":"missing api key"}`. Otherwise stashes the key and continues. |
| `on-request-body` | Prices the JSON body, spends that many tokens on the key. Unparseable body → `400` `{"error":"invalid json body"}`. Bucket empty → `429` `{"error":"token budget exhausted"}` + `Retry-After`. |
| `on-response` | Stamps `x-tokenlimit-cost` and `x-tokenlimit-remaining` on requests that were charged. |

### Cost

All integer arithmetic, no tokenizer — the exact number matters far less than charging in the
right unit, and a hot-path filter cannot afford a real tokenizer:

```
input_est = ceil(utf8_byte_len(prompt + every string messages[i].content) / chars-per-token)
base      = input_est + (max_tokens from the body, else max-tokens-default)
cost      = max(1, base * model-cost-percent.<model> / 100)     # floor division, default 100%
```

`max_tokens` is a **reservation**: the charge happens before the model runs, so what the caller
asks to generate is the only honest estimate of output. Only a **non-negative integer** counts —
negative, fractional (`12.9`), a string, `NaN`, or past the exact-integer range of a JSON number
all fall back to `max-tokens-default`, because coercing them (`-1` → 0, `12.9` → 12) would make a
malformed value *cheaper* than omitting the field. A genuine integer is clamped to `100000000`
rather than rejected, keeping the arithmetic away from the saturation point.

A `model` name longer than 128 bytes is never turned into a `model-cost-percent.<model>` lookup: it
is untrusted body content, and the 100% fallback is the expensive side. Non-string
`messages[i].content` (tool calls, multi-part arrays) is ignored rather than guessed at; extend
`promptText()` where your schema differs.

## Config (`[filter.config]`)

| Key | Default | Meaning |
| --- | --- | --- |
| `key-header` | `x-api-key` | Request header whose value is the rate-limit key |
| `max-tokens-default` | `1024` | Output reservation when the body omits `max_tokens` |
| `chars-per-token` | `4` | UTF-8 bytes per estimated token (must be ≥ 1) |
| `model-cost-percent.<model>` | `100` | Integer percent multiplier for one model name |

Every key is optional, and every value is parsed **strictly** (ASCII digits only — `"12abc"`,
`"0x10"` and `" 7 "` are errors, not surprises). An invalid value traps, and with
`isolation = "trusted"` that surfaces as a filter which refuses to **load** rather than one that
mis-charges quietly (ADR 000066). `chars-per-token = "0"` traps for the same reason a non-number
does: JavaScript would divide by it and yield `Infinity` instead of failing. A present-but-empty
`key-header` also traps — it could never match a header, so the filter would answer `401` to every
request. A `model-cost-percent.*` value can only be discovered when a request names that model, so
an invalid one fails that request closed with the same reasoning.

## Build

```bash
bash build.sh          # npm ci + componentize + zero-WASI assertion
```

Produces `dist/filter_tokenlimit_js.wasm` (~12 MB — the StarlingMonkey engine is a fixed cost,
independent of filter size). Requires node ≥ 20 and `wasm-tools` ≥ 1.252 on PATH.

`build.mjs` disables every WASI-backed engine feature (`random`, `stdio`, `clocks`, `http`,
`fetch-event`), so the component imports **only** the plecto host-API — the property the
deny-by-default Linker requires. `Date.now()` and `Math.random()` are therefore unavailable inside
the filter by design; nothing here needs them, because the bucket refills against the host's own
per-request clock snapshot.

### World

This guest declares its own world in `wit/world.wit`, in its own package. It sits between the two
published worlds on the acceptance lattice (ADR 000098): everything `filter` requires, plus
`on-request-body`, and deliberately **not** `on-response-body` — the absence is what tells the host
to keep streaming the response body zero-copy (ADR 000038) while buffering only the request body.
Its imports are subset the same way: `host-log`, `host-ratelimit`, `host-config` and nothing else.

`build.mjs` materialises `wit/deps/plecto-filter/` from the canonical `plecto/wit/world.wit` at
build time (git-ignored) rather than committing a second copy, so this guest cannot drift from the
contract it claims to implement. The canonical WIT is never edited to accommodate a guest
(ADR 000063).

## Manifest

See [`manifest.toml`](manifest.toml) — it is a **working auth chain**, not a single filter, because
keying a budget on a client-supplied header is the deployment mistake this filter invites:

```toml
[[filter]]                                # 1. establish WHO the caller is
id = "apikey"
source = "artifacts/apikey"
digest = "sha256:…"

[[filter]]                                # 2. then price and charge that identity
id = "tokenlimit"
source = "artifacts/tokenlimit-js"
digest = "sha256:…"
isolation = "trusted"
ratelimit = { capacity = 60000, refill_tokens = 1000, refill_interval_ms = 1000 }

[filter.config]
key-header = "x-authenticated-user"       # the stamp filter-apikey made, not a client header
"model-cost-percent.chat-large" = "500"   # quote dotted keys: this map is flat
"model-cost-percent.chat-mini" = "25"

[[route]]
filters = ["apikey", "tokenlimit"]        # order matters: the stamp must exist before it is read
```

`filter-apikey` maps a credential to a caller and stamps the authoritative `x-authenticated-user`
on the request it forwards, **overwriting** whatever the client sent — so the budget follows the
account, not the credential, and a caller cannot reset it by rotating a header.

The manifest keeps a commented-out single-filter variant (`key-header = "x-api-key"`) for a quick
trial. It is labelled as such deliberately: a client-supplied key header is spoofable, so any
caller can send another caller's value and drain that caller's budget, or dodge their own.

The `ratelimit` block is not optional in practice: a filter with no host-configured bucket has
every acquire denied (fail-closed, ADR 000026), so this filter would answer `429` to everything.

## Try it, end to end

Requires the `plecto` binary (`cargo install plecto`, or `cargo run -q -p plecto --`).

```bash
# 1. Build the component (npm ci + componentize + the zero-WASI assertion).
bash build.sh

# 2. Sign it and pin the digest. `plecto package` conformance-gates the component, signs it and
#    its SBOM, writes an offline OCI layout, and prints the pinned image-manifest digest. The key
#    is an ECDSA P-256 PKCS8 PEM and its public half goes in `[trust]` — see
#    docs/writing-a-filter.md §5 for the key material and the production (out-of-band) variant.
#    (`plecto dev <dir>`, the watch/rebuild/sign/reload loop, covers Rust filters today, so a JS
#    guest signs through `package`.)
plecto package dist/filter_tokenlimit_js.wasm --key signer.pem --out artifacts/tokenlimit-js

# ...and the identity filter the chain keys on. Any filter that stamps an identity header works;
# the bundled filter-apikey example is a Rust guest, so componentize it the Rust way (§3) first
# and then package it exactly the same:
plecto package my_auth_filter.component.wasm --key signer.pem --out artifacts/apikey

# 3. Pin both printed digests in manifest.toml, then prove the manifest + artifacts load
#    BEFORE serving anything (this runs the real provenance gate).
plecto validate --resolve manifest.toml

# 4. Serve it (your model upstream on 127.0.0.1:9000).
plecto manifest.toml 127.0.0.1:8080
```

Three requests, against the config above (`chars-per-token = 4`, `chat-mini` at 25%, `chat-large`
at 500%, a 60000-token bucket refilling at 1000/s). `alice-secret` is one of `filter-apikey`'s demo
credentials; it authenticates as `alice`, and `alice` is the budget being spent.

```bash
# 1. Charged and forwarded. "hello" is 5 bytes -> ceil(5/4) = 2, + 256 reserved = 258,
#    x25% for chat-mini = 64 tokens out of the 60000-token bucket.
curl -si http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' -H 'x-api-key: alice-secret' \
  -d '{"model":"chat-mini","max_tokens":256,"messages":[{"role":"user","content":"hello"}]}' \
  | grep -i '^HTTP/\|^x-tokenlimit-'
# HTTP/1.1 200 OK
# x-tokenlimit-cost: 64
# x-tokenlimit-remaining: 59936

# 2. No credential — refused before anything is priced.
curl -si http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' -d '{"prompt":"hello"}'
# HTTP/1.1 401 Unauthorized

# 3. Budget exhausted. Each of these reserves 10000 output tokens at 500%:
#    ceil(9/4) = 3 input + 10000 = 10003, x500% = 50015. The first is charged; the second
#    cannot be paid for out of the 9985 left.
curl -si http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' -H 'x-api-key: alice-secret' \
  -d '{"model":"chat-large","max_tokens":10000,"prompt":"summarise"}'   # 200, 9985 left
curl -si http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' -H 'x-api-key: alice-secret' \
  -d '{"model":"chat-large","max_tokens":10000,"prompt":"summarise"}'
# HTTP/1.1 429 Too Many Requests
# content-type: application/json
# retry-after: 41
# {"error":"token budget exhausted"}
```

In the chained manifest the `401` comes from the **auth** filter — an unauthenticated request never
reaches the limiter at all. This filter's own `401` (`{"error":"missing api key"}`, the shape in the
hook table above) is the fail-closed backstop for a chain where the stamp is missing, and is what
you see directly with the single-filter quick-trial variant. A body that is not JSON gets the third
refusal shape, `400 {"error":"invalid json body"}`.

`Retry-After` is in delay-seconds (RFC 9110), rounded **up** from the host's milliseconds — a
client that waited the rounded-down value would just be denied again. The exact number drifts down
as the bucket refills between the two calls.

## Expectations

Read these before putting this in front of real traffic. They are the boundaries of what an
estimate-and-admit limiter can honestly claim, and they are identical for all three language
implementations.

- **Estimate-and-admit, never reconcile.** The price is charged *before* the upstream runs, from
  what the caller asked for. The `plecto:filter` `host-ratelimit` surface offers `try-acquire` and
  nothing else, so an over-estimate is **never refunded**: a caller that reserves a large
  `max_tokens` and then generates three tokens has still spent the reservation, and can starve its
  own bucket until it refills. Reconciling against actual usage is a **non-goal** of this filter,
  not a missing feature.
- **`chars-per-token` is a hot-path heuristic, not billing accuracy.** It is a byte count divided by
  a constant; no tokenizer runs on the request path. Never present `x-tokenlimit-cost` as a billing
  source of truth — it is an admission-control number.
- **This is a gateway-side budget reservation, not an imitation of any upstream's own limiter.**
  Upstreams differ on whether a requested output reservation counts toward admission at all. Treat
  this as your budget, in your unit, enforced at your edge.
- **What the estimator does not count is a documented undercharge.** Tool/function definitions,
  non-string or array message content, and system prompts carried outside the fields this filter
  reads are all billable input priced at zero here. A production fork should either count them or
  reject bodies that use them.

## The cross-language behaviour is tested, not asserted

The three implementations are held to **one shared assertion battery** in the host test suite —
`plecto/crates/host/tests/tokenlimit_battery/`, driven by `tests/tokenlimit.rs` (Tier A: JS +
MoonBit) and `tests/tokenlimit_tier_b.rs` (Tier B: Go). Those are the golden vectors: the same
request body must produce the same cost, status, and headers in every language.

```bash
cargo test -p plecto-host --features polyglot-conformance --test tokenlimit
cargo test -p plecto-host --features polyglot-conformance,fat-guest --test tokenlimit_tier_b
```

## The scratch rule

The key is read from a header in `on-request`, but the cost can only be computed in
`on-request-body`, and **headers are not available in that hook**. The two hooks are bridged by one
module-level variable:

```js
let scratch = { key: null, cost: null, remaining: null };
```

The rule that makes this safe: **`on-request` overwrites all of `scratch` on every path, before
anything reads it.** Never read a field that the current request has not written. A `trusted`
filter runs on pooled instances, so a scratch left over from an earlier request would otherwise be
visible to the next one — the overwrite is what keeps the filter effectively stateless across
requests (Tenet: filters are stateless; durable state belongs in host KV).

The same rule reads backwards on the response side: `on-response` stamps headers only when
`scratch.cost` is non-null, so a request that was answered `401` or `400` — and therefore never
charged — reports nothing.

## Not a shipped reference

This is a copy-paste starter, not part of the signed reference shelf: it ships no OCI artifact, no
signature, no SBOM and no compatibility guarantee (ADR
[000080](../../../../docs/ADR/000080.md)). Read it, take it, change it — the estimate, the JSON
shape, and the error bodies are all yours to own.
