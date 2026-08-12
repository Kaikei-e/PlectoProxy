# filter-tokenlimit (MoonBit)

A rate limiter that meters **token cost**, not request count.

"One request" is the wrong unit for a generation endpoint: a twenty-token prompt and a
twenty-thousand-token prompt are one request each and nothing alike on the bill. This filter
reads the request body, prices it, and spends that price against the host's token bucket —
which the operator sizes in the manifest, so the filter can only spend from the budget, never
widen it (ADR 000026).

Contract: `plecto:filter@0.4.0`. Guest tier: **A** (zero WASI imports).

The same limiter exists in three languages. **Maintenance policy: this MoonBit guest is the
smallest Tier A build of the three** (~66 KB, against ~1.4 MB for TinyGo and ~12 MB for the
ComponentizeJS engine); the [JS guest](../filter-tokenlimit-js) is the canonical copy-target and
carries the complete **build → sign → serve → curl** walkthrough, which is not repeated here.
[Go/TinyGo](../filter-tokenlimit-go) is the Tier B (`fat-guest`) exemplar.

## What it exports, and what it deliberately does not

```
export init
export on-request
export on-request-body
```

…and **not** `on-response-body`. The host probes the optional body exports by name, so an
absent `on-response-body` is a positive statement: this filter never inspects a response body,
and the host keeps that direction streaming zero-copy past the chain (ADR 000038 / ADR 000098).
Pricing needs what the client *asked for*; buffering every generated answer to price it again
would be pure loss. `on-response` is exported too — it is header-only and stamps the cost
headers on the way out.

Imports are subset to the three capabilities the filter actually exercises — `host-config`,
`host-ratelimit`, `host-log`. No KV, no counters, no clock (deny-by-default, ADR 000006).

## Behaviour

`on-request` (headers only)

1. Read the header named by `key-header`. Missing, empty, or not valid UTF-8 → **401**
   `{"error":"missing api key"}` with `content-type: application/json`. A non-ASCII but
   well-formed UTF-8 key is a key like any other; only malformed bytes are refused, because the
   key crosses the boundary as a WIT `string`.
2. Otherwise stash the key and continue.

`on-request-body` (raw bytes only — headers are *not* available in this hook)

1. Parse the body as JSON. Failure → **400** `{"error":"invalid json body"}`.
2. Read `model` (string, default `""`), `max_tokens` (non-negative integer, default
   `max-tokens-default`), and the input text: `prompt` plus every `messages[i].content` that
   is a string. Non-string contents (tool calls, image parts) are ignored — they are not text.
   A `max_tokens` that is not a non-negative integer (negative, fractional, `NaN`, or past the
   exact-integer range of a JSON number) is treated as **absent**, not coerced; a genuine
   integer is clamped to `100000000`. A `model` longer than 128 bytes is never looked up.
3. Price it, in integer arithmetic:

   ```
   input_est = ceil(utf8_byte_len(text) / chars-per-token)
   base      = input_est + max_tokens
   percent   = model-cost-percent.<model>, or 100
   cost      = max(1, base * percent / 100)
   ```

4. `host-ratelimit.try-acquire(key, cost)`. Denied → **429**
   `{"error":"token budget exhausted"}` with `content-type: application/json` and
   `retry-after: <ceil(retry-after-ms / 1000)>`. Allowed → continue, unchanged
   (the bare `%continue` arm: this filter inspects the body, it never rewrites it, so the
   bytes are never copied back across the boundary).

`on-response`

* Adds `x-tokenlimit-cost` and `x-tokenlimit-remaining` when the request was priced;
  otherwise passes through unchanged.

`content-length` is never set by the filter — the host owns it and rejects a guest-supplied one
fail-closed (ADR 000071).

## The scratch rule

`on-request` sees the headers but not the body. `on-request-body` sees the body but not the
headers. The rate-limit key lives in a header and the price comes from the body, so the key has
to be carried between the two hooks, and the only carrier a component instance has is
instance-global state.

The rule that keeps that from becoming *cross-request* state:

> Every scratch field is written unconditionally at the **top** of `on-request`, before any
> early return, and none is ever read before that write within a request.

This matters most under `isolation = "trusted"`, where instances are pooled and reused: without
the unconditional reset, one caller's key could still be sitting in the scratch when the next
caller's body arrives, and the wrong bucket would be charged. The filter is stateless across
requests by construction, not by luck.

## Fail-closed choices

* **No key → 401**, not "continue unmetered". A request with no key has no bucket to spend
  from; letting it through would make it the only unlimited request on the route.
* **Bad `max-tokens-default` / `chars-per-token` → trap in `init`.** A mis-typed number would
  otherwise silently price every request wrong for as long as the filter runs. With
  `isolation = "trusted"` the operator sees this as a load failure instead of a production
  mystery (ADR 000066). `chars-per-token` must be ≥ 1 — a zero divisor is as broken as a
  non-number.
* **Empty `key-header` → trap in `init`.** An empty header name matches nothing, so the filter
  would answer every request `401` forever. That is a broken configuration, not a strict one.
* **Bad `model-cost-percent.<model>` → trap on the request that names that model.** The key
  only becomes reachable once a payload names the model, so `init` cannot validate it. Falling
  back to 100% would silently undercharge exactly the model the operator singled out as
  expensive.
* **A `max_tokens` that is not a non-negative integer is treated as absent**, never coerced —
  truncating `12.9` to 12 or clamping `-1` to 0 would make a malformed value cheaper than an
  honest one. A genuine integer **is clamped** before it reaches the arithmetic, and the percent
  multiply saturates instead of wrapping: an overflow that wrapped a huge cost back down to a
  small one would be a free request.
* **UTF-8, not UTF-16.** MoonBit strings are UTF-16; the cost formula is defined over UTF-8
  bytes, the unit the payload was measured in on the wire. Counting UTF-16 code units would
  sell a long CJK prompt at a fraction of its real size, so the length is computed by walking
  code points and summing their UTF-8 widths.

## Configuration

All keys are optional and read once in `init` (`host-config`, ADR 000066).

| key | default | meaning |
| --- | --- | --- |
| `key-header` | `x-api-key` | request header whose value is the rate-limit key |
| `max-tokens-default` | `1024` | output reservation assumed when `max_tokens` is absent |
| `chars-per-token` | `4` | UTF-8 input bytes per estimated token |
| `model-cost-percent.<model>` | `100` | integer percent multiplier for that model |

The bucket itself is not filter config — `[[filter]] ratelimit = { capacity, refill_tokens,
refill_interval_ms }` is operator-owned. [`manifest.toml`](manifest.toml) is a **working auth
chain**, not a single filter: `filter-apikey` runs first and stamps the authoritative
`x-authenticated-user`, and this filter keys on that stamp, so the budget follows the authenticated
identity rather than whichever credential presented it. The commented-out single-filter variant
(`key-header = "x-api-key"`) is labelled quick-trial-only on purpose — a client-supplied key header
is spoofable, and one caller can drain another's budget with it.

## Build

Requires the MoonBit toolchain (`moon`) and `wasm-tools` ≥ 1.252 on `PATH`.

```bash
./build.sh
```

Produces `dist/filter_tokenlimit_moonbit.wasm` and asserts the surface: zero `wasi:` imports,
the four expected exports, and no `on-response-body`.

The wit-bindgen bindings under `interface/`, `world/`, and `gen/` are **committed**, not
regenerated on every build — `gen/world/filter-request-body/` holds the hand-written
`tokenlimit.mbt` next to the generated stubs, and its `moon.pkg.json` carries host-API imports
that a regeneration would reset. `wit/deps/` is materialised from the canonical
`plecto/wit/world.wit` at build time so this guest's world uses the contract's own types
without a vendored copy that could drift; the canonical file is never edited (ADR 000063).

## Try it

The full walkthrough — sign the component, pin the digests, validate, serve, and the three curls
(`200` with the `x-tokenlimit-*` headers, `401`, `429`) — lives in the
[JS guest's README](../filter-tokenlimit-js/README.md#try-it-end-to-end) and applies verbatim: the
manifests here are the same shape and the same numbers, and every language answers identically by
construction (see the shared test battery below). Substitute this guest's artifact:

```bash
./build.sh
plecto package dist/filter_tokenlimit_moonbit.wasm --key signer.pem --out artifacts/tokenlimit
```

One worked number against [`manifest.toml`](manifest.toml)'s config, so the arithmetic is visible
here too:

```bash
curl -i http://localhost:8080/v1/chat/completions \
  -H 'x-api-key: alice-secret' \
  -H 'content-type: application/json' \
  -d '{"model":"chat-mini","max_tokens":256,"prompt":"summarise this paragraph"}'
```

```
HTTP/1.1 200 OK
x-tokenlimit-cost: 65
x-tokenlimit-remaining: 59935
```

The prompt is 24 UTF-8 bytes, so `input_est = ceil(24 / 4) = 6`; `base = 6 + 256 = 262`;
`chat-mini` is configured at 25%, so `cost = 262 * 25 / 100 = 65`, out of a 60000-token bucket.

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
`plecto/crates/host/tests/tokenlimit_battery/`, driven by `tests/tokenlimit.rs` (Tier A: this guest
+ JS) and `tests/tokenlimit_tier_b.rs` (Tier B: Go). Those are the golden vectors: the same request
body must produce the same cost, status, and headers in every language.

```bash
cargo test -p plecto-host --features polyglot-conformance --test tokenlimit
cargo test -p plecto-host --features polyglot-conformance,fat-guest --test tokenlimit_tier_b
```

## Status

This is a **starter**, not a shipped reference filter. Copy it, change the pricing to match your
own bill, and build it into your own signed artifact. The reference shelf is defined by signed
OCI artifacts with a digest, an SBOM, and a stated runtime profile — not by directories in this
repository (ADR 000080); nothing here is published on it.
