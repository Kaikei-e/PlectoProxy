# filter-tokenlimit-go

A **token-cost rate limiter** for LLM-style JSON APIs, written in Go and compiled with TinyGo to a
`plecto:filter@0.4.0` component.

Requests-per-second is the wrong unit in front of a model-serving upstream: one request can be a
thousand times more expensive than the next. This filter prices each request from its own JSON
body — estimated input size plus the output the caller reserved, times a per-model multiplier — and
spends that price against the **host's** token bucket. The filter decides *what a request costs*;
the operator owns *how big the budget is* (`ratelimit` in the manifest), so a filter can never
widen its own limit.

> **A starter, not a shipped reference filter** ([ADR 000080](../../../../docs/ADR/000080.md)).
> Copy it, change the cost formula to whatever your upstream actually bills, and ship your own.

The same limiter exists in three languages. **Maintenance policy: this Go guest is the Tier B
exemplar** — the one that shows the `fat-guest` + `wasi = "minimal"` grant end to end. The
[JS guest](../filter-tokenlimit-js) is the canonical copy-target and carries the complete
**build → sign → serve → curl** walkthrough, which is not repeated here;
[MoonBit](../filter-tokenlimit-moonbit) is the smallest Tier A build.

## Tier B: this guest needs the WASI grant

TinyGo's `wasip2` target carries a language runtime that assumes a WASI baseline, which makes this
a **Tier B ("fat guest") filter** ([ADR 000063](../../../../docs/ADR/000063.md),
[writing-a-filter §7](../../../../docs/writing-a-filter.md#7-other-languages)). Two things must
line up or it will not instantiate — deny-by-default, both ends:

1. the host is built with the off-by-default `fat-guest` cargo feature, and
2. this filter's manifest entry declares `wasi = "minimal"`.

The grant is a fixed slice — `wasi:io` / `wasi:clocks` / `wasi:random` / `wasi:cli`, plus an empty
`wasi:filesystem` (zero preopens, so zero reachable paths) that TinyGo's bootstrap imports even
though this program touches no file. Never sockets, never HTTP. `build.sh` asserts exactly that
allowlist on every build.

`wasi:clocks` is a *runtime* detail here, not a time source: no decision in this filter reads a
clock. Anything that needs wall time must use the `host-clock` host-API, whose per-request snapshot
is identical across a retry.

## What it does

`init` — reads `[filter.config]`, traps on an unparseable number (with `isolation = "trusted"` the
host builds an instance eagerly at load, so a manifest typo fails the **load**, not the first
request), logs the effective config once at `info`.

`on-request` (headers only) — reads `key-header`. Missing, empty, or not valid UTF-8 →
**401** `{"error":"missing api key"}`. Otherwise the key goes into scratch and the request
continues.

`on-request-body` (raw bytes; headers are not available in this hook) — parses the JSON body,
prices it, and charges it:

| step | rule |
| --- | --- |
| invalid JSON | **400** `{"error":"invalid json body"}` |
| billable text | `prompt` (if a string) + every `messages[i].content` that is a string |
| `input_est` | `ceil(utf8_byte_len(text) / chars-per-token)` |
| `base` | `input_est + max_tokens` (body value, or `max-tokens-default` when absent) |
| `max_tokens` | only a non-negative integer counts; negative / fractional / `NaN` / past the exact-integer range of a JSON number are treated as **absent**, not coerced. A genuine integer is clamped to `100000000` |
| `percent` | `model-cost-percent.<model>` if configured, else `100` |
| `cost` | `max(1, base * percent / 100)` — integer math end to end |
| bucket denied | **429** `{"error":"token budget exhausted"}` + `retry-after` in **seconds**, rounded up |

Non-string `content` entries (tool calls, multimodal parts) are ignored rather than guessed at, and
a `model` name longer than 128 bytes is never turned into a config lookup. All arithmetic saturates
instead of wrapping: the numbers come from an untrusted body, and a wrapped cost would be a *small*
cost.

`on-response` — stamps `x-tokenlimit-cost` and `x-tokenlimit-remaining` when the request went
through the acquire path; otherwise it continues untouched.

`on-response-body` is **deliberately not exported**. The host reads that absence as "this filter
never inspects a response body" and keeps the response plane streaming zero-copy — which matters
most exactly here, in front of a streaming model response.

## Config (`[filter.config]`)

All keys are optional; a key that is present must parse.

| Key | Default | Meaning |
| --- | --- | --- |
| `key-header` | `x-api-key` | request header whose value is the rate-limit key |
| `max-tokens-default` | `1024` | assumed output reservation when `max_tokens` is absent |
| `chars-per-token` | `4` | UTF-8 bytes per estimated token (must be ≥ 1) |
| `model-cost-percent.<model>` | `100` | integer percent multiplier for that model name |

Quote the multiplier keys in TOML (`"model-cost-percent.chat-large" = "400"`) — the dot is part of
the key, not a table separator. A present-but-empty `key-header` traps in `init`: it could never
match a header, so the filter would answer `401` to everything.

An unparseable multiplier cannot be a load-time trap — it is only discoverable once a request names
that model — so it fails **that request** closed instead: the filter logs at `error` and traps.
Falling back to 100% would quietly undercharge exactly the model the operator singled out as
expensive, and a limiter that silently mis-charges is worse than one that stops.

[`manifest.toml`](manifest.toml) is a **working auth chain**, not a single filter: the
[`filter-apikey`](../filter-apikey) example runs first and stamps the authoritative
`x-authenticated-user` (overwriting whatever the client sent), and this filter keys on that stamp —
so the budget follows the account rather than the credential, and rotating a key does not reset it.

The commented-out single-filter variant (`key-header = "x-api-key"`) is labelled quick-trial-only
on purpose: a client-supplied key header is spoofable, so any caller can send another caller's value
and drain that caller's budget, or dodge their own.

## The scratch rule

The three hooks are three separate calls into the same instance, and WIT passes no context between
them, so the key and the cost live in an instance global between hooks. With
`isolation = "trusted"` the host **pools and reuses** instances, which makes a leftover value from
the previous request live data belonging to another caller. The rule that keeps this safe:

> `on-request` overwrites the **whole** scratch struct before reading anything from it, and no hook
> reads a field the same request has not written.

Resetting on entry — not on exit — means even a trapped or short-circuited request cannot leak into
the next one. That is what "filters are stateless" means in practice: durable state belongs to the
host (here, to the host-owned token bucket), never to a guest global.

## Build

```bash
export PATH="$HOME/.local/opt/tinygo-0.41.1/bin:$HOME/go/bin:$PATH"   # or wherever yours live
bash build.sh
```

Requires TinyGo ≥ 0.41, `wasm-tools` ≥ 1.252, `wit-bindgen-go`
(`go install go.bytecodealliance.org/cmd/wit-bindgen-go@v0.7.0`), and a Go toolchain. Output:
`dist/filter_tokenlimit_go.wasm` (~1.4 MB). The script generates the bindings, builds the
component, and then asserts the two things that are part of the contract: the Tier B WASI
allowlist, and the exact export set (four exports, `on-response-body` absent).

WIT deps are vendored under [`wit/`](wit/) and checked in, so the build fetches nothing.
`wit/deps/plecto-filter-0.4.0/package.wit` is a verbatim copy of the canonical
`plecto/wit/world.wit`, and `build.sh` fails if the two ever differ. The guest's own
[`wit/world.wit`](wit/world.wit) is where `include wasi:cli/imports@0.2.0` is composed onto the
plecto interfaces — that composition belongs to the guest and never to the canonical contract
([ADR 000063](../../../../docs/ADR/000063.md) Decision 3).

JSON is read with [gjson](https://github.com/tidwall/gjson), a scanning reader with no reflection
and no `encoding/json` dependency — a good fit for a guest where binary size and runtime surface
both matter. The body is validated before any field is read, because a scanning reader will happily
"find" fields in malformed input.

## Try it

The full walkthrough — sign the component, pin the digests, `plecto validate --resolve`, serve, and
the three curls (`200` with the `x-tokenlimit-*` headers, `401`, `429`) — lives in the
[JS guest's README](../filter-tokenlimit-js/README.md#try-it-end-to-end) and applies verbatim: same
manifest shape, same numbers, and every language answers identically by construction (see the
shared test battery below). Two Tier B deltas apply here:

```bash
export PATH="$HOME/.local/opt/tinygo-0.41.1/bin:$HOME/go/bin:$PATH"
bash build.sh
plecto package dist/filter_tokenlimit_go.wasm --key signer.pem --out artifacts/tokenlimit-go
```

1. the manifest entry keeps `wasi = "minimal"`, and
2. the gateway must be a `fat-guest` build — otherwise `plecto validate` rejects that line and the
   component would not instantiate anyway.

One worked number against [`manifest.toml`](manifest.toml)'s config (`chars-per-token = 4`,
`chat-mini` at 25%, a 60000-token bucket refilling at 1000/s), so the arithmetic is visible here
too. `alice-secret` is one of `filter-apikey`'s demo credentials; it authenticates as `alice`, and
`alice` is the budget being spent.

```bash
# priced and allowed — the response carries what it cost and what is left
curl -s -D - -o /dev/null http://localhost:8080/v1/chat/completions \
  -H 'x-api-key: alice-secret' -H 'content-type: application/json' \
  -d '{"model":"chat-mini","max_tokens":256,"messages":[{"role":"user","content":"hello"}]}'
# HTTP/1.1 200 OK
# x-tokenlimit-cost: 64             # ceil(5/4)=2 input + 256 reserved = 258, at 25% for chat-mini
# x-tokenlimit-remaining: 59936
```

A malformed body gets the third refusal shape:

```bash
curl -s -i http://localhost:8080/v1/chat/completions \
  -H 'x-api-key: alice-secret' -d 'not json'
# HTTP/1.1 400 Bad Request
# {"error":"invalid json body"}
```

`content-length` never appears in any of these — the host owns it and recomputes it from the bytes
it actually forwards.

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
MoonBit) and `tests/tokenlimit_tier_b.rs` (Tier B: this guest). Those are the golden vectors: the
same request body must produce the same cost, status, and headers in every language.

```bash
cargo test -p plecto-host --features polyglot-conformance --test tokenlimit
cargo test -p plecto-host --features polyglot-conformance,fat-guest --test tokenlimit_tier_b
```
