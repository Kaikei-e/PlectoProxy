//! A named upstream the fast-path server forwards to (`[[upstream]]`, ADR 000013 / 000017 /
//! 000035 / 000042), plus its load-balancing, health, circuit-breaker, and outlier-detection
//! sub-config.

use serde::{Deserialize, Serialize};

/// A named upstream the fast-path server forwards a matched request to (ADR 000013 / 000017).
/// One or more `addresses` (`host:port`, no scheme) are the upstream's instances; the fast path
/// balances across the healthy ones per `lb_algorithm`. The forward leg is plain HTTP/1.1 unless
/// `[upstream.tls]` is declared, which re-encrypts with rustls and lets ALPN negotiate h2 /
/// http/1.1 (ADR 000042). Every upstream carries an active-health-check policy (`health`) —
/// required, because instances start pessimistic (unhealthy) and only a passing probe puts one
/// into rotation (ADR 000017).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    pub name: String,
    /// Each backend instance's `host:port` (no scheme — `[upstream.tls]` decides the scheme for
    /// the whole upstream, ADR 000042). Each entry is either a bare string (`"127.0.0.1:9000"`,
    /// weight 1) or a weighted inline table (`{ address = "...", weight = N }`) for heterogeneous
    /// instances (ADR 000035). Must be non-empty (validated at build). The two forms mix in one
    /// list; a bare string and an explicit `weight = 1` are equivalent (same content hash).
    pub addresses: Vec<AddressSpec>,
    /// `[upstream.tls]` (ADR 000042): when present, the forward leg to every instance re-encrypts
    /// with TLS and ALPN negotiates the protocol (h2 preferred, http/1.1 fallback). Absent = plain
    /// HTTP/1.1 (the pre-000042 behaviour). There is no insecure/verify-off escape hatch —
    /// verification failure keeps the instance out of rotation (fail-closed).
    #[serde(default)]
    pub tls: Option<UpstreamTls>,
    /// Per-upstream load-balancing algorithm (ADR 000035): `round_robin` (default), `least_request`
    /// (power-of-two-choices over per-instance active requests), or `maglev` (consistent hashing for
    /// session affinity). The chosen algorithm selects an instance from the healthy set; default
    /// `round_robin` keeps the pre-000035 behaviour with no per-request cost change.
    #[serde(default)]
    pub lb_algorithm: LbAlgorithm,
    /// Maglev hash config (ADR 000035), `[upstream.hash]`. Required iff `lb_algorithm = "maglev"`
    /// (a `maglev` upstream needs a hash key; any other algorithm must not set it). Absent otherwise.
    #[serde(default)]
    pub hash: Option<HashConfig>,
    /// Active-health-check policy for this upstream's instances (ADR 000017).
    pub health: HealthConfig,
    /// How often (ms) hostname addresses are re-resolved and the endpoint set refreshed — the
    /// standard periodic-DNS endpoint-discovery technique (the shape of nginx `resolve` / Envoy
    /// STRICT_DNS): each A/AAAA record becomes a load-balancing endpoint with its own health;
    /// a vanished record is dropped, a new one starts pessimistic (ADR 000017); a failed
    /// resolution keeps the last-known-good set. Interval-based (getaddrinfo carries no TTL) —
    /// pick a value at or below your DNS TTL. `0` disables (the default): hostnames still
    /// resolve per connect, but the endpoint set stays as declared.
    #[serde(default)]
    pub resolve_interval_ms: u64,
    /// End-to-end timeout (ms) for forwarding a request to this upstream (ADR 000019 / review
    /// f000005 P2#4). Bounds time-to-response-headers; once headers arrive the body streams without
    /// a deadline, so streaming responses are unaffected. Exceeding it fails closed with **504**.
    /// Default 30000; **`0` disables** the timeout (for long-poll / streaming upstreams).
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Maximum times the fast path re-sends this upstream's request to a DIFFERENT healthy instance
    /// after a retryable failure (ADR 000023). A timeout is retried only for an idempotent method
    /// (the upstream may have acted); a connect failure — the upstream never received the request —
    /// is retried for any method. Only bodyless requests are retried (the opaque streamed body,
    /// ADR 000013, can't be replayed). Default 1; **`0` disables** retry.
    #[serde(default = "default_max_retries")]
    pub max_retries: u64,
    /// Overall request timeout (ms) bounding the WHOLE transaction — every attempt PLUS the backoff
    /// between them (ADR 000031) — whereas `request_timeout_ms` bounds a single attempt (per-try).
    /// This is the end-to-end deadline across retries; exceeding it fails closed **504**
    /// (`x-plecto-fault: request-timeout`, distinct from the per-try `upstream-timeout`). Should be
    /// `>= request_timeout_ms`; the runtime applies the tighter of the two regardless. Default `0` =
    /// no overall bound (only the per-try timeout applies — the pre-000031 behaviour).
    #[serde(default)]
    pub overall_timeout_ms: u64,
    /// Per-upstream circuit breaker (ADR 000028): bounds the load the fast path puts on this
    /// upstream. Distinct from health — `health` ejects *failing* instances, this caps concurrent
    /// work on *healthy* ones so a saturated backend sheds load fast instead of queueing unbounded.
    /// Absent = unlimited (the default).
    #[serde(default)]
    pub circuit_breaker: CircuitBreaker,
    /// Per-upstream outlier detection (ADR 000032): eject an instance from rotation when it
    /// MISBEHAVES on live traffic (gateway-class 5xx), a third resilience axis distinct from active
    /// health ("is it reachable?") and the circuit breaker ("is it saturated?"). Absent / threshold
    /// `0` = disabled (the default).
    #[serde(default)]
    pub outlier_detection: OutlierDetection,
}

/// Upstream TLS re-encryption config (`[upstream.tls]`, ADR 000042). Presence of the section
/// enables TLS to every instance of the upstream; server certificate verification is ALWAYS on
/// (no insecure option — custom CA covers the self-signed / internal-CA case, fail-closed).
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTls {
    /// Manifest-relative path to a PEM bundle of CA certificates that REPLACES the webpki
    /// (Mozilla) roots for this upstream — the internal-CA / self-signed deployment shape.
    /// `None` = verify against the webpki roots. Only the path rides the content hash, like
    /// `[[tls]]` cert paths (ADR 000014).
    #[serde(default)]
    pub ca_path: Option<String>,
    /// Verification-name override (ADR 000050): when set, every TLS leg to this upstream uses
    /// `sni` for BOTH the SNI extension and certificate-name verification, instead of deriving it
    /// from the connected address. Required to make an IP-literal `addresses` entry (or a
    /// `resolve_interval_ms`-expanded one, ADR 000044) verify against a normal hostname
    /// certificate: an IP endpoint sends no SNI and is verified against its bare IP by default,
    /// which fails unless the certificate carries an IP SAN. `None` = derive from the address
    /// (the pre-000050 behaviour). Only the name rides the content hash, like `ca_path`.
    #[serde(default)]
    pub sni: Option<String>,
}

/// Per-upstream load-balancing algorithm (ADR 000035). `round_robin` is the default and keeps the
/// pre-000035 healthy-set rotation (ADR 000024); the others are opt-in. This selects an INSTANCE
/// within a chosen upstream group — a layer below the route→group weighted split (ADR 000034).
#[derive(
    Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum LbAlgorithm {
    /// Healthy-set round-robin (ADR 000017 / 000024) — the default.
    #[default]
    RoundRobin,
    /// Weighted least-request via power-of-two-choices: sample two healthy instances, forward to the
    /// one with the smaller `(active_requests + 1) / weight` (ADR 000035).
    LeastRequest,
    /// Consistent hashing via a (weighted) Maglev lookup table: a request's hash key maps to a
    /// stable instance for session affinity / cache locality (ADR 000035). Needs `[upstream.hash]`.
    Maglev,
}

/// One instance of an upstream (ADR 000035): a bare `host:port` string (weight 1) or a weighted
/// inline table `{ address = "host:port", weight = N }`. The bare form preserves the pre-000035
/// `addresses = ["h:p", ...]` manifest verbatim. A custom `Serialize` canonicalises a `weight = 1`
/// table back to the bare string, so an explicitly-written default weight does not change the
/// content hash (the manifest determinism invariant — same spirit as an explicit `isolation =
/// "untrusted"`).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum AddressSpec {
    /// A bare `host:port` — weight 1.
    Bare(String),
    /// A weighted instance `{ address, weight }`.
    Weighted(WeightedAddress),
}

/// The weighted form of an [`AddressSpec`]: an instance `host:port` plus its integer `weight`
/// (ADR 000035). Weight biases both the least-request comparison and the Maglev table share toward
/// higher-capacity instances; `1` is the default, `0` is rejected at build (drain an instance by
/// removing its address, not by zeroing its weight).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WeightedAddress {
    pub address: String,
    #[serde(default = "default_instance_weight")]
    pub weight: u32,
}

impl AddressSpec {
    /// This instance's `host:port`.
    pub fn address(&self) -> &str {
        match self {
            AddressSpec::Bare(a) => a,
            AddressSpec::Weighted(w) => &w.address,
        }
    }

    /// This instance's load-balancing weight (bare form is 1).
    pub fn weight(&self) -> u32 {
        match self {
            AddressSpec::Bare(_) => default_instance_weight(),
            AddressSpec::Weighted(w) => w.weight,
        }
    }
}

impl Serialize for AddressSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Canonicalise a default `weight = 1` (whether bare or written-out) to the bare-string form
        // so it does not perturb the semantic content hash (manifest determinism invariant, like an
        // explicit `isolation = "untrusted"`). A non-default weight serialises as the table.
        match self {
            AddressSpec::Bare(addr) => serializer.serialize_str(addr),
            AddressSpec::Weighted(w) if w.weight == default_instance_weight() => {
                serializer.serialize_str(&w.address)
            }
            AddressSpec::Weighted(w) => w.serialize(serializer),
        }
    }
}

fn default_instance_weight() -> u32 {
    1
}

/// Upper bound on a per-instance weight (ADR 000035). Keeps the least-request cross-product and the
/// Maglev populate (which interleaves a backend every `max_weight / weight` rounds) bounded; mirrors
/// Google's documented per-instance weight range (0–1000) for weighted Maglev. Drain via address
/// removal, not weight 0, so the floor is 1.
pub(crate) const MAX_INSTANCE_WEIGHT: u32 = 1000;

/// The Maglev consistent-hashing config (ADR 000035), `[upstream.hash]`. Only valid when
/// `lb_algorithm = "maglev"`. Names the request attribute hashed for affinity and the lookup-table
/// size.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HashConfig {
    /// Which request attribute is the hash key.
    pub key: HashKeyKind,
    /// The header name to hash, required iff `key = "header"` (matched case-insensitively). Ignored
    /// for `source_ip`.
    #[serde(default)]
    pub header: Option<String>,
    /// Maglev lookup-table size `M` — must be PRIME (the permutation's `skip` is coprime to `M` only
    /// then) and `>= instance count`. Default 65537. Larger `M` reduces disruption on instance
    /// change at the cost of `M × 2` bytes per upstream; validated prime / in range at build.
    #[serde(default = "default_hash_table_size")]
    pub table_size: u32,
}

/// The request attribute a Maglev upstream hashes for affinity (ADR 000035).
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HashKeyKind {
    /// The value of a named request header (requires `header`).
    Header,
    /// The connection peer's IP address (NOT a spoofable forwarding header).
    SourceIp,
}

fn default_hash_table_size() -> u32 {
    65537
}

/// Upper bound on the Maglev `table_size` (ADR 000035), matching Envoy's documented cap. Bounds the
/// per-upstream table memory (`table_size × 2` bytes); the operator must opt into anything this
/// large explicitly. The default (65537) is two orders of magnitude below it.
pub(crate) const MAX_HASH_TABLE_SIZE: u32 = 5_000_011;

/// Per-upstream outlier-detection policy (ADR 000032). A THIRD resilience axis: active health (ADR
/// 000017) asks "is the instance reachable?", the circuit breaker (ADR 000028) "is the upstream
/// saturated?", and this "is the instance misbehaving on live traffic?". An ejected instance leaves
/// the rotation for a (backing-off) ejection window, independent of the health bit; a shed request
/// (circuit breaker) never feeds it, only real upstream 5xx responses do.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutlierDetection {
    /// Consecutive gateway-class 5xx (502/503/504) from live forwards to eject an instance; `0` =
    /// disabled (the default). Counts only real responses — a circuit-breaker 503 or a per-try
    /// timeout is not an instance-misbehaviour signal.
    #[serde(default)]
    pub consecutive_gateway_failures: u32,
    /// Base ejection duration (ms): an ejected instance is out of rotation this long, doubling per
    /// consecutive ejection up to a bounded cap, then auto-returns when the window expires (no probe
    /// needed). Default 30000.
    #[serde(default = "default_base_ejection_time_ms")]
    pub base_ejection_time_ms: u64,
    /// Max % of the pool that may be outlier-ejected at once. The rest stay in rotation even while
    /// failing — fail-closed must not become a self-inflicted total outage (ejecting every instance).
    /// Default 10.
    #[serde(default = "default_max_ejection_percent")]
    pub max_ejection_percent: u32,
}

impl Default for OutlierDetection {
    fn default() -> Self {
        Self {
            consecutive_gateway_failures: 0,
            base_ejection_time_ms: default_base_ejection_time_ms(),
            max_ejection_percent: default_max_ejection_percent(),
        }
    }
}

fn default_base_ejection_time_ms() -> u64 {
    30_000
}

fn default_max_ejection_percent() -> u32 {
    10
}

/// Per-upstream circuit-breaker thresholds (ADR 000028). Overload protection that is deliberately
/// SEPARATE from outlier detection / health (ADR 000017): health answers "is this instance up?",
/// the breaker answers "is this upstream saturated?". A request rejected by the breaker is the
/// upstream shedding load, not an instance failing — so it never demotes an instance.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitBreaker {
    /// Max concurrent in-flight requests the fast path will forward to this upstream at once. At the
    /// cap a new request fails closed with **503** (`x-plecto-fault: circuit-open`) rather than
    /// piling onto the backend. `0` = unlimited (the default). Counts one slot per request across
    /// the whole retry sequence, released when the upstream response headers arrive (or it fails).
    #[serde(default)]
    pub max_requests: u32,
}

/// Active-health-check policy (ADR 000017). A background prober issues `GET {path}` to each
/// instance every `interval_ms` — on the instance's own traffic port, unless `port` names a
/// dedicated health-check port — and a 2xx within `timeout_ms` is a success, anything else
/// (non-2xx, timeout, connect error) a failure. `unhealthy_threshold` consecutive failures eject a
/// healthy instance from the rotation; `healthy_threshold` consecutive successes restore an ejected
/// one. Instances start pessimistic (unhealthy); the FIRST successful probe alone promotes a
/// never-yet-healthy instance (cold-start fast path), after which the full `healthy_threshold`
/// governs re-entry. Only `path` is required; the rest default. `PartialEq` lets a reload detect a
/// changed policy and re-probe the upstream's instances from scratch (ADR 000017 reconcile).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    /// The probe request path, e.g. `/healthz`.
    pub path: String,
    /// Probe period in milliseconds (default 2000).
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    /// Per-probe timeout in milliseconds (default 1000).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Consecutive successes to restore an ejected instance (default 2). The first-ever promotion
    /// of a never-yet-healthy instance needs only one success, regardless of this value.
    #[serde(default = "default_healthy_threshold")]
    pub healthy_threshold: u32,
    /// Consecutive failures to eject a healthy instance (default 3).
    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
    /// Dedicated port the probe connects to, distinct from the instance's traffic port (e.g. a
    /// separate metrics/health listener on the same host). `None` (default) probes the traffic port
    /// itself, the pre-existing behaviour.
    #[serde(default)]
    pub port: Option<u16>,
}

fn default_request_timeout_ms() -> u64 {
    30_000
}

fn default_max_retries() -> u64 {
    1
}

fn default_interval_ms() -> u64 {
    2000
}
fn default_timeout_ms() -> u64 {
    1000
}
fn default_healthy_threshold() -> u32 {
    2
}
fn default_unhealthy_threshold() -> u32 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ControlError;
    use crate::manifest::Manifest;

    #[test]
    fn upstream_circuit_breaker_defaults_unlimited_and_parses() {
        // Absent `[upstream.circuit_breaker]` → unlimited (max_requests 0), the safe default.
        let m = Manifest::from_toml(
            r#"
[[upstream]]
name = "a"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"
"#,
        )
        .unwrap();
        assert_eq!(
            m.upstreams[0].circuit_breaker.max_requests, 0,
            "an absent breaker is unlimited"
        );

        // Present → the cap is read.
        let m2 = Manifest::from_toml(
            r#"
[[upstream]]
name = "a"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"
[upstream.circuit_breaker]
max_requests = 64
"#,
        )
        .unwrap();
        assert_eq!(m2.upstreams[0].circuit_breaker.max_requests, 64);
    }

    #[test]
    fn upstream_tls_sni_defaults_absent_and_parses() {
        // ADR 000050: absent `sni` = derive from the connected address (pre-000050 behaviour).
        let without = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"
[upstream.tls]
"#,
        )
        .unwrap();
        assert_eq!(without.upstreams[0].tls.as_ref().unwrap().sni, None);

        // Present → the override is read, and it flips the content hash (a real config change).
        let with = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"
[upstream.tls]
sni = "backend.internal"
"#,
        )
        .unwrap();
        assert_eq!(
            with.upstreams[0].tls.as_ref().unwrap().sni.as_deref(),
            Some("backend.internal")
        );
        assert_ne!(
            without.content_hash().unwrap(),
            with.content_hash().unwrap(),
            "declaring sni must flip the content hash"
        );
    }

    #[test]
    fn upstream_requires_health() {
        // health is mandatory for every upstream (ADR 000017): a missing `[upstream.health]`
        // table is rejected, since instances start pessimistic and need a probe to enter rotation.
        let parsed = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
"#,
        );
        assert!(
            parsed.is_err(),
            "an upstream without [upstream.health] is rejected"
        );
    }

    #[test]
    fn health_requires_path_but_defaults_the_rest() {
        // `path` is required; thresholds/interval/timeout default.
        let no_path = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
[upstream.health]
interval_ms = 500
"#,
        );
        assert!(no_path.is_err(), "health without a probe path is rejected");

        // explicit overrides ride through
        let m = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/up"
interval_ms = 250
timeout_ms = 100
healthy_threshold = 1
unhealthy_threshold = 5
"#,
        )
        .unwrap();
        let h = &m.upstreams[0].health;
        assert_eq!(h.interval_ms, 250);
        assert_eq!(h.timeout_ms, 100);
        assert_eq!(h.healthy_threshold, 1);
        assert_eq!(h.unhealthy_threshold, 5);
    }

    #[test]
    fn health_port_defaults_to_none_but_can_override_the_probe_port() {
        // absent `port` = probe the instance's own traffic port (pre-existing behaviour).
        let default_port = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"
"#,
        )
        .unwrap();
        assert_eq!(default_port.upstreams[0].health.port, None);

        // an explicit `port` names a dedicated health-check listener.
        let overridden = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"
port = 9100
"#,
        )
        .unwrap();
        assert_eq!(overridden.upstreams[0].health.port, Some(9100));
    }

    #[test]
    fn upstream_request_timeout_defaults_and_overrides() {
        // ADR 000019 / review f000005 P2#4: an upstream gets a 30s default end-to-end timeout when
        // unspecified; an explicit value (incl. 0 = disabled) rides through and flips the hash.
        let defaulted = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"
"#,
        )
        .unwrap();
        assert_eq!(
            defaulted.upstreams[0].request_timeout_ms, 30_000,
            "an unspecified upstream timeout defaults to 30s"
        );

        let overridden = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
request_timeout_ms = 250
[upstream.health]
path = "/healthz"
"#,
        )
        .unwrap();
        assert_eq!(overridden.upstreams[0].request_timeout_ms, 250);
        // a timeout change is a real config change → the content hash must flip.
        assert_ne!(
            defaulted.content_hash().unwrap(),
            overridden.content_hash().unwrap(),
            "changing the upstream timeout must flip the config version"
        );

        // `0` is an explicit opt-out (long-poll / streaming upstream) and must parse, not error.
        let disabled = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
request_timeout_ms = 0
[upstream.health]
path = "/healthz"
"#,
        )
        .unwrap();
        assert_eq!(
            disabled.upstreams[0].request_timeout_ms, 0,
            "request_timeout_ms = 0 disables the timeout and must parse"
        );
    }

    #[test]
    fn upstream_max_retries_defaults_and_overrides() {
        // ADR 000023: an upstream gets 1 retry by default; an explicit value (incl. 0 = disabled)
        // rides through and flips the content hash.
        let defaulted = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"
"#,
        )
        .unwrap();
        assert_eq!(
            defaulted.upstreams[0].max_retries, 1,
            "an unspecified upstream defaults to one retry"
        );

        let disabled = Manifest::from_toml(
            r#"
[[upstream]]
name = "x"
addresses = ["127.0.0.1:9000"]
max_retries = 0
[upstream.health]
path = "/healthz"
"#,
        )
        .unwrap();
        assert_eq!(disabled.upstreams[0].max_retries, 0, "0 disables retry");
        assert_ne!(
            defaulted.content_hash().unwrap(),
            disabled.content_hash().unwrap(),
            "changing max_retries must flip the config version"
        );
    }

    // ----- ADR 000035: lb_algorithm, per-instance weight, maglev hash config -----

    fn upstream_toml(body: &str) -> Result<Manifest, ControlError> {
        Manifest::from_toml(&format!(
            "[[upstream]]\nname = \"u\"\n{body}\n[upstream.health]\npath = \"/healthz\"\n"
        ))
    }

    #[test]
    fn lb_algorithm_defaults_round_robin_and_parses() {
        // Absent → round_robin (the pre-000035 default).
        let m = upstream_toml("addresses = [\"a:1\"]").unwrap();
        assert_eq!(m.upstreams[0].lb_algorithm, LbAlgorithm::RoundRobin);

        let lr = upstream_toml("addresses = [\"a:1\"]\nlb_algorithm = \"least_request\"").unwrap();
        assert_eq!(lr.upstreams[0].lb_algorithm, LbAlgorithm::LeastRequest);

        // a non-default algorithm flips the content hash (it is part of config identity).
        assert_ne!(
            m.content_hash().unwrap(),
            lr.content_hash().unwrap(),
            "changing lb_algorithm must flip the config version"
        );
    }

    #[test]
    fn addresses_parse_bare_and_weighted_mixed() {
        // A bare string and a weighted inline table coexist in one list (ADR 000035).
        let m = upstream_toml(
            "addresses = [\"a:1\", { address = \"b:2\", weight = 5 }, { address = \"c:3\" }]",
        )
        .unwrap();
        let a = &m.upstreams[0].addresses;
        assert_eq!(a[0].address(), "a:1");
        assert_eq!(a[0].weight(), 1, "bare = weight 1");
        assert_eq!(a[1].address(), "b:2");
        assert_eq!(a[1].weight(), 5);
        assert_eq!(a[2].address(), "c:3");
        assert_eq!(a[2].weight(), 1, "weighted form defaults weight to 1");
    }

    #[test]
    fn explicit_weight_one_hashes_like_a_bare_address() {
        // The determinism invariant: an explicit `weight = 1` is representation noise vs a bare
        // string, so the two must share a content hash (like an explicit `isolation = "untrusted"`).
        let bare = upstream_toml("addresses = [\"a:1\"]").unwrap();
        let explicit = upstream_toml("addresses = [{ address = \"a:1\", weight = 1 }]").unwrap();
        assert_eq!(
            bare.content_hash().unwrap(),
            explicit.content_hash().unwrap(),
            "an explicit default weight must not change the config version"
        );

        // …but a non-default weight DOES change it.
        let weighted = upstream_toml("addresses = [{ address = \"a:1\", weight = 3 }]").unwrap();
        assert_ne!(
            bare.content_hash().unwrap(),
            weighted.content_hash().unwrap()
        );
    }

    #[test]
    fn maglev_hash_config_parses() {
        let m = upstream_toml(
            "addresses = [\"a:1\", \"b:2\"]\nlb_algorithm = \"maglev\"\n[upstream.hash]\nkey = \"header\"\nheader = \"x-user\"\ntable_size = 1009",
        )
        .unwrap();
        let up = &m.upstreams[0];
        assert_eq!(up.lb_algorithm, LbAlgorithm::Maglev);
        let hash = up.hash.as_ref().unwrap();
        assert_eq!(hash.key, HashKeyKind::Header);
        assert_eq!(hash.header.as_deref(), Some("x-user"));
        assert_eq!(hash.table_size, 1009);
        assert!(up.validate_lb().is_ok());
    }

    #[test]
    fn hash_table_size_defaults_to_65537() {
        let m = upstream_toml(
            "addresses = [\"a:1\"]\nlb_algorithm = \"maglev\"\n[upstream.hash]\nkey = \"source_ip\"",
        )
        .unwrap();
        assert_eq!(m.upstreams[0].hash.as_ref().unwrap().table_size, 65537);
        assert_eq!(
            m.upstreams[0].hash.as_ref().unwrap().key,
            HashKeyKind::SourceIp
        );
    }

    #[test]
    fn validate_lb_rejects_bad_configs() {
        // weight 0 (drain via address removal, not weight 0)
        let w0 = upstream_toml("addresses = [{ address = \"a:1\", weight = 0 }]").unwrap();
        assert!(w0.upstreams[0].validate_lb().is_err(), "weight 0 rejected");

        // weight over the cap
        let wbig = upstream_toml(&format!(
            "addresses = [{{ address = \"a:1\", weight = {} }}]",
            MAX_INSTANCE_WEIGHT + 1
        ))
        .unwrap();
        assert!(
            wbig.upstreams[0].validate_lb().is_err(),
            "over-cap weight rejected"
        );

        // maglev without a hash block
        let no_hash = upstream_toml("addresses = [\"a:1\"]\nlb_algorithm = \"maglev\"").unwrap();
        assert!(
            no_hash.upstreams[0].validate_lb().is_err(),
            "maglev needs [upstream.hash]"
        );

        // hash block on a non-maglev algorithm
        let stray_hash =
            upstream_toml("addresses = [\"a:1\"]\n[upstream.hash]\nkey = \"source_ip\"").unwrap();
        assert!(
            stray_hash.upstreams[0].validate_lb().is_err(),
            "[upstream.hash] only valid with maglev"
        );

        // header key with no header name
        let no_header = upstream_toml(
            "addresses = [\"a:1\"]\nlb_algorithm = \"maglev\"\n[upstream.hash]\nkey = \"header\"",
        )
        .unwrap();
        assert!(
            no_header.upstreams[0].validate_lb().is_err(),
            "header key needs a name"
        );

        // non-prime table size
        let nonprime = upstream_toml(
            "addresses = [\"a:1\"]\nlb_algorithm = \"maglev\"\n[upstream.hash]\nkey = \"source_ip\"\ntable_size = 1000",
        )
        .unwrap();
        assert!(
            nonprime.upstreams[0].validate_lb().is_err(),
            "table_size must be prime"
        );

        // table smaller than the instance count
        let too_small = upstream_toml(
            "addresses = [\"a:1\", \"b:2\", \"c:3\"]\nlb_algorithm = \"maglev\"\n[upstream.hash]\nkey = \"source_ip\"\ntable_size = 2",
        )
        .unwrap();
        assert!(
            too_small.upstreams[0].validate_lb().is_err(),
            "M must be >= instance count"
        );

        // a valid maglev config passes
        let ok = upstream_toml(
            "addresses = [\"a:1\", \"b:2\"]\nlb_algorithm = \"maglev\"\n[upstream.hash]\nkey = \"source_ip\"\ntable_size = 97",
        )
        .unwrap();
        assert!(ok.upstreams[0].validate_lb().is_ok());
    }
}
