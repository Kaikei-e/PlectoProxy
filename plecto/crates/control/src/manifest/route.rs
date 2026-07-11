//! A routing rule (`[[route]]`, ADR 000013 / 000034) and its matching / rate-limit sub-config.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One routing rule (ADR 000013 / 000034): match a request by the `[route.match]` dimensions, run
/// an inline chain of `filters`, and forward to a single `upstream` (shorthand) or a weighted set of
/// `backends` (traffic split / canary). `strip_prefix` is a **host-native** path rewrite applied to
/// the *forwarded* request only (the chain still sees the original path) — the common reverse-proxy
/// prefix-strip, without a `plecto:filter` contract change. `filters` / `strip_prefix` / `rate_limit`
/// are per-route: they apply identically across every backend (ADR 000034 keeps a backend a pure
/// `{upstream, weight}` pair; a route needing different policy per target uses a separate route).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// The match dimensions (`[route.match]`): host / path_prefix / method / headers / query (ADR
    /// 000034). At least `path_prefix` is required; all other dimensions are optional and ANDed.
    #[serde(rename = "match")]
    pub matcher: RouteMatch,
    /// This route's chain: filter ids run in order (may be empty for a pure pass-through route).
    #[serde(default)]
    pub filters: Vec<String>,
    /// Single-upstream shorthand: the `[[upstream]]` `name` to forward a passing request to.
    /// Mutually exclusive with `backends` (exactly one is required; validated at build). A single
    /// `upstream` is normalised to a one-element weighted set (weight 1) at compile time.
    #[serde(default)]
    pub upstream: Option<String>,
    /// Weighted traffic split (ADR 000034): forward to these `{upstream, weight}` backends in
    /// proportion to their weights (canary). Mutually exclusive with `upstream`. Empty unless used.
    #[serde(default)]
    pub backends: Vec<Backend>,
    /// If set and the forwarded path starts with it, strip it before forwarding to the upstream
    /// (host-native rewrite; the chain saw the original path). E.g. `/api` + `/api/x` → `/x`.
    #[serde(default)]
    pub strip_prefix: Option<String>,
    /// Native fast-path rate limit (ADR 000033): a coarse token-bucket baseline consulted BEFORE
    /// this route's filter chain. Absent = unlimited (the default). Distinct from the per-filter
    /// `host-ratelimit` capability (ADR 000026): this is the operator's native floor on a route
    /// (or per client-IP), needs no WASM filter, and never crosses the WASM boundary.
    #[serde(default)]
    pub rate_limit: Option<RouteRateLimit>,
    /// HTTP/1.1 Upgrade opt-in (ADR 000048): the Upgrade tokens this route tunnels. Absent =
    /// deny-by-default (the Upgrade/Connection pair keeps being stripped as hop-by-hop).
    #[serde(default)]
    pub upgrade: Option<RouteUpgrade>,
    /// Native response compression opt-in (ADR 000074 / 000075): negotiate a content coding against the
    /// client's `Accept-Encoding` and compress eligible responses AFTER the response chain
    /// (filters always see identity). Absent = never transform — deny-by-default, which is also
    /// the per-route BREACH opt-out. Do **not** enable on routes that reflect secrets into the
    /// response body (CSRF tokens, session nonces echoed from the request): compression + reflection
    /// enables BREACH-class chosen-plaintext attacks against TLS. Leave those routes uncompressed.
    #[serde(default)]
    pub compression: Option<RouteCompression>,
}

/// A route's Upgrade declaration (`[route.upgrade]`, ADR 000048). The allowlist shape is the
/// h2c-smuggling mitigation (only listed tokens are ever re-issued upstream); `h2c` itself is
/// rejected at validation (ADR 000015 — Plecto has no h2c on either side).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteUpgrade {
    /// Upgrade tokens to tunnel, matched case-insensitively against the client's `Upgrade`
    /// header (e.g. `["websocket"]`). Must be non-empty; `h2c` is rejected.
    pub protocols: Vec<String>,
    /// Idle timeout for an established tunnel, in ms — a byte in EITHER direction resets it
    /// (the activity-based form nginx/Envoy/HAProxy all share). `0` disables the timer.
    #[serde(default = "default_upgrade_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
}

/// 5 minutes — Envoy's stream-idle default; long enough for ping/pong-quiet apps, short enough
/// that an abandoned tunnel cannot hold a connection permit for hours.
fn default_upgrade_idle_timeout_ms() -> u64 {
    300_000
}

/// A route's compression declaration (`[route.compression]`, ADR 000074). Every field has a safe
/// default, so the bare block header is the whole opt-in; the fields exist to narrow it.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCompression {
    /// The codings this route offers, in SERVER-PREFERENCE order — the tie-break when the
    /// client's qvalues don't decide (RFC 9110 §12.5.3 orders only by qvalue). Default
    /// `["zstd", "br", "gzip"]`: best ratio-per-CPU first, universal fallback last.
    #[serde(default = "default_compression_algorithms")]
    pub algorithms: Vec<CompressionAlgorithm>,
    /// Don't compress a response whose declared `Content-Length` is below this (bytes). Under
    /// ~1 KiB the codec dictionary + trailer can exceed the saving (common practice defaults
    /// cluster around tens of bytes to ~1 KiB; 1024 is the safe middle). A response with NO
    /// declared length (streamed / chunked) is always eligible — its size is unknowable up front.
    #[serde(default = "default_compression_min_length")]
    pub min_length: u64,
    /// The `type/subtype` allowlist (matched against the response `Content-Type` essence,
    /// case-insensitive, parameters ignored). REPLACES the default when set. The default covers
    /// the textual web types; already-compressed media (images, video, archives) and
    /// `text/event-stream` (a compressor buffering an SSE stream stalls events) stay excluded.
    #[serde(default = "default_compression_content_types")]
    pub content_types: Vec<String>,
}

/// A content coding Plecto can produce (ADR 000074: gzip baseline + zstd / brotli). The serde
/// spelling is the wire token (`Accept-Encoding` / `Content-Encoding`), so the manifest reads
/// exactly like the negotiation it configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressionAlgorithm {
    /// RFC 8878 Zstandard. Encoded with window_log ≤ 23: RFC 9659 forbids frames over an 8 MiB
    /// window for web content (browsers reject them).
    Zstd,
    /// RFC 7932 Brotli.
    Br,
    /// RFC 9110 §8.4.1.3 gzip — the universally-supported baseline.
    Gzip,
}

impl CompressionAlgorithm {
    /// The registered content-coding token (what goes on the wire in `Content-Encoding`).
    pub fn token(self) -> &'static str {
        match self {
            CompressionAlgorithm::Zstd => "zstd",
            CompressionAlgorithm::Br => "br",
            CompressionAlgorithm::Gzip => "gzip",
        }
    }
}

fn default_compression_algorithms() -> Vec<CompressionAlgorithm> {
    vec![
        CompressionAlgorithm::Zstd,
        CompressionAlgorithm::Br,
        CompressionAlgorithm::Gzip,
    ]
}

fn default_compression_min_length() -> u64 {
    1024
}

/// The default compressible-content allowlist: the textual web types every major proxy ships
/// (HTML/CSS/JS/JSON/XML feeds/SVG) plus `application/wasm` (components compress well and are
/// Plecto's own distribution format). Notably ABSENT: `text/event-stream` and all
/// already-compressed media.
fn default_compression_content_types() -> Vec<String> {
    [
        "text/html",
        "text/css",
        "text/plain",
        "text/xml",
        "text/javascript",
        "application/javascript",
        "application/x-javascript",
        "application/json",
        "application/xml",
        "application/xhtml+xml",
        "application/rss+xml",
        "application/atom+xml",
        "image/svg+xml",
        "application/wasm",
    ]
    .map(str::to_string)
    .to_vec()
}

/// The match dimensions of a route (`[route.match]`, ADR 000034), modelled on Gateway-API v1.5.0
/// HTTPRoute matching. A request matches when EVERY specified dimension matches (AND); an
/// unspecified dimension is a wildcard. Among matching routes the most specific wins (see
/// `route::select`): host-constrained > longest `path_prefix` > `method` present > more header
/// matches > more query matches, with manifest order the final stable tie-break.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteMatch {
    /// Match only this authority (case-insensitive, port ignored). `None` matches any host.
    #[serde(default)]
    pub host: Option<String>,
    /// Match requests whose path starts with this prefix (on a `/` boundary). Longest wins.
    pub path_prefix: String,
    /// Match only this HTTP method (exact, upper-case token, e.g. `"POST"`). `None` matches any.
    #[serde(default)]
    pub method: Option<String>,
    /// Header matches: every entry must be present with an exact value. Header NAME is matched
    /// case-insensitively (lower-cased here at parse-ish time); the VALUE is matched byte-exact.
    /// `BTreeMap` (not `HashMap`) to keep the manifest's deterministic-serialisation invariant.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Query-parameter matches: every entry must be present with an exact value. Parameter NAME is
    /// case-sensitive (Gateway-API semantics, asymmetric with headers); the VALUE is matched exact.
    #[serde(default)]
    pub query: BTreeMap<String, String>,
}

/// One weighted backend of a route's traffic split (`[[route.backends]]`, ADR 000034): the
/// `[[upstream]]` `name` and its integer `weight`. The proportion a backend receives is
/// `weight / Σweights` (Gateway-API semantics). `weight` defaults to 1, caps at 1_000_000 (Σ
/// overflow guard), and `0` drains the backend (no traffic). Validated at build (ADR 000034 5b).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    /// The `[[upstream]]` `name` this backend forwards to.
    pub upstream: String,
    /// This backend's integer weight in the split (default 1, max 1_000_000, `0` = drain).
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 {
    1
}

/// Upper bound on a single backend weight (Gateway-API `Maximum=1000000`). Caps the summed weight
/// so the weighted-split accumulator and the precomputed table stay bounded (ADR 000034 5b).
pub(crate) const MAX_BACKEND_WEIGHT: u32 = 1_000_000;

impl Route {
    /// This route's forwarding targets as `(upstream_name, weight)` pairs (ADR 000034): the single
    /// `upstream` shorthand normalised to one weight-1 backend, or the explicit weighted `backends`.
    /// EXACTLY ONE of the two must be set — both or neither is a config error (returned as a reason
    /// the caller wraps with the route's context). Borrows the names from `self` (no allocation).
    pub(crate) fn targets(&self) -> Result<Vec<(&str, u32)>, &'static str> {
        match (self.upstream.as_deref(), self.backends.as_slice()) {
            (Some(_), [_, ..]) => {
                Err("a route sets both `upstream` and `backends`; set exactly one")
            }
            (None, []) => Err("a route sets neither `upstream` nor `backends`; set exactly one"),
            (Some(name), []) => Ok(vec![(name, 1)]),
            (None, backends) => Ok(backends
                .iter()
                .map(|b| (b.upstream.as_str(), b.weight))
                .collect()),
        }
    }
}

/// Native per-route rate-limit spec (ADR 000033), declared as `[route.rate_limit]`. A coarse
/// token bucket the fast path consults before forwarding — the Tier-0 baseline a filterless route
/// otherwise lacks. `rate`/`burst` map onto the same token-bucket math as `host-ratelimit`
/// (`capacity = burst`, refill `rate` tokens every second), but the surface is deliberately the
/// friendlier two-knob (rate + burst) form, since this is a blunt floor, not a policy limiter.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteRateLimit {
    /// Sustained requests per second (tokens added each second). Must be non-zero.
    pub rate: u64,
    /// Burst capacity: the most tokens the bucket holds (and starts full with). Must be non-zero.
    pub burst: u64,
    /// What the bucket counts against (default `route`). `route` shares one bucket across every
    /// client of the route (a total floor); `client-ip` gives each client its own bucket (fairness
    /// between clients), keyed on the connection peer (v4 /32, v6 /64).
    #[serde(default)]
    pub key: RateLimitKeyKind,
}

/// The dimension a native route rate limit counts against (ADR 000033).
#[derive(
    Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "kebab-case")]
pub enum RateLimitKeyKind {
    /// One shared bucket for the whole route — a total cap regardless of client.
    #[default]
    Route,
    /// A per-client-IP bucket (peer address, v4 /32 + v6 /64), bounded to a fixed-size table.
    ClientIp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    #[test]
    fn route_rate_limit_defaults_absent_and_parses_with_key() {
        // Absent `[route.rate_limit]` → no native limiter (unlimited), the default.
        let m = Manifest::from_toml(
            r#"
[[route]]
upstream = "a"
[route.match]
path_prefix = "/"
"#,
        )
        .unwrap();
        assert!(
            m.routes[0].rate_limit.is_none(),
            "an absent rate_limit is unlimited"
        );

        // Present → rate/burst are read; `key` defaults to `route`.
        let m2 = Manifest::from_toml(
            r#"
[[route]]
upstream = "a"
[route.match]
path_prefix = "/"
[route.rate_limit]
rate = 100
burst = 200
"#,
        )
        .unwrap();
        let rl = m2.routes[0].rate_limit.unwrap();
        assert_eq!(rl.rate, 100);
        assert_eq!(rl.burst, 200);
        assert_eq!(rl.key, RateLimitKeyKind::Route, "key defaults to route");

        // `key = "client-ip"` is the kebab-case spelling.
        let m3 = Manifest::from_toml(
            r#"
[[route]]
upstream = "a"
[route.match]
path_prefix = "/"
[route.rate_limit]
rate = 5
burst = 5
key = "client-ip"
"#,
        )
        .unwrap();
        assert_eq!(
            m3.routes[0].rate_limit.unwrap().key,
            RateLimitKeyKind::ClientIp
        );
    }
}
