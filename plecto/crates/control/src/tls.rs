//! TLS termination config (ADR 000014): build a rustls `ServerConfig` from the manifest's
//! `[[tls]]` certs at load/reload. Cert selection is by SNI — a per-host cert, falling back to a
//! host-less default; if neither matches, the handshake is refused (no cert to present), which is
//! fail-closed. Building here (not in the server) means a bad cert aborts the whole build, so a
//! failed reload never swaps in a TLS config that cannot serve, and the config rides `ActiveConfig`
//! behind the same `ArcSwap` as the filter set. Sync rustls + the `aws_lc_rs` provider (ADR
//! 000051 — one crypto backend shared with the QUIC config and the host's `sigstore` dependency);
//! the async acceptor lives in `plecto-server`. Session resumption is stateless (ADR 000052): by
//! default one process-lifetime ticket key (6h rotation / 12h window, node-local per ADR 000053),
//! or — opt-in via `[resumption]` (ADR 000062) — cert-bound keys derived from a shared file
//! (`stek.rs`) so tickets resume across replicas. Either way: no stateful session cache, 0-RTT
//! refused as an invariant.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use rustls::ServerConfig;
use rustls::crypto::aws_lc_rs as provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, NoServerSessionStorage, ProducesTickets, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::error::ControlError;
use crate::manifest::{ClientAuth, Resumption, TlsCert};
use crate::stek::SharedStekTicketer;

/// SNI cert selection (ADR 000014): a per-host cert map plus an optional default. `resolve` is
/// called by rustls during the handshake with the client's SNI; no match and no default → `None`,
/// and rustls fails the handshake (fail-closed — Plecto presents no cert it was not configured to).
#[derive(Debug)]
struct SniResolver {
    by_host: HashMap<String, Arc<CertifiedKey>>,
    default: Option<Arc<CertifiedKey>>,
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        hello
            .server_name()
            .and_then(|name| self.by_host.get(&name.to_ascii_lowercase()))
            .cloned()
            .or_else(|| self.default.clone())
    }
}

/// The process-lifetime session-ticket producer (ADR 000052): `aws_lc_rs::Ticketer` = rustls'
/// `TicketRotator` over RFC 5077 §4 self-encrypted tickets, 6h key rotation with a 12h acceptance
/// window (current + previous key). ONE instance for the whole process — shared by the TCP and
/// QUIC configs (a ticket obtained over either resumes on both; both configs present the same
/// certs via the shared `SniResolver`, so rustls' cross-config resumption caveat does not bite)
/// and, critically, surviving manifest reloads: rebuilding the `ServerConfig`s must not invalidate
/// outstanding tickets. Keys live in process memory only, node-local (ADR 000053) — never on disk,
/// never in the manifest.
fn shared_ticketer() -> Result<Arc<dyn ProducesTickets>, ControlError> {
    static TICKETER: OnceLock<Arc<dyn ProducesTickets>> = OnceLock::new();
    if let Some(ticketer) = TICKETER.get() {
        return Ok(ticketer.clone());
    }
    // Fallible init outside `get_or_init` (which can't return errors): the race where two threads
    // both construct is benign — one wins the OnceLock, the loser's keys are dropped unissued.
    let fresh = provider::Ticketer::new().map_err(|e| ControlError::TlsCert {
        host: None,
        path: String::new(),
        reason: format!("session-ticket key init: {e}"),
    })?;
    Ok(TICKETER.get_or_init(|| fresh).clone())
}

/// The TLS configs built from `[[tls]]`: the TCP config (HTTP/1.1 + HTTP/2 via ALPN, ADR 000015)
/// and the QUIC config (HTTP/3, ADR 000016). Both share one SNI cert resolver, so a host's cert is
/// presented identically over TCP and QUIC.
pub(crate) struct TlsConfigs {
    /// HTTP/1.1 + HTTP/2 over TCP: ALPN `[h2, http/1.1]`, TLS 1.2 + 1.3.
    pub(crate) tcp: Arc<ServerConfig>,
    /// HTTP/3 over QUIC: ALPN `[h3]`, TLS 1.3 only.
    pub(crate) quic: Arc<ServerConfig>,
}

/// Build the TCP + QUIC TLS `ServerConfig`s from the manifest's `[[tls]]` entries, or `None` when
/// there are none (the server then serves plain HTTP/1.1, no h3). Any unreadable / unparsable /
/// duplicate cert is a fail-closed `ControlError` that aborts the caller's build (ADR 000014). The
/// TCP config advertises ALPN `[h2, http/1.1]` (h2 preferred, ADR 000015); the QUIC config
/// advertises `[h3]` and is TLS-1.3 only (QUIC mandates 1.3, RFC 9001). Both share one `SniResolver`.
pub(crate) fn build_server_configs(
    entries: &[TlsCert],
    resumption: Option<&Resumption>,
    client_auth: Option<&ClientAuth>,
    base_dir: &Path,
) -> Result<Option<TlsConfigs>, ControlError> {
    let _ = client_auth; // wired by the GREEN half of this slice (ADR 000078)
    if entries.is_empty() {
        // `[resumption]` without any `[[tls]]` is a config mistake, not a no-op: the operator
        // asked for cross-replica resumption on a proxy that terminates no TLS. Fail closed.
        if let Some(resumption) = resumption {
            return Err(ControlError::Stek {
                path: resumption.stek_file.clone(),
                reason: "[resumption] requires at least one [[tls]] cert (ticket keys bind to \
                         the cert set, ADR 000062)"
                    .to_string(),
            });
        }
        return Ok(None);
    }

    let mut by_host: HashMap<String, Arc<CertifiedKey>> = HashMap::new();
    let mut default: Option<Arc<CertifiedKey>> = None;
    let mut all_certified: Vec<Arc<CertifiedKey>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let certified = Arc::new(load_certified_key(entry, base_dir)?);
        all_certified.push(certified.clone());
        match &entry.host {
            Some(host) => {
                if by_host
                    .insert(host.to_ascii_lowercase(), certified)
                    .is_some()
                {
                    return Err(tls_err(entry, "duplicate cert for this SNI host"));
                }
            }
            None => {
                if default.replace(certified).is_some() {
                    return Err(tls_err(entry, "more than one default (host-less) cert"));
                }
            }
        }
    }

    // One resolver, shared by both configs (Arc clone): SNI selects the same cert over TCP and QUIC.
    let resolver = Arc::new(SniResolver { by_host, default });
    // One ticket producer, ditto: stateless TLS 1.3 resumption over both paths. Default is the
    // per-node process-lifetime key (ADR 000052); `[resumption]` swaps in the shared cert-bound
    // ticketer (ADR 000062) — rebuilt each build, which is safe because its keys are a pure
    // function of (key file, cert set): unchanged inputs re-derive the same keys across reloads.
    let ticketer: Arc<dyn ProducesTickets> = match resumption {
        Some(resumption) => SharedStekTicketer::from_manifest(
            resumption,
            base_dir,
            cert_set_binding(&all_certified, resumption)?,
        )?,
        None => shared_ticketer()?,
    };

    // TCP: HTTP/1.1 + HTTP/2 via ALPN (h2 preferred, ADR 000015), TLS 1.2 + 1.3.
    let mut tcp = ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(provider_init_err)?
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
    tcp.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    // QUIC: HTTP/3, ALPN `h3`, TLS 1.3 only (ADR 000016). 0-RTT stays disabled: `max_early_data_size`
    // is left at its default 0, so the server refuses TLS early data. A gateway must not forward
    // early-data requests to upstreams that may not emit 425 (Too Early) (RFC 8470), so refusing it
    // outright is the only safe choice here. The stateless ticketer below refuses it a second way
    // (rustls only configures early data when the ticketer is DISabled) — but the invariant test
    // pins `max_early_data_size == 0` on its own, so a rustls behavior change cannot reopen 0-RTT.
    let mut quic = ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(provider_init_err)?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    quic.alpn_protocols = vec![b"h3".to_vec()];

    // Stateless session resumption (ADR 000052), replacing the implicit rustls default (a
    // 256-entry stateful cache whose tickets are lookup keys). The self-encrypted ticket carries
    // the session; per-session server memory is ZERO and there is no cache-size knob to fall off —
    // the same bounded-memory discipline as native rate limit (ADR 000033, CWE-770). TLS 1.2
    // clients get RFC 5077 stateless tickets from the same producer; 1.2 session-ID resumption is
    // gone with the cache, which is the point.
    for config in [&mut tcp, &mut quic] {
        config.ticketer = ticketer.clone();
        config.session_storage = Arc::new(NoServerSessionStorage {});
    }

    Ok(Some(TlsConfigs {
        tcp: Arc::new(tcp),
        quic: Arc::new(quic),
    }))
}

/// Build the rustls `ClientConfig` for one `[upstream.tls]` entry (ADR 000042): server
/// certificate verification is ALWAYS on — against the manifest's CA bundle when `ca_path` is
/// set (replacing, not extending, the webpki roots: an internal-CA deployment trusts exactly its
/// CA), else against the webpki (Mozilla) roots. ALPN is left unset here BY CONTRACT: the fast
/// path's HTTPS connector owns it (hyper-rustls rejects a pre-populated list) and advertises
/// `[h2, http/1.1]` — the negotiation result, not manifest config, selects the upstream protocol.
/// Built at load/reload like the server configs above, so a bad CA fails the build closed.
pub(crate) fn build_upstream_client_config(
    upstream_name: &str,
    tls: &crate::manifest::UpstreamTls,
    base_dir: &Path,
) -> Result<Arc<rustls::ClientConfig>, ControlError> {
    let mut roots = rustls::RootCertStore::empty();
    match &tls.ca_path {
        Some(ca_path) => {
            let err = |reason: String| ControlError::UpstreamTlsCa {
                upstream: upstream_name.to_string(),
                path: ca_path.clone(),
                reason,
            };
            let bytes = std::fs::read(base_dir.join(ca_path))
                .map_err(|e| err(format!("read failed: {e}")))?;
            let certs = CertificateDer::pem_slice_iter(&bytes)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| err(format!("bad CA PEM: {e}")))?;
            if certs.is_empty() {
                return Err(err("no certificates in CA PEM".to_string()));
            }
            let (added, _ignored) = roots.add_parsable_certificates(certs);
            if added == 0 {
                return Err(err("no usable root certificate in CA PEM".to_string()));
            }
        }
        None => {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
    }
    let config =
        rustls::ClientConfig::builder_with_provider(Arc::new(provider::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(provider_init_err)?
            .with_root_certificates(roots)
            .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Parse one `[upstream.tls] sni` verification-name override into a rustls `ServerName` (ADR
/// 000050). Fail-closed at build, like the CA bundle above: a name that parses as neither a DNS
/// name nor an IP address aborts the reconcile before the registry mutates, rather than letting
/// every TLS leg to this upstream fail at request time.
pub(crate) fn parse_upstream_sni(
    upstream_name: &str,
    sni: &str,
) -> Result<rustls::pki_types::ServerName<'static>, ControlError> {
    rustls::pki_types::ServerName::try_from(sni.to_string()).map_err(|e| {
        ControlError::UpstreamTlsSni {
            upstream: upstream_name.to_string(),
            sni: sni.to_string(),
            reason: e.to_string(),
        }
    })
}

/// The cert-set binding identity fed to the shared-STEK key schedule (ADR 000062 (a)): SHA-256
/// over the sorted, deduplicated SPKI SHA-256 fingerprints of every `[[tls]]` cert. SPKI (the
/// RFC 7469 pin basis), not the whole cert: the key pair is the cryptographic identity, so a
/// routine renewal under the same key keeps outstanding tickets resumable, while any cert-set
/// difference between deployments derives disjoint ticket keys. `ProducesTickets` is per-config
/// (rustls cannot tell the ticketer which SNI cert a handshake selected), so the binding is the
/// SET — cross-SNI acceptance inside one config is separately refused by rustls' own SNI match
/// on resumption (`resumedata.sni == sni`, pinned by an E2E test).
///
/// The SPKI comes from the loaded private key (`SigningKey::public_key`), not from parsing the
/// cert: `load_certified_key` already verified they match (`keys_match`), and this avoids an
/// X.509 parser dependency. A provider that cannot expose it fails the build closed — shared
/// STEK without a binding identity is exactly the unbound sharing the ADR rejects.
fn cert_set_binding(
    certified: &[Arc<CertifiedKey>],
    resumption: &Resumption,
) -> Result<[u8; 32], ControlError> {
    use sha2::{Digest, Sha256};
    let mut fingerprints: Vec<[u8; 32]> = Vec::with_capacity(certified.len());
    for certified_key in certified {
        let spki = certified_key
            .key
            .public_key()
            .ok_or_else(|| ControlError::Stek {
                path: resumption.stek_file.clone(),
                reason:
                    "a [[tls]] key's SPKI is unavailable from the provider; shared STEK cannot \
                     bind tickets to the cert set (ADR 000062 (a))"
                        .to_string(),
            })?;
        fingerprints.push(Sha256::digest(spki.as_ref()).into());
    }
    fingerprints.sort_unstable();
    fingerprints.dedup();
    let mut hasher = Sha256::new();
    for fingerprint in &fingerprints {
        hasher.update(fingerprint);
    }
    Ok(hasher.finalize().into())
}

/// A rustls provider/version init failure (not a per-cert fault) mapped to a fail-closed error.
fn provider_init_err(e: rustls::Error) -> ControlError {
    ControlError::TlsCert {
        host: None,
        path: String::new(),
        reason: format!("rustls provider init: {e}"),
    }
}

/// Read + parse one `[[tls]]` entry's PEM cert chain and private key into a `CertifiedKey`.
fn load_certified_key(entry: &TlsCert, base_dir: &Path) -> Result<CertifiedKey, ControlError> {
    let cert_chain = read_certs(entry, base_dir)?;
    if cert_chain.is_empty() {
        return Err(tls_err(entry, "no certificates in cert_path PEM"));
    }
    let key = read_key(entry, base_dir)?;
    let signing_key = provider::sign::any_supported_type(&key)
        .map_err(|e| tls_err(entry, &format!("unsupported private key: {e}")))?;
    let certified = CertifiedKey::new(cert_chain, signing_key);
    // `CertifiedKey::new` does NOT verify the private key matches the leaf certificate, so a
    // mismatched cert/key pair would build successfully and then fail EVERY TLS handshake at
    // runtime — contradicting this module's fail-closed-at-build contract. Verify here. `Unknown`
    // (the provider can't expose the key's SPKI) is not a mismatch — accept it, mirroring rustls'
    // own `CertifiedKey::from_der`. The `aws_lc_rs` provider does expose it, so a real mismatch is
    // caught.
    match certified.keys_match() {
        Ok(()) | Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::Unknown)) => {}
        Err(e) => return Err(tls_err(entry, &format!("cert/key mismatch: {e}"))),
    }
    Ok(certified)
}

fn read_certs(
    entry: &TlsCert,
    base_dir: &Path,
) -> Result<Vec<CertificateDer<'static>>, ControlError> {
    let bytes = std::fs::read(base_dir.join(&entry.cert_path))
        .map_err(|e| tls_err_path(entry, &entry.cert_path, &format!("read failed: {e}")))?;
    // PEM parsing lives in rustls-pki-types now (rustls-pemfile is unmaintained, RUSTSEC-2025-0134).
    CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tls_err_path(entry, &entry.cert_path, &format!("bad cert PEM: {e}")))
}

fn read_key(entry: &TlsCert, base_dir: &Path) -> Result<PrivateKeyDer<'static>, ControlError> {
    let bytes = std::fs::read(base_dir.join(&entry.key_path))
        .map_err(|e| tls_err_path(entry, &entry.key_path, &format!("read failed: {e}")))?;
    // `from_pem_slice` returns the first private key, or a typed error if there is none / it is
    // malformed — so the previous "no key" branch folds into the error path.
    PrivateKeyDer::from_pem_slice(&bytes)
        .map_err(|e| tls_err_path(entry, &entry.key_path, &format!("bad key PEM: {e}")))
}

fn tls_err(entry: &TlsCert, reason: &str) -> ControlError {
    tls_err_path(entry, &entry.cert_path, reason)
}

fn tls_err_path(entry: &TlsCert, path: &str, reason: &str) -> ControlError {
    ControlError::TlsCert {
        host: entry.host.clone(),
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

impl crate::Control {
    /// The active TLS server config (ADR 000014), or `None` for plain HTTP/1.1. The fast-path
    /// server reads this per accepted connection, so a reload's new certs apply to new connections
    /// while in-flight ones keep the cert they negotiated with.
    pub fn tls_config(&self) -> Option<Arc<ServerConfig>> {
        self.active.load().tls.clone()
    }

    /// The active QUIC TLS config for HTTP/3 (ADR 000016): ALPN `h3`, TLS 1.3, sharing the TCP
    /// config's SNI cert resolver. `None` whenever there is no `[[tls]]` (h3 requires TLS, so it is
    /// only offered alongside TLS termination). The fast-path server reads this once to decide
    /// whether to bind a QUIC listener and what to advertise via `Alt-Svc`.
    pub fn quic_tls_config(&self) -> Option<Arc<ServerConfig>> {
        self.active.load().quic_tls.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-byte owner-only key file + `[resumption]` entry in `dir` (shared STEK, ADR 000062).
    fn resumption_entry(dir: &Path, fill: u8) -> Resumption {
        let path = dir.join("stek.key");
        std::fs::write(&path, [fill; crate::stek::STEK_FILE_LEN]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Resumption {
            stek_file: path.to_str().unwrap().to_string(),
            max_age_hours: 24,
        }
    }

    /// A fresh self-signed cert written to a temp dir, plus a host-less (default) `[[tls]]` entry.
    fn default_cert_entry() -> (tempfile::TempDir, TlsCert) {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, generated.cert.pem()).unwrap();
        std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();
        let entry = TlsCert {
            host: None,
            cert_path: cert_path.to_str().unwrap().to_string(),
            key_path: key_path.to_str().unwrap().to_string(),
        };
        (dir, entry)
    }

    #[test]
    fn builds_tcp_and_quic_configs_with_distinct_alpn() {
        let (dir, entry) = default_cert_entry();
        let configs = build_server_configs(&[entry], None, None, dir.path())
            .unwrap()
            .expect("a cert entry yields TCP + QUIC configs");
        // TCP advertises h2 then http/1.1 (ADR 000015); QUIC advertises only h3 (ADR 000016).
        assert_eq!(
            configs.tcp.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            "TCP ALPN is h2 then http/1.1"
        );
        assert_eq!(
            configs.quic.alpn_protocols,
            vec![b"h3".to_vec()],
            "QUIC ALPN is h3 only"
        );
    }

    #[test]
    fn stateless_resumption_invariants() {
        // ADR 000052's invariants, pinned per path so a rustls default change (or a refactor that
        // touches only one config) cannot silently reintroduce the stateful cache or 0-RTT.
        let (dir, entry) = default_cert_entry();
        let configs = build_server_configs(&[entry], None, None, dir.path())
            .unwrap()
            .expect("a cert entry yields TCP + QUIC configs");
        for (label, config) in [("tcp", &configs.tcp), ("quic", &configs.quic)] {
            assert!(
                config.ticketer.enabled(),
                "{label}: stateless session tickets are issued (ADR 000052)"
            );
            assert_eq!(
                config.ticketer.lifetime(),
                12 * 60 * 60,
                "{label}: 12h acceptance window (6h rotation, current + previous key)"
            );
            assert!(
                !config.session_storage.can_cache(),
                "{label}: no stateful session cache — per-session server memory stays zero"
            );
            assert_eq!(
                config.max_early_data_size, 0,
                "{label}: 0-RTT stays refused (ADR 000016 / RFC 8470), independent of the \
                 ticketer-exclusivity rustls happens to enforce today"
            );
        }
    }

    #[test]
    fn ticket_key_is_process_wide_across_paths_and_builds() {
        // One ticket producer for TCP + QUIC (a ticket obtained over either resumes on both), and
        // the SAME producer across rebuilds — a manifest reload must not invalidate outstanding
        // tickets (ADR 000052: process-lifetime, node-local key).
        let (dir, entry) = default_cert_entry();
        let a = build_server_configs(std::slice::from_ref(&entry), None, None, dir.path())
            .unwrap()
            .unwrap();
        let b = build_server_configs(&[entry], None, None, dir.path())
            .unwrap()
            .unwrap();
        assert!(
            Arc::ptr_eq(&a.tcp.ticketer, &a.quic.ticketer),
            "TCP and QUIC share one ticket producer"
        );
        assert!(
            Arc::ptr_eq(&a.tcp.ticketer, &b.tcp.ticketer),
            "a rebuild (reload) keeps the process-lifetime ticket key"
        );
    }

    #[test]
    fn no_tls_entries_yields_none() {
        assert!(
            build_server_configs(&[], None, None, std::path::Path::new("."))
                .unwrap()
                .is_none(),
            "no [[tls]] means no TLS/QUIC configs (plain HTTP/1.1, no h3)"
        );
    }

    #[test]
    fn mismatched_cert_and_key_is_rejected() {
        // a cert paired with a DIFFERENT private key must fail the build (fail-closed), not
        // go live and fail every TLS handshake at runtime. Cross two self-signed pairs.
        let a = rcgen::generate_simple_self_signed(vec!["a.example".to_string()]).unwrap();
        let b = rcgen::generate_simple_self_signed(vec!["b.example".to_string()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, a.cert.pem()).unwrap(); // cert A
        std::fs::write(&key_path, b.key_pair.serialize_pem()).unwrap(); // key B (mismatch)
        let entry = TlsCert {
            host: None,
            cert_path: cert_path.to_str().unwrap().to_string(),
            key_path: key_path.to_str().unwrap().to_string(),
        };
        let err = match build_server_configs(&[entry], None, None, dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("a mismatched cert/key pair must be rejected at build"),
        };
        assert!(matches!(err, ControlError::TlsCert { .. }));
    }

    // ----- ADR 000062: [resumption] shared STEK -----

    #[test]
    fn shared_stek_keeps_the_resumption_invariants() {
        // ADR 000062 (c): opting into the shared ticketer changes the KEY, not the posture —
        // stateless tickets on, no session cache, 0-RTT refused, on both paths. The lifetime
        // hint becomes max_age_hours (the acceptance window the key file discipline enforces).
        let (dir, entry) = default_cert_entry();
        let resumption = resumption_entry(dir.path(), 7);
        let configs = build_server_configs(&[entry], Some(&resumption), None, dir.path())
            .unwrap()
            .unwrap();
        for (label, config) in [("tcp", &configs.tcp), ("quic", &configs.quic)] {
            assert!(config.ticketer.enabled(), "{label}: tickets are issued");
            assert_eq!(
                config.ticketer.lifetime(),
                24 * 60 * 60,
                "{label}: the hint is max_age_hours, not the per-node 12h"
            );
            assert!(!config.session_storage.can_cache(), "{label}: no cache");
            assert_eq!(
                config.max_early_data_size, 0,
                "{label}: 0-RTT stays refused"
            );
        }
        assert!(
            Arc::ptr_eq(&configs.tcp.ticketer, &configs.quic.ticketer),
            "TCP and QUIC share the one shared-STEK producer (same cert set → same keys)"
        );
    }

    #[test]
    fn shared_stek_rebuilds_derive_interchangeable_keys() {
        // The reload story (ADR 000062): unlike the per-node OnceLock, each build constructs a
        // FRESH ticketer — outstanding tickets survive anyway because the keys are a pure
        // function of (file, cert set). Pin that: a ticket sealed by build A opens in build B.
        let (dir, entry) = default_cert_entry();
        let resumption = resumption_entry(dir.path(), 7);
        let a = build_server_configs(
            std::slice::from_ref(&entry),
            Some(&resumption),
            None,
            dir.path(),
        )
        .unwrap()
        .unwrap();
        let b = build_server_configs(&[entry], Some(&resumption), None, dir.path())
            .unwrap()
            .unwrap();
        assert!(
            !Arc::ptr_eq(&a.tcp.ticketer, &b.tcp.ticketer),
            "sanity: rebuilds construct distinct ticketer objects"
        );
        let ticket = a.tcp.ticketer.encrypt(b"session-state").unwrap();
        assert_eq!(
            b.tcp.ticketer.decrypt(&ticket).as_deref(),
            Some(&b"session-state"[..]),
            "a reload (or another replica) re-derives the same keys"
        );
    }

    #[test]
    fn shared_stek_binds_tickets_to_the_cert_set() {
        // ADR 000062 (a) at the build level: same key file, different cert → the ticket does not
        // cross (the USENIX'25 cross-listener class, killed in the key schedule).
        let (dir_a, entry_a) = default_cert_entry();
        let (dir_b, entry_b) = default_cert_entry(); // a different self-signed key pair
        let resumption = resumption_entry(dir_a.path(), 7);
        let a = build_server_configs(&[entry_a], Some(&resumption), None, dir_a.path())
            .unwrap()
            .unwrap();
        let b = build_server_configs(&[entry_b], Some(&resumption), None, dir_b.path())
            .unwrap()
            .unwrap();
        let ticket = a.tcp.ticketer.encrypt(b"session-state").unwrap();
        assert_eq!(
            b.tcp.ticketer.decrypt(&ticket),
            None,
            "a deployment with a different cert set must not accept the ticket"
        );
    }

    #[test]
    fn resumption_without_tls_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let resumption = resumption_entry(dir.path(), 7);
        let err = match build_server_configs(&[], Some(&resumption), None, dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("[resumption] with no [[tls]] must fail the build"),
        };
        assert!(matches!(err, ControlError::Stek { .. }));
    }

    #[test]
    fn shared_stek_bad_file_fails_the_build_closed() {
        // Wrong length and (on unix) loose permissions abort the build like a bad cert.
        let (dir, entry) = default_cert_entry();
        let mut resumption = resumption_entry(dir.path(), 7);
        std::fs::write(&resumption.stek_file, [7u8; 48]).unwrap();
        let err = match build_server_configs(
            std::slice::from_ref(&entry),
            Some(&resumption),
            None,
            dir.path(),
        ) {
            Err(e) => e,
            Ok(_) => panic!("a 48-byte file must be rejected"),
        };
        assert!(matches!(err, ControlError::Stek { .. }));

        // Out-of-range max_age_hours is rejected before the file is touched.
        resumption = resumption_entry(dir.path(), 7);
        resumption.max_age_hours = 169;
        let err = match build_server_configs(&[entry], Some(&resumption), None, dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("max_age_hours over the RFC 8446 cap must be rejected"),
        };
        assert!(matches!(err, ControlError::Stek { .. }));
    }

    // ----- ADR 000078: mTLS — [listen.client_auth] / [upstream.tls] client identity -----

    /// A `[listen.client_auth]` whose `ca_path` file holds `ca_pem`, written into `dir`.
    fn client_auth_entry(dir: &Path, ca_pem: &[u8]) -> ClientAuth {
        let path = dir.join("client-ca.pem");
        std::fs::write(&path, ca_pem).unwrap();
        ClientAuth {
            ca_path: path.to_str().unwrap().to_string(),
        }
    }

    /// A PEM trust anchor a client_auth test can point `ca_path` at.
    fn some_ca_pem() -> Vec<u8> {
        rcgen::generate_simple_self_signed(vec!["client-ca".to_string()])
            .unwrap()
            .cert
            .pem()
            .into_bytes()
    }

    /// A fresh self-signed client identity (cert + owner-only key PEM) for `[upstream.tls]`.
    fn upstream_client_identity(dir: &Path) -> (String, String) {
        let generated = rcgen::generate_simple_self_signed(vec!["plecto".to_string()]).unwrap();
        let cert_path = dir.join("client-cert.pem");
        let key_path = dir.join("client-key.pem");
        std::fs::write(&cert_path, generated.cert.pem()).unwrap();
        std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (
            cert_path.to_str().unwrap().to_string(),
            key_path.to_str().unwrap().to_string(),
        )
    }

    fn upstream_tls_with_identity(
        cert: Option<String>,
        key: Option<String>,
    ) -> crate::manifest::UpstreamTls {
        crate::manifest::UpstreamTls {
            client_cert_path: cert,
            client_key_path: key,
            ..Default::default()
        }
    }

    #[test]
    fn upstream_client_cert_without_key_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _key) = upstream_client_identity(dir.path());
        let tls = upstream_tls_with_identity(Some(cert), None);
        let err = match build_upstream_client_config("u", &tls, dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("client_cert_path without client_key_path must fail the build"),
        };
        assert!(matches!(err, ControlError::UpstreamClientCert { .. }));
    }

    #[test]
    fn upstream_client_key_without_cert_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (_cert, key) = upstream_client_identity(dir.path());
        let tls = upstream_tls_with_identity(None, Some(key));
        let err = match build_upstream_client_config("u", &tls, dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("client_key_path without client_cert_path must fail the build"),
        };
        assert!(matches!(err, ControlError::UpstreamClientCert { .. }));
    }

    #[test]
    fn upstream_client_identity_is_loaded_into_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = upstream_client_identity(dir.path());
        let tls = upstream_tls_with_identity(Some(cert), Some(key));
        let config = build_upstream_client_config("u", &tls, dir.path()).unwrap();
        assert!(
            config.client_auth_cert_resolver.has_certs(),
            "the declared client identity must be presented when the upstream requests one"
        );
    }

    #[cfg(unix)]
    #[test]
    fn upstream_client_key_readable_by_group_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = upstream_client_identity(dir.path());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
        let tls = upstream_tls_with_identity(Some(cert), Some(key));
        let err = match build_upstream_client_config("u", &tls, dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("a group-readable client key must fail the build (ADR 000062 (d))"),
        };
        assert!(matches!(err, ControlError::UpstreamClientCert { .. }));
    }

    #[test]
    fn client_auth_with_shared_stek_fails_closed() {
        // ADR 000062 (b) / 000078: a resumption ticket minted before the peer authenticated must
        // never let it skip authentication on another replica — the combination is refused.
        let (dir, entry) = default_cert_entry();
        let resumption = resumption_entry(dir.path(), 7);
        let auth = client_auth_entry(dir.path(), &some_ca_pem());
        let err = match build_server_configs(&[entry], Some(&resumption), Some(&auth), dir.path()) {
            Err(e) => e,
            Ok(_) => {
                panic!("[listen.client_auth] with [resumption] shared STEK must fail the build")
            }
        };
        assert!(matches!(err, ControlError::Stek { .. }));
    }

    #[test]
    fn client_auth_without_tls_entries_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let auth = client_auth_entry(dir.path(), &some_ca_pem());
        let err = match build_server_configs(&[], None, Some(&auth), dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("[listen.client_auth] with no [[tls]] must fail the build"),
        };
        assert!(matches!(err, ControlError::ClientAuthCa { .. }));
    }

    #[test]
    fn client_auth_ca_with_no_usable_root_fails_closed() {
        let (dir, entry) = default_cert_entry();
        let auth = client_auth_entry(dir.path(), b"not a pem at all");
        let err = match build_server_configs(&[entry], None, Some(&auth), dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("an unusable client-auth CA bundle must fail the build"),
        };
        assert!(matches!(err, ControlError::ClientAuthCa { .. }));
    }
}
