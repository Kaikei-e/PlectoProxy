//! plecto-control — the control plane (ADR 000007 / 000008).
//!
//! A single declarative TOML manifest pins filters by OCI digest and declares one chain. An
//! `ArtifactStore` resolves each filter from a local, offline OCI image-layout (remote
//! registry fetch via `wkg` is an out-of-band operator step, kept out of the runtime), the
//! `Host` loads it through the ADR 000006 provenance gate, and a chain dispatcher drives a
//! request through the loaded filters. `reload` rebuilds the set and swaps it **atomically**
//! (ArcSwap): new requests see the new set, in-flight holders keep the old one until it
//! drops. Single-node-first (ADR 000008).
//!
//! The trust policy lives on the `Host` and is fixed at construction; changing trust roots
//! requires a new `Control`. `reload` swaps only the filter set + chain (same `Host`, same
//! epoch ticker), so a runaway filter stays bounded across reloads.

// Hot-path discipline (bp-rust): no unwrap/expect/panic/indexing on the data plane. Exempted
// under `cfg(test)` — this crate's own `#[cfg(test)] mod` blocks legitimately use them;
// `tests/*.rs` integration tests are separate crates and are never subject to this attribute.
// plecto-control is config/build-time (not per-request), but its Maglev/weighted-split hashing
// and route matching are still touched by the fast path indirectly, so the same discipline
// applies (added at Stage 3, after the `hash.rs`/`maglev.rs`/`weighted.rs` fixes it surfaces).
//
// `clippy::pedantic`/`clippy::nursery` are NOT enabled here (nor in plecto-host/plecto-server):
// a dry run measures 400+ pre-existing hits crate-wide, almost entirely pre-existing stylistic
// noise unrelated to this refactor's scope; scoped-allow-ing all of them would be disproportionate
// busy-work. Left as a known, explicit gap rather than silently skipped.
// `rust_2018_idioms` (warn) and `unsafe_op_in_unsafe_fn` (deny) come from `[workspace.lints]`.
#![cfg_attr(
    not(test),
    warn(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod artifact;
mod chain;
mod control_observability;
mod control_reload;
mod diagnostic;
mod error;
mod hash;
mod maglev;
mod manifest;
pub mod oci;
mod ratelimit;
mod reload;
mod rng;
mod route;
mod snapshot;
mod stek;
mod tls;
mod upstream;
mod weighted;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use plecto_host::{LoadedFilter, SignedArtifact};

pub use artifact::{ArtifactStore, MemoryStore, ResolvedArtifact};
pub use chain::{ChainOutcome, RequestBodyOutcome, ResponseOutcome};
pub use diagnostic::{
    DEV_KEY_IN_TRUST, Diagnostic, PATH_NORMALIZATION_REJECTED, QUOTA_EXCEEDED,
    SIGNATURE_VERIFICATION_FAILED, diagnose, diagnosed_message,
};
pub use error::ControlError;
pub use manifest::{
    Chain, CircuitBreaker, CompressionAlgorithm, FilterEntry, HealthConfig, IsolationKind,
    Manifest, Observability, OutlierDetection, ProxyProtocolTrust, RateLimitKeyKind, Route,
    RouteCompression, RouteRateLimit, State, StateBackendKind, TlsCert, Trust, Upstream,
};
pub use ratelimit::RateLimitDecision;
#[cfg(unix)]
pub use reload::SignalReloadSource;
pub use reload::{ReloadOutcome, ReloadSource, serve_reloads};
pub use route::{CompressionConfig, RouteInfo, UpgradeConfig, normalize_path};
/// The rustls TLS client config the fast path re-encrypts upstream forward legs with
/// (ADR 000042), re-exported for the same reason as [`TlsServerConfig`].
pub use rustls::ClientConfig as TlsClientConfig;
/// The rustls TLS server config the fast path terminates with (ADR 000014), re-exported so
/// `plecto-server` names the same `rustls` type the control plane built.
pub use rustls::ServerConfig as TlsServerConfig;
pub use snapshot::ConfigSnapshot;
pub use upstream::{
    Endpoints, HashInput, HashKeySource, Pick, UpstreamGroup, UpstreamInstance, UpstreamRegistry,
};

// Re-export the host surface a caller drives the control plane with, so they need not depend
// on `plecto-host` directly for the common path — including the ADR 000009 observability
// types (build a `Host` with a sink, then drive snapshots that carry the trace context).
pub use plecto_host::{
    FanOutSink, FilterSpan, Header, Host, HttpRequest, HttpResponse, InMemorySink, MetricsSink,
    MetricsSnapshot, NoopSink, RequestTrace, SpanOutcome, TelemetrySink, TrustPolicy,
};
// Filter Dev Kit (ADR 000065): `plecto conformance` / `plecto dev` / `plecto new-filter` need
// the generic conformance battery and the persistent dev-signing key. Re-exported the same way
// as the rest of the host surface above — `plecto-server` never takes a direct `plecto-host`
// production dependency; a plain (non-`test-support`) `plecto-host` build needs no wasm32
// toolchain, so this widens no dependency edge, just this crate's existing re-export list.
pub use plecto_host::{
    ConformanceCheck, ConformanceReport, DEV_KEY_MARKER, DevKeyError, DevSigner, bound_sbom,
    public_key_path_for, run_conformance,
};
// The OTLP export surface (ADR 000040): the fast-path server drives the span buffer + the
// hand-written wire encoding through the control plane, without depending on `plecto-host`.
pub use plecto_host::otlp;

/// The atomically-swappable active configuration: the loaded filters, the chain order, and
/// the `content_hash` of the manifest that produced them. Held behind an `ArcSwap`; never
/// mutated in place — `reload` replaces it wholesale. The hash rides with the config it
/// describes so `reload_from_disk` can compare the running `config version` without a
/// separate lock.
pub(crate) struct ActiveConfig {
    pub(crate) filters: HashMap<String, Arc<LoadedFilter>>,
    /// The manifest's default `[chain]`, resolved to the loaded filter in order — built once per
    /// reload so the default-chain convenience (`ConfigSnapshot::on_request` / `on_response`)
    /// never re-hashes a filter id against `filters` on every request (mirrors
    /// `CompiledRoute::resolved_chain`).
    pub(crate) resolved_chain: Vec<Arc<LoadedFilter>>,
    /// Compiled routing table (ADR 000013): empty unless the manifest declares `[[route]]`.
    /// The fast-path server matches against these; the chain-only `on_request` ignores them.
    pub(crate) routes: Vec<route::CompiledRoute>,
    /// TLS server config built from `[[tls]]` (ADR 000014), or `None` for plain HTTP/1.1. Rides
    /// the `ArcSwap` with the rest, so a reload swaps certs atomically (new conns get new certs).
    pub(crate) tls: Option<Arc<rustls::ServerConfig>>,
    /// QUIC TLS server config for HTTP/3 (ADR 000016): ALPN `h3`, TLS 1.3, same SNI cert resolver
    /// as `tls`. `None` whenever `tls` is `None` (h3 requires TLS). Rides the same `ArcSwap`.
    pub(crate) quic_tls: Option<Arc<rustls::ServerConfig>>,
    pub(crate) hash: String,
}

/// The control plane: owns the `Host` (and thus the trust policy + epoch ticker) and the
/// artifact store, and holds the active filter set behind an `ArcSwap` for lock-free reads
/// and atomic reload. `manifest_path` is `Some` only when the plane was built from an on-disk
/// manifest — that is what `reload_from_disk` (and the SIGHUP loop) re-reads.
pub struct Control {
    host: Host,
    store: Box<dyn ArtifactStore>,
    active: ArcSwap<ActiveConfig>,
    /// Serializes reloads: `build_active` reconciles the shared `upstreams` registry in place and
    /// then stores `active`, so two interleaved reloads could leave routes holding groups the
    /// registry no longer probes (permanently pessimistic → 503). The shipped SIGHUP loop is
    /// single-threaded; this guard closes the hole for any other embedder of the public API.
    reload_gate: parking_lot::Mutex<()>,
    /// The upstream instances + their health state (ADR 000017). Lives OUTSIDE `active` so a
    /// reload's `build_active` reconciles it in place — health state survives the swap. The
    /// fast-path server reads it both via routing (`RouteInfo.upstream`, resolved at build time)
    /// and via `upstream_groups` (the health-check supervisor).
    upstreams: Arc<UpstreamRegistry>,
    manifest_path: Option<PathBuf>,
    /// The `[trust]` section the `Host` was built from, captured at construction. A reload that
    /// would change it is rejected (`TrustChangeRequiresRestart`) rather than silently dropped
    /// (f000004 #1): trust roots are fixed for the life of the `Host` / epoch ticker.
    trust: Trust,
    /// The `[state]` section the `Host`'s `KvBackend` was built from (ADR 000041), captured at
    /// construction. Same contract as `trust`: the backend lives for the life of the `Host`,
    /// so a reload that would change it is rejected (`StateChangeRequiresRestart`).
    state: manifest::State,
    /// Base directory the manifest's relative paths (filter `source`, TLS `cert_path`/`key_path`)
    /// resolve against (ADR 000014). Captured at construction so a reload re-reads certs from the
    /// same root. `"."` for the in-memory `load` core (tests use absolute cert paths).
    base_dir: PathBuf,
    /// Host-aggregated filter-execution metrics (ADR 000009): the `MetricsSink` wired into the
    /// `Host` at construction, snapshotted by the fast path's admin `/metrics` endpoint.
    filter_metrics: Arc<MetricsSink>,
    /// Operational observability config (`[observability]`, ADR 000009), captured at construction:
    /// the admin endpoint bind address and the access-log toggle. Not part of the config version.
    observability: Observability,
    /// The data-plane listener config (`[listen]`), captured at construction like
    /// `observability`: the listener binds once at startup, so a reload does not re-bind.
    listen: manifest::Listen,
    /// The parsed `[listen.proxy_protocol]` trust (ADR 000057), captured at construction like
    /// `listen` itself: the TCP listener consults it once at startup, so a reload does not
    /// change it. `None` = PROXY v2 reception off (the default).
    proxy_protocol: Option<manifest::ProxyProtocolTrust>,
    /// The OTLP span buffer (ADR 000040), present iff `[observability] otlp_endpoint` is set:
    /// fanned in beside the sinks above at `Host` construction, drained by the fast path's
    /// export pump. Like the admin listener, it binds once at startup — a reload swaps only the
    /// filter set, so the buffer (and the endpoint) live for the process.
    otlp: Option<Arc<plecto_host::otlp::OtlpBuffer>>,
}

impl Control {
    /// The ONE place a `Control` is put together: every public constructor reduces to "obtain a
    /// `Host` + store + observability handles, then assemble" — a new field means one edit here,
    /// not four (the four constructors previously hand-built the 15-field struct and had already
    /// drifted subtly on `filter_metrics`).
    fn assemble(
        host: Host,
        store: Box<dyn ArtifactStore>,
        manifest: &Manifest,
        base_dir: &Path,
        manifest_path: Option<&Path>,
        filter_metrics: Arc<MetricsSink>,
        otlp: Option<Arc<plecto_host::otlp::OtlpBuffer>>,
    ) -> Result<Self, ControlError> {
        let upstreams = Arc::new(UpstreamRegistry::new());
        let active = build_active(&host, manifest, store.as_ref(), base_dir, &upstreams)?;
        Ok(Self {
            host,
            store,
            active: ArcSwap::from_pointee(active),
            reload_gate: parking_lot::Mutex::new(()),
            upstreams,
            manifest_path: manifest_path.map(Path::to_path_buf),
            trust: manifest.trust.clone(),
            state: manifest.state.clone(),
            base_dir: base_dir.to_path_buf(),
            filter_metrics,
            observability: manifest.observability.clone(),
            listen: manifest.listen.clone(),
            proxy_protocol: manifest.listen.proxy_protocol_trust()?,
            otlp,
        })
    }

    /// Build a control plane entirely from a manifest and a base directory — the ops
    /// entrypoint. Reads the trusted-key PEMs (ADR 000006), constructs the `Host`, and
    /// resolves filters from offline OCI image-layouts under `base_dir` (ADR 000007). Every
    /// path in the manifest (`trust.keys`, each filter `source`) is resolved relative to
    /// `base_dir`. Remote fetch (`wkg`) is an out-of-band step that populates those layouts.
    pub fn from_manifest(manifest: &Manifest, base_dir: &Path) -> Result<Self, ControlError> {
        let (host, store, filter_metrics, otlp) = build_host_and_store(manifest, base_dir)?;
        Self::assemble(
            host,
            Box::new(store),
            manifest,
            base_dir,
            None,
            filter_metrics,
            otlp,
        )
    }

    /// Build from a pre-constructed `Host` (carrying its `TrustPolicy`) and an artifact store
    /// — the testable core. Each manifest filter is resolved through `store` (digest pin),
    /// loaded through the host's ADR 000006 gate (signature + SBOM), and the chain order is
    /// validated against the loaded set. Any failure aborts the build (nothing is loaded
    /// half-way into a live set).
    pub fn load(
        host: Host,
        manifest: &Manifest,
        store: Box<dyn ArtifactStore>,
    ) -> Result<Self, ControlError> {
        // The in-memory core has no manifest directory; relative paths resolve against the cwd.
        // Tests that exercise `[[tls]]` use absolute cert paths, so this base does not bite them.
        // OTLP export (ADR 000040): fan the span buffer in BESIDE the caller's sink (never
        // replacing it), before `build_active` loads filters (the sink is cloned into each).
        // The caller supplied the `Host`, so its sink is the caller's (or `NoopSink`); this
        // testable core keeps its own empty `filter_metrics` tally rather than reaching into it.
        let (host, otlp) = add_otlp_buffer(host, manifest);
        Self::assemble(
            host,
            store,
            manifest,
            Path::new("."),
            None,
            Arc::new(MetricsSink::new()),
            otlp,
        )
    }

    /// Build the whole control plane from a single on-disk manifest file — the
    /// disk-reloadable ops entrypoint (ADR 000007 / 000008). Like `from_manifest`, but reads
    /// and *remembers* the manifest path so SIGHUP / `reload_from_disk` can pick up an
    /// operator's edits. Trusted-key PEMs and filter layouts resolve relative to the
    /// manifest's own directory.
    pub fn from_manifest_path(manifest_path: &Path) -> Result<Self, ControlError> {
        let base_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let manifest = read_manifest(manifest_path)?;
        let (host, store, filter_metrics, otlp) = build_host_and_store(&manifest, base_dir)?;
        Self::assemble(
            host,
            Box::new(store),
            &manifest,
            base_dir,
            Some(manifest_path),
            filter_metrics,
            otlp,
        )
    }

    /// Like `load`, but the manifest lives on disk at `manifest_path`: the path is remembered
    /// so `reload_from_disk` can re-read it, while artifacts still resolve through the injected
    /// `store` (so a test can pair an on-disk manifest with an in-memory artifact store). The
    /// trust policy stays fixed on `host`.
    pub fn load_at(
        host: Host,
        manifest_path: &Path,
        store: Box<dyn ArtifactStore>,
    ) -> Result<Self, ControlError> {
        let base_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let manifest = read_manifest(manifest_path)?;
        let (host, otlp) = add_otlp_buffer(host, &manifest);
        Self::assemble(
            host,
            store,
            &manifest,
            base_dir,
            Some(manifest_path),
            Arc::new(MetricsSink::new()),
            otlp,
        )
    }

    /// The ids currently loaded (for diagnostics / tests). Order is unspecified.
    pub fn loaded_ids(&self) -> Vec<String> {
        self.active.load().filters.keys().cloned().collect()
    }
}

/// Read + parse a manifest from disk (shared by the on-disk constructors and `reload_from_disk`).
fn read_manifest(path: &Path) -> Result<Manifest, ControlError> {
    let toml = std::fs::read_to_string(path).map_err(|e| ControlError::IoAt {
        path: path.to_path_buf(),
        source: e,
    })?;
    Manifest::from_toml(&toml)
}

/// Read a manifest-referenced file with the path attached to any failure (DECREE §3: an error a
/// human acts on must say WHICH file — trust keys and manifests are read from several places).
fn read_file(path: &Path) -> Result<Vec<u8>, ControlError> {
    std::fs::read(path).map_err(|e| ControlError::IoAt {
        path: path.to_path_buf(),
        source: e,
    })
}

/// [`validate_manifest`] / [`validate_manifest_path`]'s success value: the manifest's config
/// version plus any non-fatal [`Diagnostic`] warnings (ADR 000065 decision 5) — currently just
/// [`DEV_KEY_IN_TRUST`], raised when a `[trust]` key file carries `plecto_host::DEV_KEY_MARKER`.
/// A warning never fails validation: a `plecto dev`-generated manifest is SUPPOSED to trip it.
#[derive(Debug)]
pub struct ValidateOutcome {
    pub config_version: String,
    pub warnings: Vec<Diagnostic>,
}

/// Statically validate `manifest` — the `plecto validate` core (the `nginx -t` shape): every
/// check the server would fail closed on at startup that needs no artifact and mutates nothing.
/// Covers the strict parse (the caller already ran it), `[trust]` key files, `[state]` coherence,
/// per-filter metering/rate-limit ranges, duplicate ids, chain and route references, the weighted
/// split, `[[tls]]` cert/key loads, and `[[upstream]]` (LB config + `[upstream.tls]` CA loads).
/// Returns the manifest's config version (semantic content hash, ADR 000008) on success.
///
/// Deliberately NOT covered, so a CI run needs only the manifest + its referenced config files:
/// OCI artifact resolution and the signature/SBOM load gate (the deploy dir may not exist where
/// validation runs — startup still enforces them, ADR 000006/000007), and the `[state]` backend
/// open (validation must never create a redb file).
pub fn validate_manifest(
    manifest: &Manifest,
    base_dir: &Path,
) -> Result<ValidateOutcome, ControlError> {
    let mut pems: Vec<Vec<u8>> = Vec::with_capacity(manifest.trust.keys.len());
    let mut warnings = Vec::new();
    for key_path in &manifest.trust.keys {
        let pem = read_file(&base_dir.join(key_path))?;
        if pem.starts_with(plecto_host::DEV_KEY_MARKER.as_bytes()) {
            warnings.push(DEV_KEY_IN_TRUST);
        }
        pems.push(pem);
    }
    TrustPolicy::from_pem_keys(&pems).map_err(|e| ControlError::TrustKey(e.to_string()))?;
    manifest.state.validate()?;
    manifest.listen.validate()?;
    let filter_ids = validate_filters_and_chain(manifest)?;
    let upstream_names: HashSet<&str> =
        manifest.upstreams.iter().map(|u| u.name.as_str()).collect();
    route::validate_routes(&manifest.routes, &filter_ids, &upstream_names)?;
    // ONE read of the client-auth CA, shared between the verifier build and the config version
    // (same rule as `build_active`): the reported version always describes the validated bytes.
    let client_auth_ca = manifest.read_client_auth_ca(base_dir)?;
    tls::build_server_configs(
        &manifest.tls,
        manifest.resumption.as_ref(),
        manifest
            .listen
            .client_auth
            .as_ref()
            .zip(client_auth_ca.as_deref()),
        base_dir,
    )?;
    // A throwaway registry runs the full upstream validation (names, LB, `[upstream.tls]` CA
    // loads) without touching any live state.
    UpstreamRegistry::new().reconcile(&manifest.upstreams, base_dir)?;
    let config_version = manifest.content_hash_with_ca(client_auth_ca.as_deref())?;
    Ok(ValidateOutcome {
        config_version,
        warnings,
    })
}

/// [`validate_manifest`] for an on-disk manifest: reads + strictly parses `path`, resolving
/// relative paths against the manifest's own directory (the same rule the server applies).
pub fn validate_manifest_path(path: &Path) -> Result<ValidateOutcome, ControlError> {
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let manifest = read_manifest(path)?;
    validate_manifest(&manifest, base_dir)
}

/// The manifest's JSON Schema (ADR 000049), derived from the very serde model `from_toml` parses
/// with — the schema cannot drift from the structs, and `deny_unknown_fields` surfaces as
/// `additionalProperties: false`, so editor validation rejects exactly what `validate` rejects.
/// draft-07 output: the level taplo / Even Better TOML consume (schemars' 2020-12 default is
/// outside taplo's documented support). Serialising a just-generated schema cannot fail, but if
/// it ever did, the error surfaces to the CLI caller — a silent `"{}"` fallback would be a
/// schema that validates EVERYTHING, fail-open for the editor validation this exists to provide.
pub fn manifest_json_schema() -> Result<String, ControlError> {
    let generator = schemars::generate::SchemaSettings::draft07().into_generator();
    let schema = generator.into_root_schema_for::<Manifest>();
    Ok(serde_json::to_string_pretty(&schema)?)
}

/// What `build_host_and_store` assembles for the manifest-driven constructors: the `Host` (sinks
/// wired), the offline OCI store, and the observability handles `Control` retains.
type BuiltHost = (
    Host,
    oci::OciLayoutStore,
    Arc<MetricsSink>,
    Option<Arc<plecto_host::otlp::OtlpBuffer>>,
);

/// Construct the `Host` (trust roots from the manifest's PEMs, ADR 000006; state backend from
/// `[state]`, ADR 000041) and the offline OCI artifact store, both rooted at `base_dir`.
/// Shared by `from_manifest` and `from_manifest_path`.
fn build_host_and_store(manifest: &Manifest, base_dir: &Path) -> Result<BuiltHost, ControlError> {
    let mut pems: Vec<Vec<u8>> = Vec::with_capacity(manifest.trust.keys.len());
    for key_path in &manifest.trust.keys {
        pems.push(read_file(&base_dir.join(key_path))?);
    }
    let trust =
        TrustPolicy::from_pem_keys(&pems).map_err(|e| ControlError::TrustKey(e.to_string()))?;
    let kv = build_state_backend(&manifest.state, base_dir)?;
    // Wire the host-aggregated filter metrics (ADR 000009): a `MetricsSink` tallies every filter
    // execution. Set BEFORE filters load (the sink is cloned into each at `load`), and retained on
    // `Control` so the fast path's admin endpoint can snapshot it. The default was `NoopSink`
    // (observability off) — this is the wiring that makes the M5 span/metrics stage observable.
    let filter_metrics = Arc::new(MetricsSink::new());
    let host = Host::with_backend(trust, kv)
        .map_err(|e| ControlError::HostInit(e.to_string()))?
        .with_telemetry_sink(filter_metrics.clone());
    // OTLP export (ADR 000040): the span buffer fans in beside the metrics tally.
    let (host, otlp) = add_otlp_buffer(host, manifest);
    let store = oci::OciLayoutStore::new(base_dir);
    Ok((host, store, filter_metrics, otlp))
}

/// Build the `KvBackend` the manifest's `[state]` selects (ADR 000041): the one store the
/// `host-kv` / `host-counter` / `host-ratelimit` capabilities share. `memory` keeps today's
/// process-lifetime behaviour; `redb` opens (or creates) the database at the manifest-relative
/// `path`. The parent directory must already exist — a typo'd path errors here instead of
/// silently growing a new tree (directory preparation is the operator's responsibility).
fn build_state_backend(
    state: &manifest::State,
    base_dir: &Path,
) -> Result<Arc<dyn plecto_host::KvBackend>, ControlError> {
    state.validate()?;
    match state.backend {
        StateBackendKind::Memory => Ok(Arc::new(plecto_host::MemoryBackend::default())),
        StateBackendKind::Redb => {
            let path = base_dir.join(state.path.as_deref().unwrap_or_default());
            if !path.parent().is_some_and(Path::is_dir) {
                return Err(ControlError::StateBackendInit(format!(
                    "parent directory of {} does not exist",
                    path.display()
                )));
            }
            let backend = plecto_host::RedbBackend::open(&path)
                .map_err(|e| ControlError::StateBackendInit(e.to_string()))?;
            Ok(Arc::new(backend))
        }
    }
}

/// When `[observability] otlp_endpoint` is set, fan the OTLP span buffer (ADR 000040) in beside
/// the host's current sink. Must run before filters load (the sink is cloned into each).
fn add_otlp_buffer(
    host: Host,
    manifest: &Manifest,
) -> (Host, Option<Arc<plecto_host::otlp::OtlpBuffer>>) {
    if manifest.observability.otlp_endpoint.is_none() {
        return (host, None);
    }
    let buffer = Arc::new(plecto_host::otlp::OtlpBuffer::default());
    (host.with_added_telemetry_sink(buffer.clone()), Some(buffer))
}

/// The pure filter/chain-semantics checks shared by [`validate_manifest`] (the `nginx -t` core)
/// and [`build_active`] (the load path): duplicate filter ids, per-entry metering / rate-limit
/// ranges, and default-chain references. ONE function so a check added for one caller cannot be
/// silently missed by the other (the two previously re-implemented this sequence in parallel).
fn validate_filters_and_chain(manifest: &Manifest) -> Result<HashSet<&str>, ControlError> {
    let mut filter_ids: HashSet<&str> = HashSet::with_capacity(manifest.filters.len());
    for entry in &manifest.filters {
        if !filter_ids.insert(entry.id.as_str()) {
            return Err(ControlError::DuplicateFilterId(entry.id.clone()));
        }
        // Reject out-of-range metering / rate-limit values before they reach the host.
        entry.validate()?;
    }
    for id in &manifest.chain.filters {
        if !filter_ids.contains(id.as_str()) {
            return Err(ControlError::UnknownChainFilter(id.clone()));
        }
    }
    Ok(filter_ids)
}

/// Resolve + verify + load every manifest filter into a fresh `ActiveConfig`. Pure w.r.t. the
/// live set: it touches nothing until it fully succeeds, so a failed `reload` leaves the
/// running set untouched.
fn build_active(
    host: &Host,
    manifest: &Manifest,
    store: &dyn ArtifactStore,
    base_dir: &Path,
    registry: &UpstreamRegistry,
) -> Result<ActiveConfig, ControlError> {
    // The pure semantic checks run FIRST (shared with `validate_manifest`), so the load loop
    // below never sees a duplicate id or an unreferenced chain filter.
    let filter_ids = validate_filters_and_chain(manifest)?;
    let mut filters: HashMap<String, Arc<LoadedFilter>> = HashMap::new();
    for entry in &manifest.filters {
        let artifact = store.resolve(&entry.source, &entry.digest)?;
        let signed = SignedArtifact {
            component_bytes: &artifact.component,
            component_signature: &artifact.component_signature,
            sbom: &artifact.sbom,
            sbom_signature: &artifact.sbom_signature,
        };
        let loaded = host
            .load(&entry.id, &signed, entry.load_options())
            .map_err(|err| ControlError::Load {
                id: entry.id.clone(),
                err,
            })?;
        filters.insert(entry.id.clone(), Arc::new(loaded));
    }

    // Routing table (ADR 000013 / 000017). Validate every route reference (upstream name, filter
    // ids), the weighted split, and the native rate limit PURELY first — before the persistent
    // upstream registry is mutated — so a manifest we'd reject never reconciles the registry
    // (reload stays all-or-nothing; the running upstream health state is untouched on a failed
    // reload). `validated_routes` carries each route's already-resolved forwarding targets, reused
    // below instead of calling `targets()` again.
    let upstream_names: HashSet<&str> =
        manifest.upstreams.iter().map(|u| u.name.as_str()).collect();
    let validated_routes = route::validate_routes(&manifest.routes, &filter_ids, &upstream_names)?;

    // TLS termination config (ADR 000014 TCP / ADR 000016 QUIC): build the rustls ServerConfigs
    // from `[[tls]]`, sharing one SNI cert resolver. A bad cert is fail-closed here, so a failed
    // reload never swaps in a TLS config that cannot serve. Built before the registry is touched.
    // The client-auth CA is read ONCE and shared with the content hash below, so the recorded
    // config version always describes the trust roots the verifier was actually built from.
    let client_auth_ca = manifest.read_client_auth_ca(base_dir)?;
    let (tls, quic_tls) = match tls::build_server_configs(
        &manifest.tls,
        manifest.resumption.as_ref(),
        manifest
            .listen
            .client_auth
            .as_ref()
            .zip(client_auth_ca.as_deref()),
        base_dir,
    )? {
        Some(configs) => (Some(configs.tcp), Some(configs.quic)),
        None => (None, None),
    };

    // Compute the content hash BEFORE the registry reconcile (review f000005 P3#8). `reconcile`
    // is the step that MUTATES persistent state (the health registry, which survives reloads), so
    // every other fallible step — including this hash — must run before it for the "after reconcile
    // the build is infallible" / all-or-nothing invariant to hold literally, not just in practice.
    let hash = manifest.content_hash_with_ca(client_auth_ca.as_deref())?;

    // Reconcile the upstream registry LAST among the fallible steps (ADR 000017): this validates
    // duplicate names / empty address lists and preserves health for unchanged `(name, address)`
    // instances across the reload. After it returns Ok the build is infallible, so a rejected
    // reload never leaves the registry reconciled to a manifest whose `active` was not swapped in.
    registry.reconcile(&manifest.upstreams, base_dir)?;
    let mut routes = Vec::with_capacity(validated_routes.len());
    for route::ValidatedRoute { route: r, targets } in validated_routes {
        // Resolve the route's forwarding targets (already validated above) to their upstream
        // groups, then compile the weighted split (ADR 000034). A single `upstream` becomes a
        // one-element set.
        let mut resolved = Vec::with_capacity(targets.len());
        for (name, weight) in targets {
            // present: the name was validated above and reconcile built a group for each manifest
            // upstream. Fall back to the error (unreachable) rather than panic (data-plane no-panic).
            let Some(group) = registry.group(name) else {
                return Err(ControlError::UnknownRouteUpstream {
                    path_prefix: r.matcher.path_prefix.clone(),
                    upstream: name.to_string(),
                });
            };
            resolved.push((group, weight));
        }
        let backends = weighted::WeightedBackends::new(resolved).map_err(|reason| {
            ControlError::InvalidRoute {
                path_prefix: r.matcher.path_prefix.clone(),
                reason,
            }
        })?;
        // The compilation itself (pre-normalised match dimensions, resolved chain, limiter /
        // upgrade / compression facilities) lives beside `CompiledRoute` in route.rs.
        routes.push(route::CompiledRoute::compile(r, backends, &filters));
    }

    let resolved_chain = manifest
        .chain
        .filters
        .iter()
        .filter_map(|id| filters.get(id).cloned())
        .collect();

    Ok(ActiveConfig {
        filters,
        resolved_chain,
        routes,
        tls,
        quic_tls,
        hash,
    })
}
