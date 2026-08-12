//! The shared assertion battery for the `filter-tokenlimit-*` example guests.
//!
//! `filter-tokenlimit` is a token-COST rate limiter: it prices each request from its JSON body and
//! spends that price against the host-owned bucket. Three guests (JS, MoonBit, Go/TinyGo) implement
//! the SAME behaviour spec, so the falsifiable form of "the spec is the contract, not the
//! implementation" is one battery driven identically from both tiers' test binaries — Tier A
//! (`tests/tokenlimit.rs`) and Tier B (`tests/tokenlimit_tier_b.rs`, ADR 000063). The tiers stay in
//! separate files for the reason ADR 000055 gives: Tier A's `LoadOptions` must keep proving those
//! guests are zero-WASI, which folding in a guest that needs `with_wasi_minimal()` would dilute.
//! What differs between the two is exactly one function — the [`Grant`] each tier applies to the
//! otherwise identical options — so everything below is shared verbatim.
//!
//! These are also the first `plecto:filter@0.4.0` EXAMPLE guests, so the battery pins the 0.4.0
//! rail itself: the bound contract version, the request-body-only hook set (the absence of
//! `on-response-body` is contractual, ADR 000038 / 000098), and the bare `%continue` body arm.

use std::collections::BTreeMap;
use std::path::PathBuf;

use plecto_host::test_support::{TestSigner, bound_sbom};
use plecto_host::{
    BodyHooks, ContractVersion, Header, Host, HttpRequest, HttpResponse, LoadOptions, LoadedFilter,
    RequestBodyDecision, RequestDecision, RequestTrace, ResponseDecision, RunError, SignedArtifact,
};

// --- the operator-owned side of the spec ------------------------------------------------------
//
// Every number the guests charge with comes from HERE, not from guest defaults: the bucket spec is
// host-side (`LoadOptions`, the manifest's `[[filter]] ratelimit` in production, ADR 000026) and
// the business config is the manifest's `[filter.config]` (ADR 000066). Pinning both makes every
// expected cost below computable in the test from the spec's own formula.

/// UTF-8 bytes of input per estimated token (`chars-per-token`).
const CHARS_PER_TOKEN: u64 = 4;
/// Output reservation assumed when a body omits `max_tokens` (`max-tokens-default`).
const MAX_TOKENS_DEFAULT: u64 = 1024;
/// A model the config prices at [`PREMIUM_PERCENT`]; anything else is charged face value.
const PREMIUM_MODEL: &str = "premium-model";
/// The multiplier configured for [`PREMIUM_MODEL`], in percent.
const PREMIUM_PERCENT: u64 = 500;
/// A model name with no `model-cost-percent.*` entry — charged at 100%.
const PLAIN_MODEL: &str = "plain-model";

/// Refill rate of the test bucket. One token per minute is "no refill" for a test's wall time,
/// which is what makes both the remaining counts AND the `retry-after` hint exactly predictable —
/// while still exercising the real refill arithmetic rather than the zero-refill special case.
const REFILL_TOKENS: u64 = 1;
const REFILL_INTERVAL_MS: u64 = 60_000;

/// The rate-limit key the charged requests spend against.
const API_KEY: &str = "tenant-under-test";

/// A guest component under test: the file stem (for assertion messages) and its bytes.
pub struct Guest {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// What a tier must add to the shared `LoadOptions` — nothing for Tier A, the `wasi = "minimal"`
/// grant for Tier B. Keeping it a plain `fn` pointer means the battery needs no generics and no
/// `fat-guest`-conditional code of its own.
pub type Grant = fn(LoadOptions) -> LoadOptions;

/// Resolve the components under test: `env_var` (a `:`-separated path list) when set, else the
/// example guests' `dist/` output.
pub fn guests(env_var: &str, defaults: &[&str], build_hint: &str) -> Vec<Guest> {
    let paths: Vec<PathBuf> = match std::env::var(env_var) {
        Ok(list) => list.split(':').map(PathBuf::from).collect(),
        Err(_) => defaults
            .iter()
            .map(|rel| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel))
            .collect(),
    };
    paths
        .into_iter()
        .map(|p| {
            let name = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("component")
                .to_string();
            let bytes = std::fs::read(&p).unwrap_or_else(|e| {
                panic!(
                    "read tokenlimit component {}: {e}\n\
                     build it first: {build_hint} \
                     (or point {env_var} at prebuilt components)",
                    p.display()
                )
            });
            Guest { name, bytes }
        })
        .collect()
}

/// The `[filter.config]` every assertion loads with, unless it deliberately breaks one key.
pub fn base_config() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("key-header".to_string(), "x-api-key".to_string()),
        (
            "max-tokens-default".to_string(),
            MAX_TOKENS_DEFAULT.to_string(),
        ),
        ("chars-per-token".to_string(), CHARS_PER_TOKEN.to_string()),
        (
            format!("model-cost-percent.{PREMIUM_MODEL}"),
            PREMIUM_PERCENT.to_string(),
        ),
    ])
}

/// The shared load options: `trusted` (pooled), because the filter carries the rate-limit key from
/// `on-request` to `on-request-body` in an instance global — under `untrusted` every hook call gets
/// a FRESH instance and the scratch could never survive the trip. Pool size 1 pins which instance a
/// serial test lands on, so the pooling-safety assertion below is about the guest's scratch
/// discipline and not about which slot the host happened to hand out.
pub fn options(grant: Grant, capacity: u64, config: BTreeMap<String, String>) -> LoadOptions {
    grant(
        LoadOptions::trusted()
            .with_trusted_pool_size(1)
            .with_ratelimit_bucket(capacity, REFILL_TOKENS, REFILL_INTERVAL_MS)
            .with_config(config),
    )
}

/// Sign with a fresh ephemeral key and load through the REAL provenance path (ADR 000006) — an
/// example guest gets no special treatment at the gate.
pub fn try_load(guest: &Guest, opts: LoadOptions) -> anyhow::Result<(Host, LoadedFilter)> {
    let signer = TestSigner::new()?;
    let component_signature = signer.sign(&guest.bytes)?;
    let sbom = bound_sbom(&guest.bytes);
    let sbom_signature = signer.sign(&sbom)?;
    let host = Host::new(signer.trust_policy()?)?;
    let artifact = SignedArtifact {
        component_bytes: &guest.bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    host.load(&guest.name, &artifact, opts)
        .map(|filter| (host, filter))
}

fn load(guest: &Guest, opts: LoadOptions) -> (Host, LoadedFilter) {
    try_load(guest, opts)
        .unwrap_or_else(|e| panic!("{} must satisfy plecto:filter@0.4.0: {e}", guest.name))
}

fn request(headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: "POST".to_string(),
        path_with_query: "/v1/chat/completions".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: headers
            .iter()
            .map(|(n, v)| Header {
                name: (*n).to_string(),
                value: v.as_bytes().to_vec(),
            })
            .collect(),
    }
}

fn keyed() -> HttpRequest {
    request(&[("x-api-key", API_KEY)])
}

/// A request whose key header carries arbitrary BYTES — header values are `list<u8>` on the
/// contract (ADR 000071), so a client can send something that is not text at all.
fn keyed_raw(value: &[u8]) -> HttpRequest {
    HttpRequest {
        headers: vec![Header {
            name: "x-api-key".to_string(),
            value: value.to_vec(),
        }],
        ..request(&[])
    }
}

fn upstream_200() -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![],
        body: vec![],
    }
}

/// A chat-completion payload whose only billable text is `content`.
fn chat_body(model: &str, max_tokens: u64, content: &str) -> Vec<u8> {
    raw_max_tokens_body(model, &max_tokens.to_string(), content)
}

/// Like [`chat_body`], but `max_tokens` is spliced in as a RAW JSON token, so a test can send
/// `-1`, `12.9`, `"256"` or anything else a client might actually put there.
fn raw_max_tokens_body(model: &str, max_tokens: &str, content: &str) -> Vec<u8> {
    format!(
        r#"{{"model":"{model}","max_tokens":{max_tokens},"messages":[{{"role":"user","content":"{content}"}}]}}"#
    )
    .into_bytes()
}

/// The same payload with no `max_tokens` member at all — the baseline the malformed values must
/// price identically to.
fn body_without_max_tokens(model: &str, content: &str) -> Vec<u8> {
    format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"{content}"}}]}}"#)
        .into_bytes()
}

/// The spec's cost formula, recomputed here rather than read back from the guest — that is the
/// point of the assertion. All integer math: `ceil(bytes / chars-per-token) + max_tokens`, scaled
/// by the model percent (floor division), floored at 1.
fn expected_cost(text: &str, max_tokens: u64, percent: u64) -> u64 {
    let input_est = (text.len() as u64).div_ceil(CHARS_PER_TOKEN);
    let base = input_est + max_tokens;
    (base * percent / 100).max(1)
}

fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
}

fn short_circuit(name: &str, what: &str, decision: RequestDecision) -> HttpResponse {
    match decision {
        RequestDecision::ShortCircuit(resp) => resp,
        other => panic!("{name}: {what} must short-circuit, got {other:?}"),
    }
}

fn body_short_circuit(name: &str, what: &str, decision: RequestBodyDecision) -> HttpResponse {
    match decision {
        RequestBodyDecision::ShortCircuit(resp) => resp,
        other => panic!("{name}: {what} must short-circuit, got {other:?}"),
    }
}

fn assert_json_error(name: &str, resp: &HttpResponse, status: u16, body: &str) {
    assert_eq!(resp.status, status, "{name}: unexpected status");
    assert_eq!(
        String::from_utf8_lossy(&resp.body),
        body,
        "{name}: the error body is part of the spec, byte for byte"
    );
    assert_eq!(
        header_value(&resp.headers, "content-type"),
        Some("application/json"),
        "{name}: a JSON error body must say so"
    );
    assert!(
        header_value(&resp.headers, "content-length").is_none(),
        "{name}: content-length is host-owned; a guest-supplied one is rejected fail-closed"
    );
}

/// Drive one priced request end to end and return the `(cost, remaining)` the response hook
/// reported. Panics unless the body hook took the BARE `%continue` arm — a filter that only
/// INSPECTS the body must never hand the bytes back (ADR 000098).
fn charge(name: &str, filter: &LoadedFilter, body: &[u8]) -> (u64, u64) {
    charge_as(name, filter, &keyed(), body)
}

fn charge_as(name: &str, filter: &LoadedFilter, req: &HttpRequest, body: &[u8]) -> (u64, u64) {
    let (decision, _logs) = filter.on_request(req, &RequestTrace::root()).unwrap();
    assert!(
        matches!(decision, RequestDecision::Continue),
        "{name}: a keyed request continues to the body hook"
    );

    let (decision, _logs) = filter
        .on_request_body(body, &RequestTrace::root())
        .unwrap_or_else(|e| panic!("{name}: on-request-body failed: {e}"));
    assert!(
        matches!(decision, RequestBodyDecision::Continue),
        "{name}: an inspecting filter takes the bare %continue arm, leaving the buffered body \
         untouched — got {decision:?}"
    );

    let (decision, _logs) = filter
        .on_response(req, &upstream_200(), &RequestTrace::root())
        .unwrap();
    match decision {
        ResponseDecision::Modified(edit) => {
            assert!(
                edit.set_status.is_none(),
                "{name}: reporting the cost must not rewrite the upstream status"
            );
            assert!(
                edit.remove_headers.is_empty(),
                "{name}: reporting the cost must not remove upstream headers"
            );
            let cost = header_value(&edit.set_headers, "x-tokenlimit-cost")
                .unwrap_or_else(|| {
                    panic!("{name}: a charged request must report x-tokenlimit-cost")
                })
                .parse()
                .unwrap_or_else(|e| panic!("{name}: x-tokenlimit-cost must be a decimal: {e}"));
            let remaining = header_value(&edit.set_headers, "x-tokenlimit-remaining")
                .unwrap_or_else(|| {
                    panic!("{name}: a charged request must report x-tokenlimit-remaining")
                })
                .parse()
                .unwrap_or_else(|e| {
                    panic!("{name}: x-tokenlimit-remaining must be a decimal: {e}")
                });
            (cost, remaining)
        }
        other => panic!("{name}: a charged request must report its cost, got {other:?}"),
    }
}

// --- the battery ------------------------------------------------------------------------------

/// These are the first `plecto:filter@0.4.0` example guests: assert they bind the 0.4.0 rail
/// natively, and that the hook set is request-body-only. The ABSENCE of `on-response-body` is the
/// load-bearing half — the host reads it as "never buffer the response body" and keeps that
/// direction streaming zero-copy (ADR 000038 / 000098), which is exactly what a limiter in front of
/// a streaming upstream needs.
pub fn binds_the_040_rail_with_the_request_body_hook_only(guest: &Guest, grant: Grant) {
    let (_host, filter) = load(guest, options(grant, 1_000, base_config()));
    assert_eq!(
        filter.contract_version(),
        ContractVersion::V04,
        "{}: built against the current contract, so it binds the 0.4.0 rail",
        guest.name
    );
    assert_eq!(
        filter.body_hooks(),
        BodyHooks {
            request: true,
            response: false,
        },
        "{}: prices what the client asked for and never reads the answer back",
        guest.name
    );
}

/// Assertion 1: no key header at all → 401, before the body is ever buffered.
pub fn a_request_without_the_key_header_is_401(guest: &Guest, grant: Grant) {
    let (_host, filter) = load(guest, options(grant, 1_000, base_config()));
    let (decision, _logs) = filter
        .on_request(&request(&[]), &RequestTrace::root())
        .unwrap();
    let resp = short_circuit(&guest.name, "an unkeyed request", decision);
    assert_json_error(&guest.name, &resp, 401, r#"{"error":"missing api key"}"#);
}

/// Assertion 2: a keyed request whose body is not JSON → 400. A body the filter cannot read is a
/// body it cannot price, so it is refused rather than forwarded unmetered.
pub fn a_body_that_is_not_json_is_400(guest: &Guest, grant: Grant) {
    let (_host, filter) = load(guest, options(grant, 1_000, base_config()));
    let (decision, _logs) = filter.on_request(&keyed(), &RequestTrace::root()).unwrap();
    assert!(
        matches!(decision, RequestDecision::Continue),
        "{}: a keyed request reaches the body hook",
        guest.name
    );

    let (decision, _logs) = filter
        .on_request_body(b"{not json at all", &RequestTrace::root())
        .unwrap();
    let resp = body_short_circuit(&guest.name, "an unparseable body", decision);
    assert_json_error(&guest.name, &resp, 400, r#"{"error":"invalid json body"}"#);
}

/// Assertion 3 (+ 8): a request within budget is charged the EXACT cost the spec's formula gives,
/// the body reaches the upstream unmodified (the bare `%continue` arm, asserted inside `charge`),
/// and the response carries both accounting headers.
pub fn a_request_within_budget_is_charged_and_reported(guest: &Guest, grant: Grant) {
    const CAPACITY: u64 = 1_000;
    const CONTENT: &str = "hello";
    const MAX_TOKENS: u64 = 16;

    let (_host, filter) = load(guest, options(grant, CAPACITY, base_config()));
    let expected = expected_cost(CONTENT, MAX_TOKENS, 100);

    let (cost, remaining) = charge(
        &guest.name,
        &filter,
        &chat_body(PLAIN_MODEL, MAX_TOKENS, CONTENT),
    );
    assert_eq!(
        cost,
        expected,
        "{}: ceil({}/{CHARS_PER_TOKEN}) input + {MAX_TOKENS} reserved, at 100%",
        guest.name,
        CONTENT.len()
    );
    assert_eq!(
        remaining,
        CAPACITY - expected,
        "{}: the bucket reports what is left after this request's acquire",
        guest.name
    );
}

/// Assertion 4: once the bucket cannot pay for the next request, the filter answers 429 with the
/// spec's body and a `retry-after` in whole seconds, rounded UP (a client that waited the
/// rounded-down hint would simply be denied again).
pub fn draining_the_bucket_is_429_with_a_retry_after(guest: &Guest, grant: Grant) {
    const CONTENT: &str = "hello";
    const MAX_TOKENS: u64 = 16;
    let cost = expected_cost(CONTENT, MAX_TOKENS, 100);
    // Capacity buys exactly one request and leaves too little for a second.
    let capacity = cost + 2;

    let (_host, filter) = load(guest, options(grant, capacity, base_config()));
    let body = chat_body(PLAIN_MODEL, MAX_TOKENS, CONTENT);

    let (charged, remaining) = charge(&guest.name, &filter, &body);
    assert_eq!(charged, cost);
    assert_eq!(remaining, capacity - cost);

    let (decision, _logs) = filter.on_request(&keyed(), &RequestTrace::root()).unwrap();
    assert!(matches!(decision, RequestDecision::Continue));
    let (decision, _logs) = filter
        .on_request_body(&body, &RequestTrace::root())
        .unwrap();
    let resp = body_short_circuit(&guest.name, "a request past the budget", decision);
    assert_json_error(
        &guest.name,
        &resp,
        429,
        r#"{"error":"token budget exhausted"}"#,
    );

    // The host's own bucket math: whole refill intervals to cover the shortfall, which the guest
    // then rounds up to seconds.
    let shortfall = cost - remaining;
    let expected_retry = (shortfall.div_ceil(REFILL_TOKENS) * REFILL_INTERVAL_MS).div_ceil(1000);
    assert_eq!(
        header_value(&resp.headers, "retry-after"),
        Some(expected_retry.to_string().as_str()),
        "{}: retry-after is delay-seconds (RFC 9110), rounded up from the host's milliseconds",
        guest.name
    );
}

/// Assertion 5: a `model-cost-percent.<model>` entry changes what the same payload costs. This is
/// the only place the filter's decision is operator-tunable per request, so it must actually reach
/// the arithmetic.
pub fn the_model_multiplier_changes_the_charged_cost(guest: &Guest, grant: Grant) {
    const CAPACITY: u64 = 10_000;
    const CONTENT: &str = "hello";
    const MAX_TOKENS: u64 = 16;

    let face_value = expected_cost(CONTENT, MAX_TOKENS, 100);
    let premium = expected_cost(CONTENT, MAX_TOKENS, PREMIUM_PERCENT);
    assert_ne!(
        face_value, premium,
        "the fixture config must actually price the two models differently"
    );

    let (_host, filter) = load(guest, options(grant, CAPACITY, base_config()));

    let (plain_cost, _) = charge(
        &guest.name,
        &filter,
        &chat_body(PLAIN_MODEL, MAX_TOKENS, CONTENT),
    );
    assert_eq!(
        plain_cost, face_value,
        "{}: an unconfigured model is charged face value",
        guest.name
    );

    let (premium_cost, remaining) = charge(
        &guest.name,
        &filter,
        &chat_body(PREMIUM_MODEL, MAX_TOKENS, CONTENT),
    );
    assert_eq!(
        premium_cost, premium,
        "{}: {PREMIUM_MODEL} is configured at {PREMIUM_PERCENT}%",
        guest.name
    );
    assert_eq!(
        remaining,
        CAPACITY - face_value - premium,
        "{}: both acquires came out of the same bucket",
        guest.name
    );
}

/// Assertion 6: the pooling-safety one. Under `trusted` isolation the host REUSES instances, so the
/// per-request scratch that carries the key from `on-request` to `on-request-body` is the obvious
/// place for one request's state to leak into the next. Interleaving a 401 (which returns before
/// writing a key, and never reaches the acquire) between two charged requests proves the scratch
/// rule holds in the real runtime: the 401'd request must report NO cost headers of its own, and
/// the request after it must still be priced and accounted exactly.
pub fn an_unkeyed_request_between_two_charges_does_not_corrupt_the_scratch(
    guest: &Guest,
    grant: Grant,
) {
    const CAPACITY: u64 = 10_000;
    const CONTENT: &str = "hello";
    const MAX_TOKENS: u64 = 16;

    let (_host, filter) = load(guest, options(grant, CAPACITY, base_config()));
    let body = chat_body(PLAIN_MODEL, MAX_TOKENS, CONTENT);
    let cost = expected_cost(CONTENT, MAX_TOKENS, 100);

    let (first_cost, first_remaining) = charge(&guest.name, &filter, &body);
    assert_eq!(first_cost, cost);
    assert_eq!(first_remaining, CAPACITY - cost);

    // The interloper: unkeyed, so it is refused before the acquire path runs at all.
    let unkeyed = request(&[]);
    let (decision, _logs) = filter.on_request(&unkeyed, &RequestTrace::root()).unwrap();
    let resp = short_circuit(&guest.name, "an unkeyed request", decision);
    assert_json_error(&guest.name, &resp, 401, r#"{"error":"missing api key"}"#);
    // The response hook on that same instance must NOT report the previous request's numbers —
    // reading a scratch field this request never wrote is precisely the leak being tested for.
    let (decision, _logs) = filter
        .on_response(&unkeyed, &upstream_200(), &RequestTrace::root())
        .unwrap();
    assert!(
        matches!(decision, ResponseDecision::Continue),
        "{}: a request that was never charged must report nothing — it would be reporting another \
         caller's cost. Got {decision:?}",
        guest.name
    );

    let (second_cost, second_remaining) = charge(&guest.name, &filter, &body);
    assert_eq!(
        second_cost, cost,
        "{}: the request after a 401 is priced from its own body, not from stale scratch",
        guest.name
    );
    assert_eq!(
        second_remaining,
        CAPACITY - 2 * cost,
        "{}: exactly two acquires happened — the 401'd request spent nothing",
        guest.name
    );
}

/// Assertion 7: a broken `chars-per-token` must fail the LOAD, not one request at a time. `init`
/// parses it, and with `trusted` isolation the host eager-builds an instance at load, so a manifest
/// typo surfaces to the operator immediately (ADR 000066). `"0"` is included because it is the case
/// a language can silently absorb — dividing by zero yields infinity rather than failing — so the
/// guest has to reject it explicitly instead of relying on the arithmetic to complain.
pub fn a_broken_chars_per_token_fails_the_load(guest: &Guest, grant: Grant) {
    for bad in ["abc", "0"] {
        let mut config = base_config();
        config.insert("chars-per-token".to_string(), bad.to_string());
        let err = try_load(guest, options(grant, 1_000, config))
            .err()
            .unwrap_or_else(|| {
                panic!(
                    "{}: chars-per-token = {bad:?} must fail the load, not price requests wrong",
                    guest.name
                )
            });
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("instantiat") || msg.contains("trap") || msg.contains("wasm"),
            "{}: expected an init/instantiate failure for chars-per-token = {bad:?}, got: {err}",
            guest.name
        );
    }
}

/// A `key-header` that is present but empty must fail the load too. It can never match a header
/// name, so the filter would answer 401 to every request for as long as it ran — a broken
/// configuration, not a strict one, and the operator should hear about it at load like any other.
pub fn an_empty_key_header_fails_the_load(guest: &Guest, grant: Grant) {
    let mut config = base_config();
    config.insert("key-header".to_string(), String::new());
    let err = try_load(guest, options(grant, 1_000, config))
        .err()
        .unwrap_or_else(|| {
            panic!(
                "{}: an empty key-header must fail the load, not 401 every request",
                guest.name
            )
        });
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("instantiat") || msg.contains("trap") || msg.contains("wasm"),
        "{}: expected an init/instantiate failure for an empty key-header, got: {err}",
        guest.name
    );
}

/// A `max_tokens` that is not a non-negative integer is treated as ABSENT — never coerced. Each
/// malformed spelling must price EXACTLY as omitting the field does: truncating `12.9` to 12 or
/// clamping `-1` to 0 would make a malformed value CHEAPER than an honest one, and the cheapest way
/// to use the API would become lying about the reservation. A genuine integer past the ceiling is
/// clamped instead of rejected, so an absurd reservation prices absurdly and the bucket denies it.
pub fn a_malformed_max_tokens_prices_as_absent(guest: &Guest, grant: Grant) {
    const CONTENT: &str = "hello";
    /// The guests' shared ceiling on a client-supplied reservation.
    const CEILING: u64 = 100_000_000;

    let absent = expected_cost(CONTENT, MAX_TOKENS_DEFAULT, 100);
    let (_host, filter) = load(guest, options(grant, 1_000_000, base_config()));

    let (cost, _) = charge(
        &guest.name,
        &filter,
        &body_without_max_tokens(PLAIN_MODEL, CONTENT),
    );
    assert_eq!(
        cost, absent,
        "{}: an absent max_tokens is priced at max-tokens-default",
        guest.name
    );

    for spelling in [
        "-1",               // negative
        "12.9",             // fractional
        "\"256\"",          // a string, not a number
        "null",             // present but not a number at all
        "9007199254740993", // past the exact-integer range of a JSON number
    ] {
        let (cost, _) = charge(
            &guest.name,
            &filter,
            &raw_max_tokens_body(PLAIN_MODEL, spelling, CONTENT),
        );
        assert_eq!(
            cost, absent,
            "{}: max_tokens = {spelling} is not a non-negative integer, so it must price exactly \
             like an absent one — anything cheaper is a discount for malformed input",
            guest.name
        );
    }

    let (_host, filter) = load(guest, options(grant, 4 * CEILING, base_config()));
    let (cost, _) = charge(
        &guest.name,
        &filter,
        &raw_max_tokens_body(PLAIN_MODEL, "200000000", CONTENT),
    );
    assert_eq!(
        cost,
        expected_cost(CONTENT, CEILING, 100),
        "{}: a genuine integer past the ceiling is clamped to it, not dropped to the default",
        guest.name
    );
}

/// The key header is decoded as UTF-8 because `host-ratelimit.try-acquire` takes a WIT `string`,
/// which malformed bytes cannot be lowered into. Those bytes get the SAME 401 as a missing key —
/// never a trap, because the data plane does not panic on client input. A well-formed non-ASCII key
/// is accepted like any other: refusing it would be a limitation, not a defence.
pub fn a_key_header_that_is_not_utf8_is_401_and_non_ascii_is_accepted(guest: &Guest, grant: Grant) {
    const CONTENT: &str = "hello";
    const MAX_TOKENS: u64 = 16;
    let (_host, filter) = load(guest, options(grant, 10_000, base_config()));

    // 0xC3 0x28 is a truncated two-byte sequence — the canonical invalid-UTF-8 probe.
    let (decision, _logs) = filter
        .on_request(&keyed_raw(&[0xC3, 0x28, 0xFF]), &RequestTrace::root())
        .unwrap_or_else(|e| panic!("{}: a malformed key must not trap: {e}", guest.name));
    let resp = short_circuit(&guest.name, "a key that is not UTF-8", decision);
    assert_json_error(&guest.name, &resp, 401, r#"{"error":"missing api key"}"#);

    let non_ascii = keyed_raw("tenant-ünïcode-テナント".as_bytes());
    let (cost, _) = charge_as(
        &guest.name,
        &filter,
        &non_ascii,
        &chat_body(PLAIN_MODEL, MAX_TOKENS, CONTENT),
    );
    assert_eq!(
        cost,
        expected_cost(CONTENT, MAX_TOKENS, 100),
        "{}: a well-formed non-ASCII key is a key like any other",
        guest.name
    );
}

/// A model name too long to be plausible is never turned into a `model-cost-percent.<model>`
/// lookup — the name is untrusted body content. The config here DOES carry a multiplier under that
/// exact name, so a guest that looked it up would charge the premium; charging face value is the
/// proof that the lookup was skipped, and face value is the EXPENSIVE side of the fallback.
pub fn an_over_long_model_name_is_never_looked_up(guest: &Guest, grant: Grant) {
    const CONTENT: &str = "hello";
    const MAX_TOKENS: u64 = 16;
    // 129 bytes: one past the guests' shared 128-byte bound.
    let long_model = "m".repeat(129);

    let mut config = base_config();
    config.insert(
        format!("model-cost-percent.{long_model}"),
        PREMIUM_PERCENT.to_string(),
    );
    let (_host, filter) = load(guest, options(grant, 10_000, config));

    let (cost, _) = charge(
        &guest.name,
        &filter,
        &chat_body(&long_model, MAX_TOKENS, CONTENT),
    );
    assert_eq!(
        cost,
        expected_cost(CONTENT, MAX_TOKENS, 100),
        "{}: a 129-byte model name is charged face value, config entry or not",
        guest.name
    );
}

/// An unparseable `model-cost-percent.<model>` fails the REQUEST closed. Unlike the keys `init`
/// validates, this one only becomes reachable once a payload names that model, so it cannot fail the
/// load — and falling back to 100% would quietly undercharge exactly the model the operator singled
/// out as premium. A limiter that silently mis-charges is worse than one that stops.
pub fn an_unparseable_model_multiplier_fails_the_request_closed(guest: &Guest, grant: Grant) {
    const CONTENT: &str = "hello";
    const MAX_TOKENS: u64 = 16;
    const BROKEN_MODEL: &str = "broken-model";

    let mut config = base_config();
    config.insert(
        format!("model-cost-percent.{BROKEN_MODEL}"),
        "not-a-number".to_string(),
    );
    let (_host, filter) = load(guest, options(grant, 10_000, config));

    let (decision, _logs) = filter.on_request(&keyed(), &RequestTrace::root()).unwrap();
    assert!(matches!(decision, RequestDecision::Continue));

    let err = filter
        .on_request_body(
            &chat_body(BROKEN_MODEL, MAX_TOKENS, CONTENT),
            &RequestTrace::root(),
        )
        .err()
        .unwrap_or_else(|| {
            panic!(
                "{}: an unparseable multiplier must fail the request, not charge face value",
                guest.name
            )
        });
    assert!(
        matches!(err, RunError::Trap(_)),
        "{}: expected a trap, got {err:?}",
        guest.name
    );

    // A model with a VALID entry still prices normally — the trap is scoped to the broken key,
    // not to the filter as a whole.
    let (cost, _) = charge(
        &guest.name,
        &filter,
        &chat_body(PREMIUM_MODEL, MAX_TOKENS, CONTENT),
    );
    assert_eq!(
        cost,
        expected_cost(CONTENT, MAX_TOKENS, PREMIUM_PERCENT),
        "{}: one broken config key does not take the whole filter down",
        guest.name
    );
}
