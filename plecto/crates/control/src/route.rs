//! Host-native routing (ADR 000013 / 000034): match a request to a route by its `[route.match]`
//! dimensions (host, path prefix, method, headers, query), then the fast-path server runs that
//! route's chain and forwards to its weighted backends. Pure config logic — no I/O, no wasmtime —
//! so it is unit-tested directly and runs on the async thread (the blocking work is the chain
//! dispatch, not the match). Matching is allocation-free: the compiled dimensions are pre-normalised
//! at build, and per request we only scan and compare borrowed slices.

use std::borrow::Cow;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderName, HeaderValue};
use plecto_host::{BodyHooks, Header, LoadedFilter};

use crate::error::ControlError;
use crate::manifest::{
    CompressionAlgorithm, MAX_RESPONSE_BODY_CAP, OverCapMode, Route, RouteCompression,
    RouteHeaders, RouteResponseBody, RouteTimeouts, UninspectableMode,
};
use crate::ratelimit::{NativeRateLimit, RateLimitDecision};
use crate::upstream::UpstreamGroup;
use crate::weighted::{self, WeightedBackends};

/// A route compiled from a manifest [`crate::Route`] into the live config: the match dimensions are
/// pre-normalised (host + header names lower-cased, method upper-cased), and the forwarding target —
/// a single upstream or a weighted split — is resolved to a [`WeightedBackends`] whose groups are
/// shared (`Arc`) with the upstream registry, so the actual instance is chosen by round-robin over
/// the healthy set at forward time, not here (ADR 000017 / 000024).
#[derive(Clone)]
pub(crate) struct CompiledRoute {
    /// Lower-cased authority to match, or `None` for any host.
    pub(crate) host: Option<String>,
    pub(crate) path_prefix: String,
    /// Upper-cased HTTP method to match (exact), or `None` for any method (ADR 000034).
    pub(crate) method: Option<String>,
    /// Header matches (ADR 000034): `(lower-cased name, exact value)`, ANDed. Name is compared
    /// case-insensitively, value byte-/string-exact. A `Vec` (not a map) since it is iterated, small,
    /// and order-stable.
    pub(crate) headers: Vec<(String, String)>,
    /// Query-parameter matches (ADR 000034): `(name, exact value)`, ANDed. Name is case-sensitive
    /// (Gateway-API semantics, asymmetric with headers).
    pub(crate) query: Vec<(String, String)>,
    /// This route's inline chain (filter ids, in order). Kept for validation / introspection
    /// (`validate_routes`, `has_filters`); per-request dispatch uses `resolved_chain` instead so
    /// it never re-hashes these ids against `ActiveConfig::filters`.
    pub(crate) filters: Vec<String>,
    /// `filters` resolved to the loaded filter, in order — built once per reload (`build_active`
    /// already has the id → `Arc<LoadedFilter>` map in scope there). `chain::dispatch_request` /
    /// `dispatch_request_body` / `dispatch_response` run this directly instead of doing a
    /// `HashMap::get` per filter id on every single request.
    pub(crate) resolved_chain: Vec<Arc<LoadedFilter>>,
    /// Which body directions any filter on this route reads (the OR-fold of the chain's
    /// [`BodyHooks`], ADR 000038 / 000098). Precomputed at build from the loaded filters so
    /// per-request buffering is a single bool check per direction. A direction no filter exports
    /// keeps its body on the zero-copy stream path.
    pub(crate) body_hooks: BodyHooks,
    /// The route's forwarding target: a weighted set of upstream groups (a single `upstream` is a
    /// one-element set). The fast path picks a group via [`WeightedBackends::pick`], then the group
    /// picks a healthy instance. `Arc`-shared so the split cursor persists across a config generation.
    pub(crate) backends: Arc<WeightedBackends>,
    /// `Arc<str>`, not `String`: `snapshot.rs`'s `find_route` clones this into a fresh
    /// [`RouteInfo`] on every single request — the same reason `backends` above is `Arc`-shared
    /// rather than deep-cloned. An owned `String` here would allocate on every request for any
    /// route with a `strip_prefix` configured; an `Arc<str>` clone is an atomic refcount bump.
    pub(crate) strip_prefix: Option<Arc<str>>,
    /// This route's native rate limiter (ADR 000033), or `None` for unlimited (the default). Shared
    /// (`Arc`) so every request on the route consults the same token buckets within a config
    /// generation; a reload builds a fresh limiter (the node-local buckets reset).
    pub(crate) rate_limit: Option<Arc<NativeRateLimit>>,
    /// This route's Upgrade opt-in (ADR 000048), or `None` for deny-by-default (strip as today).
    pub(crate) upgrade: Option<Arc<UpgradeConfig>>,
    /// This route's compression opt-in (ADR 000074), or `None` for never-transform (the default).
    pub(crate) compression: Option<Arc<CompressionConfig>>,
    /// This route's declared response headers (ADR 000100), or `None` for no declaration. Named
    /// `response_headers` here because `headers` above is already the request-side MATCH
    /// dimension — one is what the route matches on, the other what it stamps on the way out.
    pub(crate) response_headers: Option<Arc<ResponseHeaders>>,
    /// This route's compiled `[route.timeouts]` overrides (ADR 000102). Two `Option<Duration>`s
    /// by value, not behind an `Arc`: the per-request `RouteInfo` clone copies 32 bytes instead of
    /// bumping a refcount, and the resolution against the chosen upstream's defaults is then a
    /// pair of `Option::unwrap_or`.
    pub(crate) timeouts: TimeoutConfig,
    /// This route's compiled `[route.response_body]` settings (ADR 000098), or the strict defaults
    /// when the block is absent. Only consulted when `body_hooks.response` is set.
    pub(crate) response_body: Arc<ResponseBodyConfig>,
}

impl CompiledRoute {
    /// Compile one already-validated manifest route (ADR 000013 / 000034): pre-normalise the
    /// match dimensions, resolve the chain against the loaded filters, and build the per-route
    /// facilities (native limiter / upgrade / compression). Lives beside the struct so the
    /// knowledge of a `CompiledRoute`'s shape stays in this file (`build_active` previously
    /// inlined the whole block).
    pub(crate) fn compile(
        r: &Route,
        backends: WeightedBackends,
        filters: &std::collections::HashMap<String, Arc<LoadedFilter>>,
    ) -> Self {
        Self {
            // Pre-normalise the compiled match dimensions so per-request matching is
            // allocation-free (ADR 000034): host + header names lower-cased (case-insensitive),
            // method upper-cased (exact upper-case token), query names kept as-is
            // (case-sensitive).
            host: r.matcher.host.as_ref().map(|h| h.to_ascii_lowercase()),
            path_prefix: r.matcher.path_prefix.clone(),
            method: r.matcher.method.as_ref().map(|m| m.to_ascii_uppercase()),
            headers: r
                .matcher
                .headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                .collect(),
            query: r
                .matcher
                .query
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            // The route buffers a direction iff at least one of its filters exports that
            // direction's hook (ADR 000038 / 000098). Computed from the loaded filters here so
            // the fast path only checks a bool per direction.
            body_hooks: r
                .filters
                .iter()
                .fold(BodyHooks::NONE, |acc, id| match filters.get(id) {
                    Some(f) => acc.union(f.body_hooks()),
                    None => acc,
                }),
            // present: `validate_routes` already checked every id against the loaded set, so
            // this is always `Some` — `filter_map` stays total (no indexing/unwrap panic)
            // rather than asserting an invariant that's already enforced one step earlier.
            resolved_chain: r
                .filters
                .iter()
                .filter_map(|id| filters.get(id).cloned())
                .collect(),
            filters: r.filters.clone(),
            backends: Arc::new(backends),
            // Built once per reload (unlike the per-request `RouteInfo` clone in `snapshot.rs`),
            // so allocating here to convert into the per-request-cheap `Arc<str>` is fine.
            strip_prefix: r.strip_prefix.as_deref().map(Arc::from),
            // Build the native limiter (ADR 000033) — `rate`/`burst` were validated non-zero.
            rate_limit: r.rate_limit.map(|rl| Arc::new(NativeRateLimit::new(rl))),
            // Compile the Upgrade opt-in (ADR 000048) — tokens were validated non-empty/non-h2c.
            upgrade: r
                .upgrade
                .as_ref()
                .map(|u| Arc::new(UpgradeConfig::new(&u.protocols, u.idle_timeout_ms))),
            // Compile the compression opt-in (ADR 000074) — codings / allowlist validated.
            compression: r
                .compression
                .as_ref()
                .map(|c| Arc::new(CompressionConfig::new(c))),
            // Compile the response-header declaration (ADR 000100) — names / values validated.
            response_headers: r
                .headers
                .as_ref()
                .map(|h| Arc::new(ResponseHeaders::new(h))),
            // Compile the timeout overrides (ADR 000102); absent = take the upstream's defaults.
            timeouts: r
                .timeouts
                .as_ref()
                .map(TimeoutConfig::new)
                .unwrap_or_default(),
            // Compile the response-body settings (ADR 000098); absent = the strict defaults.
            response_body: Arc::new(
                r.response_body
                    .as_ref()
                    .map(ResponseBodyConfig::new)
                    .unwrap_or_default(),
            ),
        }
    }
}

// Manual `Debug`: `LoadedFilter` (behind `resolved_chain`'s `Arc`) doesn't implement it, so this
// can't be `#[derive(Debug)]`. Reports `resolved_chain` by length — the same information `filters`
// (the id list, printed in full) already carries, just resolved.
impl std::fmt::Debug for CompiledRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledRoute")
            .field("host", &self.host)
            .field("path_prefix", &self.path_prefix)
            .field("method", &self.method)
            .field("headers", &self.headers)
            .field("query", &self.query)
            .field("filters", &self.filters)
            .field("resolved_chain_len", &self.resolved_chain.len())
            .field("body_hooks", &self.body_hooks)
            .field("backends", &self.backends)
            .field("strip_prefix", &self.strip_prefix)
            .field("rate_limit", &self.rate_limit)
            .field("upgrade", &self.upgrade)
            .field("compression", &self.compression)
            .field("response_headers", &self.response_headers)
            .field("timeouts", &self.timeouts)
            .field("response_body", &self.response_body)
            .finish()
    }
}

/// A route's compiled `[route.upgrade]` (ADR 000048): the lower-cased token allowlist and the
/// tunnel idle timeout. Pre-normalised at build so the per-request check is a scan over a
/// (typically one-element) list.
#[derive(Debug)]
pub struct UpgradeConfig {
    protocols: Vec<String>,
    idle_timeout: Option<std::time::Duration>,
}

impl UpgradeConfig {
    pub(crate) fn new(protocols: &[String], idle_timeout_ms: u64) -> Self {
        Self {
            protocols: protocols
                .iter()
                .map(|p| p.trim().to_ascii_lowercase())
                .collect(),
            idle_timeout: (idle_timeout_ms > 0)
                .then(|| std::time::Duration::from_millis(idle_timeout_ms)),
        }
    }

    /// The first token of the client's `Upgrade` header value this route allows (lower-cased
    /// canonical form), or `None` — the request then stays plain HTTP (the header is stripped).
    /// RFC 9110 §7.8: token comparison is case-insensitive; the header may list alternatives.
    pub fn allowed_token(&self, upgrade_header: &str) -> Option<&str> {
        upgrade_header.split(',').map(str::trim).find_map(|tok| {
            self.protocols
                .iter()
                .find(|p| p.eq_ignore_ascii_case(tok))
                .map(String::as_str)
        })
    }

    /// The established tunnel's idle timeout; `None` = the operator disabled it (`0`).
    pub fn idle_timeout(&self) -> Option<std::time::Duration> {
        self.idle_timeout
    }
}

/// A route's compiled `[route.compression]` (ADR 000074): the offered codings in server-preference
/// order, the min-length floor, and the content-type allowlist (lower-cased at build so the
/// per-response check is an allocation-free case-insensitive scan over a short list).
#[derive(Debug)]
pub struct CompressionConfig {
    algorithms: Vec<CompressionAlgorithm>,
    min_length: u64,
    content_types: Vec<String>,
}

impl CompressionConfig {
    /// Compile a manifest `[route.compression]` block. Public because the fast-path server's
    /// negotiation unit tests build configs directly, without a manifest parse.
    pub fn new(rc: &RouteCompression) -> Self {
        Self {
            algorithms: rc.algorithms.clone(),
            min_length: rc.min_length,
            content_types: rc
                .content_types
                .iter()
                .map(|ct| ct.trim().to_ascii_lowercase())
                .collect(),
        }
    }

    /// The codings this route offers, in server-preference order (the qvalue tie-break).
    pub fn algorithms(&self) -> &[CompressionAlgorithm] {
        &self.algorithms
    }

    /// The declared-length floor: a response shorter than this is not worth a codec header.
    pub fn min_length(&self) -> u64 {
        self.min_length
    }

    /// Is this `Content-Type` essence (`type/subtype`, parameters already stripped) compressible
    /// on this route? Case-insensitive scan — the list is ~a dozen entries, no per-request alloc.
    pub fn content_type_eligible(&self, essence: &str) -> bool {
        let essence = essence.trim();
        self.content_types
            .iter()
            .any(|ct| ct.eq_ignore_ascii_case(essence))
    }
}

/// A route's compiled `[route.headers]` (ADR 000100): the operator's literal response-header
/// declaration, parsed into `http` types once per reload so stamping it onto a response is a map
/// write, never a re-parse. `remove` runs before `set`, and `set` REPLACES any same-named header
/// the upstream or a filter produced — the declaration is a floor, not a suggestion.
#[derive(Debug)]
pub struct ResponseHeaders {
    set: Vec<(HeaderName, HeaderValue)>,
    remove: Vec<HeaderName>,
}

impl ResponseHeaders {
    /// Compile a manifest `[route.headers]` block. Public because the fast-path server's unit
    /// tests build configs directly, without a manifest parse.
    ///
    /// `validate_response_headers` already parsed every name and value, so `filter_map` stays
    /// total here rather than asserting an invariant enforced one step earlier (the same
    /// discipline `resolved_chain` follows above).
    pub fn new(rh: &RouteHeaders) -> Self {
        Self {
            set: rh
                .set
                .iter()
                .filter_map(|(name, value)| {
                    Some((
                        HeaderName::from_bytes(name.as_bytes()).ok()?,
                        HeaderValue::from_str(value).ok()?,
                    ))
                })
                .collect(),
            remove: rh
                .remove
                .iter()
                .filter_map(|name| HeaderName::from_bytes(name.as_bytes()).ok())
                .collect(),
        }
    }

    /// Stamp the declaration onto an about-to-be-sent response.
    pub fn apply(&self, headers: &mut HeaderMap) {
        for name in &self.remove {
            headers.remove(name);
        }
        for (name, value) in &self.set {
            // `insert`, not `append`: the operator's declaration replaces every same-named
            // header already on the response, duplicates included.
            headers.insert(name.clone(), value.clone());
        }
    }
}

/// Parse one declared header name (ADR 000100), rejecting anything the operator must not name.
/// The reserved set is the hop-by-hop headers (RFC 9110 §7.6.1 — connection management belongs to
/// the transport, not to a declaration) plus `content-length`, which the host computes for the
/// body it actually sends: a manifest-set length is a response-desync primitive (CWE-444). Both
/// fail the BUILD rather than being dropped per response, where nobody would see it.
fn declared_header_name(name: &str) -> Result<HeaderName, String> {
    let parsed = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("headers names `{name}`, which is not a valid HTTP header name"))?;
    let reserved = parsed == http::header::CONTENT_LENGTH
        || plecto_host::HOP_BY_HOP_GUEST_HEADERS
            .iter()
            .any(|h| parsed.as_str() == *h);
    if reserved {
        return Err(format!(
            "headers names `{name}`, a hop-by-hop / framing header the host owns"
        ));
    }
    Ok(parsed)
}

/// Validate a route's `[route.headers]` declaration (ADR 000100), fail-closed. The declaration
/// lands on every response the route answers, so an unusable name or value must stop startup /
/// reload; an empty block is a config typo (it declares nothing) and is rejected the same way.
fn validate_response_headers(h: &crate::manifest::RouteHeaders) -> Result<(), String> {
    if h.set.is_empty() && h.remove.is_empty() {
        return Err("headers declares neither `set` nor `remove`".to_string());
    }
    let mut seen = HashSet::new();
    for (name, value) in &h.set {
        let parsed = declared_header_name(name)?;
        if !seen.insert(parsed) {
            return Err(format!(
                "headers.set declares `{name}` twice (header names are case-insensitive)"
            ));
        }
        if HeaderValue::from_str(value).is_err() {
            return Err(format!(
                "headers.set gives `{name}` a value that is not a valid HTTP header value"
            ));
        }
    }
    for name in &h.remove {
        declared_header_name(name)?;
    }
    Ok(())
}

/// A route's compiled `[route.response_body]` (ADR 000098): the cap, the two fail-closed modes,
/// and the content-type allowlist that arms buffering (lower-cased at build so the per-response
/// check is an allocation-free case-insensitive scan, like `CompressionConfig`'s).
#[derive(Debug)]
pub struct ResponseBodyConfig {
    max_bytes: usize,
    over_cap: OverCapMode,
    uninspectable: UninspectableMode,
    content_types: Vec<String>,
}

impl Default for ResponseBodyConfig {
    /// The settings a route with a response-body filter and no `[route.response_body]` block runs
    /// under. Built from the manifest defaults rather than restated, so the two cannot drift.
    fn default() -> Self {
        Self::new(&RouteResponseBody::default())
    }
}

impl ResponseBodyConfig {
    /// Compile a manifest `[route.response_body]` block. Public because the fast-path server's
    /// unit tests build configs directly, without a manifest parse.
    pub fn new(rb: &RouteResponseBody) -> Self {
        Self {
            // `validate_response_body` already bounded this by `MAX_RESPONSE_BODY_CAP`, which is
            // far inside `usize` on every target the proxy builds for; saturate rather than assert.
            max_bytes: usize::try_from(rb.max_bytes).unwrap_or(usize::MAX),
            over_cap: rb.over_cap,
            uninspectable: rb.uninspectable,
            content_types: rb
                .content_types
                .iter()
                .map(|ct| ct.trim().to_ascii_lowercase())
                .collect(),
        }
    }

    /// The most bytes of one response body the host will hold for the hook.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// What to do with a body over [`max_bytes`](Self::max_bytes).
    pub fn over_cap(&self) -> OverCapMode {
        self.over_cap
    }

    /// What to do with a response that cannot be inspected at all.
    pub fn uninspectable(&self) -> UninspectableMode {
        self.uninspectable
    }

    /// Does this `Content-Type` essence arm inspection on this route? The caller passes the
    /// AS-RECEIVED essence (ADR 000098 decision 3): deciding on a value a chain filter could have
    /// rewritten would let a filter earlier in the chain steer the response past the one that
    /// inspects it.
    pub fn content_type_eligible(&self, essence: &str) -> bool {
        let essence = essence.trim();
        self.content_types
            .iter()
            .any(|ct| ct.eq_ignore_ascii_case(essence))
    }
}

/// Validate a route's `[route.response_body]` declaration (ADR 000098), fail-closed: a cap of zero
/// could never hold a body, a cap over the request-side ceiling would buy the response plane more
/// headroom than the request plane has, and an empty or malformed allowlist would silently disarm
/// the inspection the route declared a filter for.
fn validate_response_body(rb: &RouteResponseBody) -> Result<(), String> {
    if rb.max_bytes == 0 {
        return Err("response_body.max_bytes must be non-zero".to_string());
    }
    if rb.max_bytes > MAX_RESPONSE_BODY_CAP {
        return Err(format!(
            "response_body.max_bytes is {} bytes, over the {MAX_RESPONSE_BODY_CAP}-byte ceiling",
            rb.max_bytes
        ));
    }
    if rb.content_types.is_empty() {
        return Err("response_body.content_types must be non-empty".to_string());
    }
    for ct in &rb.content_types {
        let essence = ct.trim();
        let well_formed = essence
            .split_once('/')
            .is_some_and(|(t, s)| !t.is_empty() && !s.is_empty())
            && !essence.contains([';', ',', ' ']);
        if !well_formed {
            return Err(format!(
                "response_body.content_types entry `{essence}` is not a type/subtype"
            ));
        }
    }
    Ok(())
}

/// A route's compiled `[route.timeouts]` (ADR 000102): the per-try / overall bounds this route
/// declares, each resolved against the upstream's own default at forward time. `None` is "not
/// declared" and `Some(Duration::ZERO)` is "declared disabled" — the fast path must keep the two
/// apart, since writing `0` is how a streaming route opts out of a short upstream default.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimeoutConfig {
    request: Option<Duration>,
    overall: Option<Duration>,
}

impl TimeoutConfig {
    /// Compile a manifest `[route.timeouts]` block. Public because the fast-path server's unit
    /// tests build configs directly, without a manifest parse.
    pub fn new(t: &RouteTimeouts) -> Self {
        Self {
            request: t.request_timeout_ms.map(Duration::from_millis),
            overall: t.overall_timeout_ms.map(Duration::from_millis),
        }
    }

    /// The effective per-try bound (ADR 000019 / 000102): this route's override, else the
    /// upstream's declared default. `Duration::ZERO` (either source) disables the bound.
    pub fn request_timeout(&self, upstream_default: Duration) -> Duration {
        self.request.unwrap_or(upstream_default)
    }

    /// The effective overall bound (ADR 000031 / 000102): this route's override, else the
    /// upstream's declared default. `Duration::ZERO` (either source) means no overall bound.
    pub fn overall_timeout(&self, upstream_default: Duration) -> Duration {
        self.overall.unwrap_or(upstream_default)
    }
}

/// A manifest route validated against the (already-loaded) filter set and upstream names, carrying
/// its already-resolved forwarding targets — so `build_active`'s later compile pass does not need
/// to call `targets()` a second time.
#[derive(Debug)]
pub(crate) struct ValidatedRoute<'a> {
    pub(crate) route: &'a Route,
    pub(crate) targets: Vec<(&'a str, u32)>,
}

/// Validate every route's forwarding target (`upstream`/`backends`), weighted split, filter
/// references, and native rate-limit config — PURELY, against the declared filter ids and
/// upstream names, with no I/O and no registry mutation (ADR 000013 / 000017 / 000033 / 000034).
/// Fails closed on the first invalid route, mirroring the checks `build_active` used to run
/// inline. Takes filter IDS (not loaded filters), so the static `validate_manifest` path shares
/// it without loading any artifact. Directly unit-testable with hand-built sets.
pub(crate) fn validate_routes<'a>(
    routes: &'a [Route],
    filter_ids: &HashSet<&str>,
    upstream_names: &HashSet<&str>,
) -> Result<Vec<ValidatedRoute<'a>>, ControlError> {
    let mut validated = Vec::with_capacity(routes.len());
    for r in routes {
        // The route's forwarding targets: the single `upstream` shorthand or weighted `backends`
        // (ADR 000034). Both-set / neither-set is fail-closed here.
        let targets = r.targets().map_err(|reason| ControlError::InvalidRoute {
            path_prefix: r.matcher.path_prefix.clone(),
            reason: reason.to_string(),
        })?;
        for (name, _) in &targets {
            if !upstream_names.contains(name) {
                return Err(ControlError::UnknownRouteUpstream {
                    path_prefix: r.matcher.path_prefix.clone(),
                    upstream: (*name).to_string(),
                });
            }
        }
        // Validate the weighted split (empty / all-zero / over-cap weight / oversized reduced
        // table) before the registry reconcile, so a bad split never mutates persistent state.
        let weights: Vec<u32> = targets.iter().map(|(_, w)| *w).collect();
        weighted::validate_split(&weights).map_err(|reason| ControlError::InvalidRoute {
            path_prefix: r.matcher.path_prefix.clone(),
            reason,
        })?;
        for f in &r.filters {
            if !filter_ids.contains(f.as_str()) {
                return Err(ControlError::UnknownRouteFilter {
                    path_prefix: r.matcher.path_prefix.clone(),
                    filter: f.clone(),
                });
            }
        }
        // Reject a native rate limit that can never serve a token (ADR 000033): a zero `rate`
        // never refills and a zero `burst` holds nothing — a config typo, fail-closed at build
        // before the limiter arithmetic ever runs (CWE-20).
        if let Some(rl) = &r.rate_limit {
            if rl.rate == 0 {
                return Err(ControlError::InvalidRouteRateLimit {
                    path_prefix: r.matcher.path_prefix.clone(),
                    reason: "rate must be non-zero".to_string(),
                });
            }
            if rl.burst == 0 {
                return Err(ControlError::InvalidRouteRateLimit {
                    path_prefix: r.matcher.path_prefix.clone(),
                    reason: "burst must be non-zero".to_string(),
                });
            }
        }
        // Validate the Upgrade opt-in (ADR 000048): an empty allowlist is a config typo, and
        // `h2c` is rejected outright — Plecto has no h2c on either side (ADR 000015), and
        // forwarding `Upgrade: h2c` is the classic smuggling vector the allowlist exists to block.
        if let Some(up) = &r.upgrade {
            if up.protocols.is_empty() {
                return Err(ControlError::InvalidRoute {
                    path_prefix: r.matcher.path_prefix.clone(),
                    reason: "upgrade.protocols must be non-empty".to_string(),
                });
            }
            for p in &up.protocols {
                if p.trim().is_empty() {
                    return Err(ControlError::InvalidRoute {
                        path_prefix: r.matcher.path_prefix.clone(),
                        reason: "upgrade.protocols contains an empty token".to_string(),
                    });
                }
                if p.trim().eq_ignore_ascii_case("h2c") {
                    return Err(ControlError::InvalidRoute {
                        path_prefix: r.matcher.path_prefix.clone(),
                        reason: "h2c upgrade is not supported (h2 is TLS+ALPN only; \
                                 forwarding h2c enables request smuggling)"
                            .to_string(),
                    });
                }
            }
        }
        // Validate the compression opt-in (ADR 000074): an empty coding list or an empty /
        // non-`type/subtype` allowlist entry is a config typo — fail closed at build, before a
        // request ever negotiates against it.
        if let Some(c) = &r.compression {
            if c.algorithms.is_empty() {
                return Err(ControlError::InvalidRoute {
                    path_prefix: r.matcher.path_prefix.clone(),
                    reason: "compression.algorithms must be non-empty".to_string(),
                });
            }
            let mut seen = HashSet::new();
            for a in &c.algorithms {
                if !seen.insert(a) {
                    return Err(ControlError::InvalidRoute {
                        path_prefix: r.matcher.path_prefix.clone(),
                        reason: format!("compression.algorithms lists `{}` twice", a.token()),
                    });
                }
            }
            if c.content_types.is_empty() {
                return Err(ControlError::InvalidRoute {
                    path_prefix: r.matcher.path_prefix.clone(),
                    reason: "compression.content_types must be non-empty".to_string(),
                });
            }
            for ct in &c.content_types {
                let essence = ct.trim();
                let well_formed = essence
                    .split_once('/')
                    .is_some_and(|(t, s)| !t.is_empty() && !s.is_empty())
                    && !essence.contains([';', ',', ' ']);
                if !well_formed {
                    return Err(ControlError::InvalidRoute {
                        path_prefix: r.matcher.path_prefix.clone(),
                        reason: format!(
                            "compression.content_types entry `{essence}` is not a type/subtype"
                        ),
                    });
                }
            }
        }
        // Validate the declarative response headers (ADR 000100) before anything can serve them.
        if let Some(h) = &r.headers {
            validate_response_headers(h).map_err(|reason| ControlError::InvalidRoute {
                path_prefix: r.matcher.path_prefix.clone(),
                reason,
            })?;
        }
        // Validate the response-body inspection settings (ADR 000098) the same way — an unusable
        // cap or allowlist must stop startup / reload, not disarm inspection per response.
        if let Some(rb) = &r.response_body {
            validate_response_body(rb).map_err(|reason| ControlError::InvalidRoute {
                path_prefix: r.matcher.path_prefix.clone(),
                reason,
            })?;
        }
        validated.push(ValidatedRoute { route: r, targets });
    }
    Ok(validated)
}

/// The request attributes route matching reads (ADR 000034), borrowed so matching stays
/// allocation-free. `path` may carry `?query`; the query is split out only inside `select`.
pub(crate) struct RequestParts<'a> {
    pub(crate) authority: &'a str,
    pub(crate) path: &'a str,
    pub(crate) method: &'a str,
    pub(crate) headers: &'a [Header],
}

/// What [`crate::ConfigSnapshot::find_route`] hands the fast-path server: which route matched
/// (`index`, used to dispatch its chain) plus the data needed to forward — the weighted backends
/// (the server picks a group, then a healthy instance from it) and the optional prefix strip. Owned
/// / `Arc`-shared so it survives a move into `spawn_blocking`.
#[derive(Debug, Clone)]
pub struct RouteInfo {
    pub index: usize,
    pub(crate) backends: Arc<WeightedBackends>,
    /// `Arc<str>` so building this per-request `RouteInfo` from the compiled route (`find_route`,
    /// called on every request) is an atomic refcount bump, not a heap allocation.
    pub strip_prefix: Option<Arc<str>>,
    /// Whether this route has any filters (drives the header-side chain dispatch).
    pub has_filters: bool,
    /// Which body directions this route's chain reads (ADR 000038 / 000098). The fast path buffers
    /// a direction ONLY when its flag is set; a route whose filters export neither hook keeps the
    /// zero-copy streaming path in BOTH directions (the real fix for the body-tax, docs/servey).
    pub body_hooks: BodyHooks,
    /// This route's native rate limiter (ADR 000033), or `None` for unlimited. `Arc`-shared with the
    /// live config so the per-request `RouteInfo` consults the route's persistent buckets.
    pub(crate) rate_limit: Option<Arc<NativeRateLimit>>,
    /// This route's Upgrade opt-in (ADR 000048), or `None` for deny-by-default. The fast path
    /// tunnels only when the client's token is allowlisted here.
    pub upgrade: Option<Arc<UpgradeConfig>>,
    /// This route's compression opt-in (ADR 000074), or `None` for never-transform. The fast path
    /// negotiates and compresses AFTER the response chain, on the streamed body filters never see.
    pub compression: Option<Arc<CompressionConfig>>,
    /// This route's declared response headers (ADR 000100), or `None` for no declaration. The
    /// fast path stamps them on every response it answers for this route — after the chain, so no
    /// filter can drop them, and before compression, so the codec still owns the representation.
    pub response_headers: Option<Arc<ResponseHeaders>>,
    /// This route's `[route.timeouts]` overrides (ADR 000102). Read through
    /// [`RouteInfo::request_timeout`] / [`RouteInfo::overall_timeout`], which resolve them
    /// against the upstream group the request is actually being forwarded to.
    pub(crate) timeouts: TimeoutConfig,
    /// This route's response-body inspection settings (ADR 000098), or the strict defaults. Only
    /// consulted when `body_hooks.response` is set.
    pub response_body: Arc<ResponseBodyConfig>,
}

impl RouteInfo {
    /// Pick the upstream group to forward this request to from the route's weighted traffic split
    /// (ADR 000034): the next backend in the split order that has an eligible instance (renormalize
    /// over healthy), or `None` when no backend is eligible (the fast path then fails closed 503,
    /// the same no-healthy fault as a single upstream). Lock-free.
    pub fn pick_upstream(&self) -> Option<Arc<UpstreamGroup>> {
        self.backends.pick()
    }

    /// Apply this route's host-native prefix strip to the path the fast-path server forwards to
    /// the upstream. The chain already ran against the original path; this only affects what the
    /// upstream sees. No rule (or a non-matching path) leaves the path unchanged — borrowed, so
    /// the common no-strip case allocates nothing.
    pub fn rewrite_path<'a>(&self, path: &'a str) -> Cow<'a, str> {
        rewrite_path(path, self.strip_prefix.as_deref())
    }

    /// The per-try timeout for a request this route forwards to `group` (ADR 000102): the route's
    /// `[route.timeouts]` override when it declares one, else the upstream's own default. Bounds
    /// ONE attempt's time-to-response-headers; `Duration::ZERO` disables it.
    pub fn request_timeout(&self, group: &UpstreamGroup) -> Duration {
        self.timeouts.request_timeout(group.request_timeout())
    }

    /// The overall timeout for a request this route forwards to `group` (ADR 000102), resolved the
    /// same way. Bounds the whole transaction — every attempt plus the backoff between them;
    /// `Duration::ZERO` means no overall bound. The tighter of this and the per-try bound governs
    /// each attempt (ADR 000031), which the forward loop applies as it spends the budget.
    pub fn overall_timeout(&self, group: &UpstreamGroup) -> Duration {
        self.timeouts.overall_timeout(group.overall_timeout())
    }

    /// Consult this route's native rate limiter (ADR 000033) for one request, keyed on the
    /// connection `peer`. `Allow` when the route has no limiter or a token was available;
    /// `Limit { retry_after_ms }` when the bucket is empty (the fast path fails closed with 429).
    /// Called BEFORE the filter chain so a flood is shed without spending WASM CPU.
    pub fn check_rate_limit(&self, peer: IpAddr) -> RateLimitDecision {
        match &self.rate_limit {
            Some(rl) => rl.check(peer),
            None => RateLimitDecision::Allow,
        }
    }
}

/// Normalise an authority for host matching: drop any `:port` and a trailing dot, borrowed — no
/// allocation on the request path. (`example.COM:8443` → `example.COM`; case is handled at the
/// comparison via `eq_ignore_ascii_case` against the pre-lowered route host.) IPv6 literals in
/// brackets keep their colons inside `[...]`.
fn normalize_host(authority: &str) -> &str {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // `[::1]:8080` → `[::1]`
        match rest.split_once(']') {
            // In-bounds by construction (`authority = "[" + inner + "]" + rest`, ASCII
            // delimiters), but stay total anyway — this is the request hot path, and `get` keeps
            // it panic-free under the crate's `indexing_slicing` discipline without an `allow`.
            Some((inner, _)) => authority.get(..inner.len() + 2).unwrap_or(authority),
            None => authority,
        }
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    // Absolute-form hosts carry a trailing dot (`example.com.`); strip a single one so they match
    // the canonical `example.com` route and cannot silently slip to a wildcard route (/
    // CWE-644). IPv6 literals (`[...]`) never end in a dot, so this only affects DNS names.
    match host.strip_suffix('.') {
        Some(stripped) if !host.starts_with('[') => stripped,
        _ => host,
    }
}

/// Normalize a request target's PATH for routing and forwarding so the proxy and the origin agree
/// on it (CWE-22 Path Traversal / CWE-436 Interpretation Conflict). Per-route filter chains
/// are an access-control boundary, so route selection IS access control: a `..` segment that selects
/// a laxer route here but resolves to a stricter path at the upstream would bypass that route's
/// filters. The fast path normalizes once at ingress and then routes, runs the chain, and forwards
/// on the SAME normalized path, so the origin cannot re-derive a different path.
///
/// Returns the normalized `path[?query]`, or `None` to reject (the server fails closed with 400).
/// Policy: reject control bytes, backslash, and percent-encoded separators/dots (`%2e`/`%2f`/`%5c`,
/// ambiguous between front-end and back-end), then lexically remove `.`/`..` segments; a `..` that
/// escapes the root is rejected. The query string (after `?`) is preserved verbatim.
/// A path with no `.`/`..` segments — the overwhelming majority — is returned borrowed
/// (`Cow::Borrowed`), so the per-request common case allocates nothing.
pub fn normalize_path(target: &str) -> Option<Cow<'_, str>> {
    let (raw, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    // A non-origin target (asterisk-form, or no leading `/`) cannot fall under a `/`-prefixed
    // route, so it needs no traversal handling — pass it through unchanged.
    if !raw.starts_with('/') {
        return Some(Cow::Borrowed(target));
    }
    // Reject control bytes / backslash and percent-encoded separators or dots: an origin that
    // decodes `%2f`/`%2e`/`%5c` would re-derive a path different from the one we route on and
    // forward, re-opening the front-end/back-end normalization gap.
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) || raw.contains('\\') {
        return None;
    }
    if contains_encoded_separator(raw) {
        return None;
    }
    // No `.`/`..` segment → the lexical resolution below is the identity (split + join over `/`
    // reproduces the input byte-for-byte, empty segments included), so return the input borrowed.
    if !raw.split('/').any(|seg| seg == "." || seg == "..") {
        return Some(Cow::Borrowed(target));
    }
    // Lexically resolve `.` / `..` over `/`-separated segments. `out` always holds the leading ""
    // (root); a `..` that would pop past it escapes the root and is rejected (fail-closed).
    let mut out: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "." => {}
            ".." => {
                if out.len() <= 1 {
                    return None;
                }
                out.pop();
            }
            other => out.push(other),
        }
    }
    let mut norm = out.join("/");
    if norm.is_empty() {
        norm.push('/');
    }
    if let Some(q) = query {
        norm.push('?');
        norm.push_str(q);
    }
    Some(Cow::Owned(norm))
}

/// Does the path contain a percent-encoded separator or dot (`%2e`/`%2f`/`%5c`, any hex case)?
// `.windows(3)` guarantees each `w` has exactly 3 elements, so w[0..=2] are always in bounds.
#[allow(clippy::indexing_slicing)]
fn contains_encoded_separator(path: &str) -> bool {
    path.as_bytes().windows(3).any(|w| {
        w[0] == b'%'
            && matches!(
                (w[1], w[2]),
                (b'2', b'e' | b'E') | (b'2', b'f' | b'F') | (b'5', b'c' | b'C')
            )
    })
}

/// Does `path` fall under `prefix` on a `/` boundary? `/api` matches `/api` and `/api/x` but not
/// `/apix`; `/` matches everything. `path` is the BARE path — `select` strips the `?query` /
/// `#fragment` once per request (not once per route), so a bare prefix with a query
/// (`/search?q=x` under `/search`) still matches (review f000005 P1#1). Rewriting
/// (`rewrite_path`) keeps the query; this is purely the selection predicate.
fn path_under_prefix(prefix: &str, path: &str) -> bool {
    if !path.starts_with(prefix) {
        return false;
    }
    if path.len() == prefix.len() || prefix.ends_with('/') {
        return true;
    }
    path.as_bytes().get(prefix.len()) == Some(&b'/')
}

/// Does the request match every dimension this route specifies (ADR 000034)? All specified
/// dimensions are ANDed; an unspecified one is a wildcard. `host` is pre-normalised, `query` is the
/// already-split query string. Header name is matched case-insensitively and value exact; query name
/// is case-sensitive. Byte-/string-exact comparison never panics on untrusted input (no-panic tenet).
fn route_matches(
    r: &CompiledRoute,
    host: &str,
    path: &str,
    method: &str,
    headers: &[Header],
    query: &str,
) -> bool {
    if let Some(h) = &r.host
        && !h.eq_ignore_ascii_case(host)
    {
        return false;
    }
    if !path_under_prefix(&r.path_prefix, path) {
        return false;
    }
    if let Some(m) = &r.method
        && method != m
    {
        return false;
    }
    for (name, value) in &r.headers {
        // Match the FIRST header with this name (case-insensitive name); a later duplicate must not
        // flip the decision (CWE-436): the origin receives every copy and may read a different one,
        // so deciding on the first occurrence keeps our routing aligned with Gateway-API's
        // "first match entry decides" and mirrors the query rule below.
        match headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)) {
            Some(h) if h.value.as_slice() == value.as_bytes() => {}
            _ => return false,
        }
    }
    for (name, value) in &r.query {
        if !query_param_matches(query, name, value) {
            return false;
        }
    }
    true
}

/// Does the query string contain `name=value` exactly (ADR 000034)? Case-sensitive name. The FIRST
/// occurrence of `name` decides (Gateway-API: a repeated key matches its first value); a malformed
/// pair (no `=`) is skipped; an absent name is no-match.
fn query_param_matches(query: &str, name: &str, value: &str) -> bool {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == name
        {
            return v == value;
        }
    }
    false
}

/// Select the best route for `req` (ADR 000013 / 000034): among all routes whose every specified
/// match dimension is satisfied, the most specific wins, ordered by host-constrained > longest
/// `path_prefix` > `method` present > more header matches > more query matches, with the earliest
/// manifest index the final stable tie-break (Gateway-API v1.5.0 precedence, adapted). Returns the
/// winner's index, or `None` (no route → the server responds 404).
pub(crate) fn select(routes: &[CompiledRoute], req: &RequestParts<'_>) -> Option<usize> {
    let host = normalize_host(req.authority);
    let query = req.path.split_once('?').map(|(_, q)| q).unwrap_or("");
    // Strip the query/fragment once here; `path_under_prefix` then compares bare paths per route.
    let path = req.path.split(['?', '#']).next().unwrap_or(req.path);
    routes
        .iter()
        .enumerate()
        .filter(|(_, r)| route_matches(r, host, path, req.method, req.headers, query))
        // Specificity key, most-significant first. `max_by_key` keeps the LAST max on ties, so the
        // final `usize::MAX - i` (earliest index largest) makes the earliest manifest route win.
        .max_by_key(|(i, r)| {
            (
                r.host.is_some(),
                r.path_prefix.len(),
                r.method.is_some(),
                r.headers.len(),
                r.query.len(),
                usize::MAX - i,
            )
        })
        .map(|(i, _)| i)
}

/// Apply a route's host-native prefix strip to the forwarded path (the chain already saw the
/// original). Leaves the path unchanged if it does not start with `strip`; always keeps a
/// leading `/`. `/api` stripped from `/api/users` → `/users`; from `/api` → `/`. The unchanged
/// and clean-strip cases are borrowed — no allocation on the request path.
///
/// The strip is SEGMENT-boundary strict, mirroring `path_under_prefix`'s discipline: `/api`
/// strips from `/api` and `/api/…` but NOT from `/apix/…` — a mid-segment strip would forward
/// `/apix/y` as `/x/y`, an origin-side path confusion (CWE-436-adjacent) whenever `strip_prefix`
/// is laxer than the route's `path_prefix`.
pub(crate) fn rewrite_path<'a>(path: &'a str, strip: Option<&str>) -> Cow<'a, str> {
    let Some(strip) = strip else {
        return Cow::Borrowed(path);
    };
    let Some(rest) = path.strip_prefix(strip) else {
        return Cow::Borrowed(path);
    };
    if rest.is_empty() {
        Cow::Borrowed("/")
    } else if rest.starts_with('/') {
        Cow::Borrowed(rest)
    } else if strip.ends_with('/') {
        // The strip consumed the boundary slash (`/api/` from `/api/users`): still a segment
        // match — re-add the leading `/`.
        Cow::Owned(format!("/{rest}"))
    } else {
        // Mid-segment "prefix" (`/api` vs `/apix/y`): not a path-segment match — don't strip.
        Cow::Borrowed(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        AddressSpec, CircuitBreaker, HealthConfig, LbAlgorithm, OutlierDetection, Upstream,
    };
    use crate::upstream::UpstreamRegistry;

    /// A throwaway upstream group named after `upstream` — these tests exercise `select` /
    /// `rewrite_path`, which never touch the group's contents, only its identity.
    fn group(upstream: &str) -> Arc<UpstreamGroup> {
        timed_group(upstream, 30_000, 0)
    }

    /// A throwaway upstream group declaring the per-try / overall timeout defaults a route's
    /// `[route.timeouts]` overrides resolve against (ADR 000102).
    fn timed_group(
        upstream: &str,
        request_timeout_ms: u64,
        overall_timeout_ms: u64,
    ) -> Arc<UpstreamGroup> {
        let reg = UpstreamRegistry::new();
        reg.reconcile(
            &[Upstream {
                name: upstream.to_string(),
                addresses: vec![AddressSpec::Bare("127.0.0.1:9000".to_string())],
                lb_algorithm: LbAlgorithm::RoundRobin,
                hash: None,
                tls: None,
                resolve_interval_ms: 0,
                health: HealthConfig {
                    path: "/healthz".to_string(),
                    interval_ms: 1000,
                    timeout_ms: 500,
                    healthy_threshold: 1,
                    unhealthy_threshold: 1,
                    port: None,
                },
                request_timeout_ms,
                max_retries: 1,
                overall_timeout_ms,
                circuit_breaker: CircuitBreaker::default(),
                outlier_detection: OutlierDetection::default(),
            }],
            std::path::Path::new("."),
        )
        .unwrap();
        reg.group(upstream).unwrap()
    }

    /// A single-upstream weighted set for a route under test.
    fn backends(upstream: &str) -> Arc<WeightedBackends> {
        Arc::new(WeightedBackends::new(vec![(group(upstream), 1)]).unwrap())
    }

    fn route(host: Option<&str>, prefix: &str, upstream: &str) -> CompiledRoute {
        CompiledRoute {
            host: host.map(|h| h.to_ascii_lowercase()),
            path_prefix: prefix.to_string(),
            method: None,
            headers: vec![],
            query: vec![],
            filters: vec![],
            resolved_chain: vec![],
            body_hooks: BodyHooks::NONE,
            backends: backends(upstream),
            strip_prefix: None,
            rate_limit: None,
            upgrade: None,
            compression: None,
            response_headers: None,
            timeouts: TimeoutConfig::default(),
            response_body: Arc::new(ResponseBodyConfig::default()),
        }
    }

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.to_string(),
            value: value.as_bytes().to_vec(),
        }
    }

    /// A GET request with no headers, for the path/host-only tests.
    fn parts<'a>(authority: &'a str, path: &'a str) -> RequestParts<'a> {
        RequestParts {
            authority,
            path,
            method: "GET",
            headers: &[],
        }
    }

    /// A manifest [`Route`] fixture for `validate_routes` tests — no `Host` / OCI artifact store
    /// needed, since `validate_routes` never touches the loaded filters' contents, only their ids.
    fn manifest_route(
        upstream: Option<&str>,
        backends: Vec<crate::manifest::Backend>,
        filters: Vec<&str>,
        rate_limit: Option<crate::manifest::RouteRateLimit>,
    ) -> Route {
        Route {
            matcher: crate::manifest::RouteMatch {
                host: None,
                path_prefix: "/".to_string(),
                method: None,
                headers: Default::default(),
                query: Default::default(),
            },
            filters: filters.into_iter().map(str::to_string).collect(),
            upstream: upstream.map(str::to_string),
            backends,
            strip_prefix: None,
            rate_limit,
            upgrade: None,
            compression: None,
            headers: None,
            timeouts: None,
            response_body: None,
        }
    }

    /// A manifest route carrying a `[route.headers]` declaration, for the validation tests.
    fn route_with_headers(set: &[(&str, &str)], remove: &[&str]) -> Vec<Route> {
        let mut r = manifest_route(Some("real"), vec![], vec![], None);
        r.headers = Some(crate::manifest::RouteHeaders {
            set: set
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            remove: remove.iter().map(|n| (*n).to_string()).collect(),
        });
        vec![r]
    }

    #[test]
    fn validate_routes_rejects_unknown_upstream() {
        let routes = vec![manifest_route(Some("ghost"), vec![], vec![], None)];
        let filters = HashSet::new();
        let upstream_names: HashSet<&str> = ["real"].into_iter().collect();
        let err = validate_routes(&routes, &filters, &upstream_names).unwrap_err();
        assert!(matches!(
            err,
            ControlError::UnknownRouteUpstream { upstream, .. } if upstream == "ghost"
        ));
    }

    #[test]
    fn validate_routes_rejects_unknown_filter() {
        let routes = vec![manifest_route(Some("real"), vec![], vec!["missing"], None)];
        let filters = HashSet::new();
        let upstream_names: HashSet<&str> = ["real"].into_iter().collect();
        let err = validate_routes(&routes, &filters, &upstream_names).unwrap_err();
        assert!(matches!(
            err,
            ControlError::UnknownRouteFilter { filter, .. } if filter == "missing"
        ));
    }

    #[test]
    fn validate_routes_rejects_all_zero_backend_weight() {
        let routes = vec![manifest_route(
            None,
            vec![crate::manifest::Backend {
                upstream: "real".to_string(),
                weight: 0,
            }],
            vec![],
            None,
        )];
        let filters = HashSet::new();
        let upstream_names: HashSet<&str> = ["real"].into_iter().collect();
        assert!(matches!(
            validate_routes(&routes, &filters, &upstream_names),
            Err(ControlError::InvalidRoute { .. })
        ));
    }

    #[test]
    fn validate_routes_rejects_zero_rate_or_burst() {
        let filters = HashSet::new();
        let upstream_names: HashSet<&str> = ["real"].into_iter().collect();

        let zero_rate = vec![manifest_route(
            Some("real"),
            vec![],
            vec![],
            Some(crate::manifest::RouteRateLimit {
                rate: 0,
                burst: 5,
                key: Default::default(),
            }),
        )];
        assert!(matches!(
            validate_routes(&zero_rate, &filters, &upstream_names),
            Err(ControlError::InvalidRouteRateLimit { .. })
        ));

        let zero_burst = vec![manifest_route(
            Some("real"),
            vec![],
            vec![],
            Some(crate::manifest::RouteRateLimit {
                rate: 5,
                burst: 0,
                key: Default::default(),
            }),
        )];
        assert!(matches!(
            validate_routes(&zero_burst, &filters, &upstream_names),
            Err(ControlError::InvalidRouteRateLimit { .. })
        ));
    }

    #[test]
    fn validate_routes_rejects_h2c_and_empty_upgrade_tokens() {
        // ADR 000048: `h2c` must never be tunnelable (h2 is TLS+ALPN only, ADR 000015; a
        // forwarded `Upgrade: h2c` is the classic smuggling vector), and an empty allowlist or
        // token is a config typo — both fail closed at build.
        let filters = HashSet::new();
        let upstream_names: HashSet<&str> = ["real"].into_iter().collect();
        let with_upgrade = |protocols: Vec<&str>| {
            let mut r = manifest_route(Some("real"), vec![], vec![], None);
            r.upgrade = Some(crate::manifest::RouteUpgrade {
                protocols: protocols.into_iter().map(str::to_string).collect(),
                idle_timeout_ms: 300_000,
            });
            vec![r]
        };

        for bad in [vec![], vec!["H2C"], vec!["websocket", "h2c"], vec!["  "]] {
            assert!(
                matches!(
                    validate_routes(&with_upgrade(bad.clone()), &filters, &upstream_names),
                    Err(ControlError::InvalidRoute { .. })
                ),
                "{bad:?} must be rejected"
            );
        }
        assert!(
            validate_routes(&with_upgrade(vec!["websocket"]), &filters, &upstream_names).is_ok()
        );
    }

    #[test]
    fn upgrade_config_matches_tokens_case_insensitively_and_ignores_unlisted() {
        let cfg = UpgradeConfig::new(&["WebSocket".to_string()], 300_000);
        assert_eq!(cfg.allowed_token("websocket"), Some("websocket"));
        assert_eq!(cfg.allowed_token("WEBSOCKET"), Some("websocket"));
        assert_eq!(
            cfg.allowed_token("h2c, websocket"),
            Some("websocket"),
            "the first ALLOWLISTED token wins; unlisted ones are skipped, never forwarded"
        );
        assert_eq!(cfg.allowed_token("h2c"), None);
        assert_eq!(cfg.allowed_token(""), None);

        assert_eq!(
            UpgradeConfig::new(&["websocket".to_string()], 0).idle_timeout(),
            None,
            "0 disables the idle timer"
        );
    }

    #[test]
    fn validate_routes_rejects_unusable_declared_response_headers() {
        // ADR 000100: the declaration is applied to every response the route answers, so a name
        // or value that is not valid HTTP must fail the build — never be dropped silently at
        // request time. Framing / connection-management names are refused outright: the host owns
        // framing (ADR 000073 review), and a manifest-set `content-length` is a response-desync
        // primitive (CWE-444).
        let filters = HashSet::new();
        let upstream_names: HashSet<&str> = ["real"].into_iter().collect();
        let rejected = |set: &[(&str, &str)], remove: &[&str]| {
            matches!(
                validate_routes(&route_with_headers(set, remove), &filters, &upstream_names),
                Err(ControlError::InvalidRoute { .. })
            )
        };

        assert!(rejected(&[], &[]), "an empty block declares nothing");
        assert!(rejected(&[("x bad", "v")], &[]), "a non-token name");
        assert!(rejected(&[("", "v")], &[]), "an empty name");
        assert!(
            rejected(&[("x-ok", "line\r\ninjected")], &[]),
            "a CRLF-bearing value"
        );
        assert!(
            rejected(&[("content-length", "0")], &[]),
            "framing belongs to the host, not to a declaration"
        );
        assert!(
            rejected(&[("connection", "close")], &[]),
            "connection management is hop-by-hop"
        );
        assert!(
            rejected(&[("x-ok", "v")], &["transfer-encoding"]),
            "the same reservation applies to a removal"
        );
        assert!(
            rejected(&[("x-ok", "v")], &["bad name"]),
            "a name to remove"
        );
        assert!(
            rejected(&[("X-Dup", "a"), ("x-dup", "b")], &[]),
            "the same name declared twice is ambiguous"
        );

        assert!(
            validate_routes(
                &route_with_headers(&[("X-Frame-Options", "DENY")], &["server"]),
                &filters,
                &upstream_names
            )
            .is_ok(),
            "a well-formed declaration passes"
        );
    }

    fn route_with_response_body(rb: RouteResponseBody) -> Vec<Route> {
        let mut r = manifest_route(Some("real"), vec![], vec![], None);
        r.response_body = Some(rb);
        vec![r]
    }

    #[test]
    fn validate_routes_rejects_an_unusable_response_body_declaration() {
        // ADR 000098: the cap and the allowlist decide whether a route's declared inspection runs
        // at all, so an unusable value must stop startup / reload rather than quietly disarm the
        // filter the operator put on the route.
        let filters = HashSet::new();
        let upstream_names: HashSet<&str> = ["real"].into_iter().collect();
        let rejected = |rb: RouteResponseBody| {
            matches!(
                validate_routes(&route_with_response_body(rb), &filters, &upstream_names),
                Err(ControlError::InvalidRoute { .. })
            )
        };

        assert!(
            rejected(RouteResponseBody {
                max_bytes: 0,
                ..Default::default()
            }),
            "a zero cap could never hold a body"
        );
        assert!(
            rejected(RouteResponseBody {
                max_bytes: MAX_RESPONSE_BODY_CAP + 1,
                ..Default::default()
            }),
            "the response plane never buys more headroom than the request plane has"
        );
        assert!(
            rejected(RouteResponseBody {
                content_types: vec![],
                ..Default::default()
            }),
            "an empty allowlist arms nothing"
        );
        assert!(
            rejected(RouteResponseBody {
                content_types: vec!["application".to_string()],
                ..Default::default()
            }),
            "an entry that is not type/subtype"
        );
        assert!(
            rejected(RouteResponseBody {
                content_types: vec!["text/plain; charset=utf-8".to_string()],
                ..Default::default()
            }),
            "the allowlist matches the ESSENCE, so a parameter in the entry is a typo"
        );

        assert!(
            validate_routes(
                &route_with_response_body(RouteResponseBody {
                    max_bytes: MAX_RESPONSE_BODY_CAP,
                    ..Default::default()
                }),
                &filters,
                &upstream_names
            )
            .is_ok(),
            "the ceiling itself is allowed"
        );
    }

    #[test]
    fn the_compiled_allowlist_matches_the_essence_case_insensitively() {
        let cfg = ResponseBodyConfig::new(&RouteResponseBody {
            content_types: vec!["Application/JSON".to_string(), " text/plain ".to_string()],
            ..Default::default()
        });

        assert!(cfg.content_type_eligible("application/json"));
        assert!(cfg.content_type_eligible("APPLICATION/JSON"));
        assert!(cfg.content_type_eligible("text/plain"));
        assert!(!cfg.content_type_eligible("text/html"));
        assert!(!cfg.content_type_eligible(""), "no declared type, no scope");
    }

    #[test]
    fn an_absent_response_body_block_compiles_to_the_strict_defaults() {
        // The no-block path and the declared-block path must not drift: a route that declares an
        // inspecting filter and nothing else runs under the same values the manifest documents.
        let cfg = ResponseBodyConfig::default();
        assert_eq!(cfg.max_bytes(), 1 << 20);
        assert_eq!(cfg.over_cap(), OverCapMode::Reject);
        assert_eq!(cfg.uninspectable(), UninspectableMode::Reject);
        assert!(cfg.content_type_eligible("application/json"));
        assert!(
            !cfg.content_type_eligible("text/event-stream"),
            "a streaming type is never in the allowlist"
        );
    }

    #[test]
    fn response_headers_remove_first_then_set_over_any_existing_value() {
        // The compiled declaration is a floor: `remove` drops what the upstream sent, `set`
        // replaces every same-named value (duplicates included) rather than appending beside it,
        // and the manifest's name casing is irrelevant.
        let declared = ResponseHeaders::new(&crate::manifest::RouteHeaders {
            set: [
                ("X-Frame-Options".to_string(), "DENY".to_string()),
                ("x-content-type-options".to_string(), "nosniff".to_string()),
            ]
            .into_iter()
            .collect(),
            remove: vec!["Server".to_string()],
        });

        let mut headers = HeaderMap::new();
        headers.insert("server", HeaderValue::from_static("upstream/1.0"));
        headers.append("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
        headers.append("x-frame-options", HeaderValue::from_static("ALLOWALL"));
        headers.insert("x-untouched", HeaderValue::from_static("1"));

        declared.apply(&mut headers);

        assert!(headers.get("server").is_none(), "a declared removal holds");
        assert_eq!(
            headers.get_all("x-frame-options").iter().count(),
            1,
            "the declaration replaces every same-named value, duplicates included"
        );
        assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(
            headers.get("x-untouched").unwrap(),
            "1",
            "headers the route never named are left alone"
        );
    }

    #[test]
    fn response_headers_set_wins_over_a_name_also_listed_for_removal() {
        // `remove` runs first, so a name in both lists ends up SET — the more specific
        // instruction, and the one an operator reading the block last would expect.
        let declared = ResponseHeaders::new(&crate::manifest::RouteHeaders {
            set: [("x-both".to_string(), "kept".to_string())]
                .into_iter()
                .collect(),
            remove: vec!["x-both".to_string()],
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-both", HeaderValue::from_static("from-upstream"));
        declared.apply(&mut headers);
        assert_eq!(headers.get("x-both").unwrap(), "kept");
    }

    /// A `RouteInfo` over one upstream group, shaped like the one `find_route` builds — only the
    /// timeout override under test is set.
    fn route_info(group: Arc<UpstreamGroup>, timeouts: TimeoutConfig) -> RouteInfo {
        RouteInfo {
            index: 0,
            backends: Arc::new(WeightedBackends::new(vec![(group, 1)]).unwrap()),
            strip_prefix: None,
            has_filters: false,
            body_hooks: BodyHooks::NONE,
            rate_limit: None,
            upgrade: None,
            compression: None,
            response_headers: None,
            timeouts,
            response_body: Arc::new(ResponseBodyConfig::default()),
        }
    }

    #[test]
    fn a_route_overrides_its_upstreams_timeouts_and_an_explicit_zero_disables_them() {
        // ADR 000102: one rule — the upstream declares the default, the route overrides it. The
        // upstream here declares both bounds, so "took the default" and "took the override" are
        // distinguishable for each knob.
        let upstream = timed_group("u", 30_000, 60_000);
        let declared = |request_timeout_ms, overall_timeout_ms| {
            TimeoutConfig::new(&RouteTimeouts {
                request_timeout_ms,
                overall_timeout_ms,
            })
        };

        let inherited = route_info(upstream.clone(), TimeoutConfig::default());
        assert_eq!(
            inherited.request_timeout(&upstream),
            Duration::from_secs(30),
            "no declaration takes the upstream's per-try default"
        );
        assert_eq!(
            inherited.overall_timeout(&upstream),
            Duration::from_secs(60),
            "no declaration takes the upstream's overall default"
        );

        let overridden = route_info(upstream.clone(), declared(Some(90_000), None));
        assert_eq!(
            overridden.request_timeout(&upstream),
            Duration::from_secs(90),
            "a declared per-try bound wins over the upstream's"
        );
        assert_eq!(
            overridden.overall_timeout(&upstream),
            Duration::from_secs(60),
            "the knob the route left out still takes the upstream's"
        );

        // An explicit `0` is the opt-out, not "undeclared": a streaming route writes it to shed a
        // short upstream default, so the two states must resolve differently.
        let streaming = route_info(upstream.clone(), declared(Some(0), Some(0)));
        assert_eq!(streaming.request_timeout(&upstream), Duration::ZERO);
        assert_eq!(streaming.overall_timeout(&upstream), Duration::ZERO);
    }

    #[test]
    fn compile_carries_the_manifest_timeout_overrides() {
        let mut r = manifest_route(Some("real"), vec![], vec![], None);
        r.timeouts = Some(RouteTimeouts {
            request_timeout_ms: Some(0),
            overall_timeout_ms: Some(1500),
        });
        let compiled = CompiledRoute::compile(
            &r,
            WeightedBackends::new(vec![(group("real"), 1)]).unwrap(),
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            compiled.timeouts.request_timeout(Duration::from_secs(30)),
            Duration::ZERO
        );
        assert_eq!(
            compiled.timeouts.overall_timeout(Duration::ZERO),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn validate_routes_accepts_a_valid_route_and_carries_its_resolved_targets() {
        let routes = vec![manifest_route(Some("real"), vec![], vec![], None)];
        let filters = HashSet::new();
        let upstream_names: HashSet<&str> = ["real"].into_iter().collect();
        let validated = validate_routes(&routes, &filters, &upstream_names).unwrap();
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].targets, vec![("real", 1)]);
    }

    #[test]
    fn longest_prefix_wins() {
        let routes = vec![
            route(None, "/", "root"),
            route(None, "/api", "api"),
            route(None, "/api/v2", "v2"),
        ];
        assert_eq!(select(&routes, &parts("h", "/api/v2/x")), Some(2));
        assert_eq!(select(&routes, &parts("h", "/api/users")), Some(1));
        assert_eq!(select(&routes, &parts("h", "/other")), Some(0));
    }

    #[test]
    fn prefix_matches_on_boundary_only() {
        let routes = vec![route(None, "/api", "api")];
        assert_eq!(select(&routes, &parts("h", "/api")), Some(0));
        assert_eq!(select(&routes, &parts("h", "/api/x")), Some(0));
        assert_eq!(
            select(&routes, &parts("h", "/apix")),
            None,
            "no boundary match"
        );
    }

    #[test]
    fn query_or_fragment_acts_as_a_prefix_boundary() {
        // review f000005 P1#1: the inbound path carries the query (`path_and_query`), so a bare
        // prefix followed by `?`/`#` must still match — `/search?q=foo` is under `/search` exactly
        // like `/search` and `/search/x` are. The earlier code treated `?` as path text and 404'd.
        let routes = vec![route(None, "/search", "s")];
        assert_eq!(select(&routes, &parts("h", "/search")), Some(0));
        assert_eq!(
            select(&routes, &parts("h", "/search?q=foo")),
            Some(0),
            "a bare prefix followed by a query must match"
        );
        assert_eq!(select(&routes, &parts("h", "/search/x?q=foo")), Some(0));
        assert_eq!(select(&routes, &parts("h", "/search#frag")), Some(0));
        // the boundary is still a real boundary: `/searching` is not under `/search`.
        assert_eq!(
            select(&routes, &parts("h", "/searching?q=foo")),
            None,
            "a longer word is not a boundary match even with a query"
        );
    }

    #[test]
    fn host_constraint_filters_and_breaks_ties() {
        let routes = vec![
            route(None, "/api", "wild"),
            route(Some("example.com"), "/api", "vhost"),
        ];
        // request to example.com (with a port) prefers the host-constrained route
        assert_eq!(
            select(&routes, &parts("example.com:8443", "/api/x")),
            Some(1)
        );
        // a different host falls back to the wildcard
        assert_eq!(select(&routes, &parts("other.test", "/api/x")), Some(0));
    }

    #[test]
    fn no_match_returns_none() {
        let routes = vec![route(Some("a.test"), "/api", "u")];
        assert_eq!(select(&routes, &parts("b.test", "/api")), None);
        let empty: Vec<CompiledRoute> = vec![];
        assert_eq!(select(&empty, &parts("a", "/")), None);
    }

    #[test]
    fn method_match_filters_and_outranks_a_bare_path() {
        // Two routes on the same path: one bare, one method-constrained. A POST takes the
        // method-constrained route (more specific); a GET falls to the bare one (ADR 000034).
        let mut post = route(None, "/api", "writes");
        post.method = Some("POST".to_string());
        let routes = vec![route(None, "/api", "reads"), post];

        let mut p = parts("h", "/api/x");
        p.method = "POST";
        assert_eq!(select(&routes, &p), Some(1), "POST takes the method route");
        p.method = "GET";
        assert_eq!(select(&routes, &p), Some(0), "GET falls to the bare route");
    }

    #[test]
    fn header_match_is_case_insensitive_name_exact_value_and_anded() {
        // A header-constrained route matches only when the request carries every named header with
        // the exact value; the header NAME is case-insensitive, the VALUE exact (ADR 000034).
        let mut v2 = route(None, "/api", "v2");
        v2.headers = vec![("x-api-version".to_string(), "2".to_string())];
        let routes = vec![route(None, "/api", "v1"), v2];

        let hdrs = [header("X-Api-Version", "2")];
        let mut p = parts("h", "/api");
        p.headers = &hdrs;
        assert_eq!(
            select(&routes, &p),
            Some(1),
            "a case-different header name still matches, and outranks the bare route"
        );

        let wrong = [header("x-api-version", "3")];
        p.headers = &wrong;
        assert_eq!(
            select(&routes, &p),
            Some(0),
            "a different value does not match"
        );

        p.headers = &[];
        assert_eq!(select(&routes, &p), Some(0), "absent header → bare route");
    }

    #[test]
    fn header_match_decides_on_the_first_duplicate() {
        // A duplicate header must not let a later copy flip the routing decision (CWE-436): the
        // FIRST occurrence of the name decides, like the query rule. Here the first `x-api-version`
        // is `1`, so the header route (which wants `2`) must NOT be selected even though a later
        // copy is `2` — otherwise our routing would disagree with an origin that reads the first.
        let mut v2 = route(None, "/api", "v2");
        v2.headers = vec![("x-api-version".to_string(), "2".to_string())];
        let routes = vec![route(None, "/api", "v1"), v2];

        let hdrs = [header("x-api-version", "1"), header("x-api-version", "2")];
        let mut p = parts("h", "/api");
        p.headers = &hdrs;
        assert_eq!(
            select(&routes, &p),
            Some(0),
            "the first duplicate value (1) decides, so the v2 header route does not match"
        );
    }

    #[test]
    fn query_match_is_case_sensitive_name_first_value_and_handles_malformed() {
        // Query name is case-sensitive (asymmetric with headers); the first occurrence's value
        // decides; a malformed (`=`-less) parameter is skipped (ADR 000034).
        let mut beta = route(None, "/api", "beta");
        beta.query = vec![("flag".to_string(), "on".to_string())];
        let routes = vec![route(None, "/api", "stable"), beta];

        assert_eq!(
            select(&routes, &parts("h", "/api?flag=on")),
            Some(1),
            "exact query value matches"
        );
        assert_eq!(
            select(&routes, &parts("h", "/api?flag=off")),
            Some(0),
            "wrong value → bare route"
        );
        assert_eq!(
            select(&routes, &parts("h", "/api?Flag=on")),
            Some(0),
            "query name is case-sensitive"
        );
        assert_eq!(
            select(&routes, &parts("h", "/api?flag=on&flag=off")),
            Some(1),
            "the first occurrence of a repeated key decides"
        );
        assert_eq!(
            select(&routes, &parts("h", "/api?flag&x=1")),
            Some(0),
            "a malformed (=-less) parameter is skipped, so the constraint is unmet"
        );
    }

    #[test]
    fn precedence_orders_method_above_header_count() {
        // Gateway-API precedence (ADR 000034 1b): method-present outranks a larger header count.
        // Route A: 2 header matches, no method. Route B: 1 header match + a method. A POST that
        // satisfies BOTH must take B (method beats header count), not A.
        let mut a = route(None, "/api", "a");
        a.headers = vec![
            ("h1".to_string(), "1".to_string()),
            ("h2".to_string(), "2".to_string()),
        ];
        let mut b = route(None, "/api", "b");
        b.headers = vec![("h1".to_string(), "1".to_string())];
        b.method = Some("POST".to_string());
        let routes = vec![a, b];

        let hdrs = [header("h1", "1"), header("h2", "2")];
        let mut p = parts("h", "/api");
        p.method = "POST";
        p.headers = &hdrs;
        assert_eq!(
            select(&routes, &p),
            Some(1),
            "method-present outranks the larger header count"
        );
    }

    #[test]
    fn strip_prefix_keeps_leading_slash() {
        assert_eq!(rewrite_path("/api/users", Some("/api")), "/users");
        assert_eq!(rewrite_path("/api", Some("/api")), "/");
        assert_eq!(rewrite_path("/api/", Some("/api")), "/");
        assert_eq!(rewrite_path("/other", Some("/api")), "/other", "no strip");
        assert_eq!(rewrite_path("/api/x", None), "/api/x", "no rule");
    }

    #[test]
    fn strip_prefix_is_segment_boundary_strict() {
        // Regression (large-review finding): `/api` must NOT strip mid-segment from `/apix/y` —
        // forwarding it as `/x/y` would be an origin-side path confusion (CWE-436-adjacent),
        // inconsistent with `path_under_prefix`'s boundary discipline.
        assert_eq!(
            rewrite_path("/apix/y", Some("/api")),
            "/apix/y",
            "a mid-segment match must not strip"
        );
        // A strip ending in `/` consumed the boundary slash — still a segment match.
        assert_eq!(rewrite_path("/api/users", Some("/api/")), "/users");
    }

    #[test]
    fn normalize_host_drops_port_and_preserves_case() {
        // Host matching is case-insensitive and port-insensitive (CWE-644: the routing decision
        // must not be steered by case tricks or a `:port` suffix a client appended). Case is left
        // as-is here — the comparison in `route_matches` is `eq_ignore_ascii_case`, so the request
        // path stays allocation-free.
        assert_eq!(normalize_host("EXAMPLE.com:8443"), "EXAMPLE.com");
        assert_eq!(normalize_host("Example.Com"), "Example.Com");
        assert_eq!(normalize_host("host:80"), "host");
    }

    #[test]
    fn normalize_host_handles_ipv6_literals() {
        // IPv6 literals keep the colons inside `[...]`; only a trailing `:port` is dropped.
        assert_eq!(normalize_host("[::1]:8080"), "[::1]");
        assert_eq!(normalize_host("[::1]"), "[::1]");
        assert_eq!(normalize_host("[2001:DB8::1]:443"), "[2001:DB8::1]");
    }

    #[test]
    fn normalize_host_is_panic_free_on_malformed_authority() {
        // A malformed authority (an unclosed bracket, a lone bracket, an empty string) must not
        // panic the data plane on the bracket-slice arithmetic — it just yields a non-matching
        // host. The fast path normalises EVERY inbound authority, so a single OOB slice here
        // would be a remote DoS.
        assert_eq!(
            normalize_host("[::1"),
            "[::1",
            "unclosed bracket returned as-is"
        );
        assert_eq!(normalize_host("["), "[", "lone bracket does not panic");
        assert_eq!(
            normalize_host("[]"),
            "[]",
            "empty bracket pair does not panic"
        );
        assert_eq!(normalize_host(""), "", "empty authority does not panic");
        assert_eq!(normalize_host("[]:9"), "[]");
    }

    #[test]
    fn normalize_host_strips_trailing_dot() {
        // / CWE-644: an absolute-form host (`example.com.`) must canonicalise to
        // `example.com` so it matches the host-constrained route instead of slipping to a wildcard.
        assert_eq!(normalize_host("example.com."), "example.com");
        assert_eq!(normalize_host("EXAMPLE.COM.:8443"), "EXAMPLE.COM");
        assert_eq!(normalize_host("example.com"), "example.com");
        // an IPv6 literal is unaffected (never ends in a dot).
        assert_eq!(normalize_host("[::1]"), "[::1]");
    }

    #[test]
    fn normalize_path_resolves_dot_segments_and_preserves_query() {
        assert_eq!(
            normalize_path("/public/../admin").as_deref(),
            Some("/admin")
        );
        assert_eq!(normalize_path("/a/./b").as_deref(), Some("/a/b"));
        assert_eq!(normalize_path("/a/b/../c").as_deref(), Some("/a/c"));
        assert_eq!(normalize_path("/").as_deref(), Some("/"));
        assert_eq!(normalize_path("/api/").as_deref(), Some("/api/"));
        assert_eq!(normalize_path("/api").as_deref(), Some("/api"));
        // the query is preserved verbatim, dot-resolution applies to the path only.
        assert_eq!(
            normalize_path("/x/../y?a=../b").as_deref(),
            Some("/y?a=../b")
        );
        // a non-origin target (asterisk-form) is passed through.
        assert_eq!(normalize_path("*").as_deref(), Some("*"));
    }

    #[test]
    fn normalize_path_rejects_traversal_and_ambiguous_encodings() {
        // root escape → reject (fail-closed).
        assert_eq!(normalize_path("/.."), None);
        assert_eq!(normalize_path("/a/../.."), None);
        // percent-encoded separators/dots are ambiguous between front-end and back-end → reject.
        assert_eq!(normalize_path("/public/%2e%2e/admin"), None);
        assert_eq!(normalize_path("/x%2fy"), None);
        assert_eq!(normalize_path("/x%2Fy"), None);
        assert_eq!(normalize_path("/x%5cy"), None);
        // backslash and control bytes → reject.
        assert_eq!(normalize_path("/a\\b"), None);
        assert_eq!(normalize_path("/a\nb"), None);
    }

    #[test]
    fn normalize_path_closes_per_route_filter_bypass() {
        // a request crafted to select the laxer `/public` route while resolving to
        // `/public/admin` at the upstream must, after ingress normalization, select the stricter
        // `/public/admin` route (which carries the auth filter) — no bypass.
        let routes = vec![
            route(None, "/public", "pub"),
            route(None, "/public/admin", "admin"),
        ];
        let raw = "/public/x/../admin";
        let norm = normalize_path(raw).expect("normalizes to a clean path");
        assert_eq!(norm, "/public/admin");
        assert_eq!(
            select(&routes, &parts("h", &norm)),
            Some(1),
            "the normalized path selects the stricter, filtered route"
        );
        // (the un-normalized path would have selected the laxer route 0 — the bug.)
        assert_eq!(select(&routes, &parts("h", raw)), Some(0));
    }
}
