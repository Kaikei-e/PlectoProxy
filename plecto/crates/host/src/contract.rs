//! `plecto:filter` contract version detection and 0.1/0.2 → 0.3 adapters
//! (ADR 000071 / 000073).

mod bindings_v01 {
    // Vendored copy of `plecto/wit/v0.1.0/` — see `crate::bindings`'s comment for why.
    wasmtime::component::bindgen!({
        path: "wit/v0.1.0",
        world: "filter",
        exports: { default: async },
    });
}

mod bindings_v02 {
    // Vendored copy of `plecto/wit/v0.2.0/` — see `crate::bindings`'s comment for why.
    wasmtime::component::bindgen!({
        path: "wit/v0.2.0",
        world: "filter",
        exports: { default: async },
    });
}

/// The canonical `plecto:filter@0.3.0` contract text, byte-identical to the vendored
/// `wit/world.wit` this module's `crate::bindings` resolves — so a consumer that needs the raw
/// WIT source (e.g. `plecto new-filter`'s scaffold, ADR 000072) can never drift from what this
/// binary's own host actually runs. Re-exported via `plecto-control` for `plecto-server`, which
/// takes no direct `plecto-host` production dependency.
pub const FILTER_WIT: &str = include_str!("../wit/world.wit");

pub(crate) use crate::bindings::{
    Filter as FilterV03, FilterPre as FilterPreV03, plecto::filter::types as types_v03,
};
pub(crate) use bindings_v01::{
    Filter as FilterV01, FilterPre as FilterPreV01, plecto::filter::types as types_v01,
};
pub(crate) use bindings_v02::{
    Filter as FilterV02, FilterPre as FilterPreV02, plecto::filter::types as types_v02,
};

use crate::{
    Header, HttpRequest, HttpResponse, RequestBodyDecision, RequestDecision, RequestEdit,
    ResponseDecision, ResponseEdit,
};

/// Which `plecto:filter` package version a loaded component targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractVersion {
    V01,
    V02,
    V03,
}

/// Detect the contract version from the component's decoded import names (wasmtime's own
/// validated type information, not a byte scan — a scan can false-positive on a string the
/// guest embeds in a data segment). Keyed on ANY `plecto:filter/…@0.N.` import, not one
/// specific interface: componentization prunes unused imports, so a guest that never
/// logs has no `host-log` import at all.
///
/// Returns `None` (fail-closed at load) when the component imports no `plecto:filter/…`
/// interface, or imports one at an unknown version (e.g. a future `@0.4.`). Only an explicit
/// `@0.1.` / `@0.2.` / `@0.3.` match is accepted — never a silent default to the latest.
pub(crate) fn detect_contract_version(
    component: &wasmtime::component::Component,
    engine: &wasmtime::Engine,
) -> Option<ContractVersion> {
    for (name, _) in component.component_type().imports(engine) {
        if !name.starts_with("plecto:filter/") {
            continue;
        }
        if name.contains("@0.1.") {
            return Some(ContractVersion::V01);
        }
        if name.contains("@0.2.") {
            return Some(ContractVersion::V02);
        }
        if name.contains("@0.3.") {
            return Some(ContractVersion::V03);
        }
        // A `plecto:filter/…` import at an unrecognised version — do not guess.
        return None;
    }
    None
}

pub(crate) fn request_to_v01(req: &HttpRequest) -> types_v01::HttpRequest {
    types_v01::HttpRequest {
        method: req.method.clone(),
        path: req.path.clone(),
        authority: req.authority.clone(),
        scheme: req.scheme.clone(),
        headers: req
            .headers
            .iter()
            .map(|h| types_v01::Header {
                name: h.name.clone(),
                value: String::from_utf8_lossy(&h.value).into_owned(),
            })
            .collect(),
    }
}

pub(crate) fn response_to_v01(resp: &HttpResponse) -> types_v01::HttpResponse {
    types_v01::HttpResponse {
        status: resp.status,
        headers: resp
            .headers
            .iter()
            .map(|h| types_v01::Header {
                name: h.name.clone(),
                value: String::from_utf8_lossy(&h.value).into_owned(),
            })
            .collect(),
        body: resp.body.clone(),
    }
}

/// Project the canonical (0.3-shaped) request into the frozen 0.2 record: the same
/// byte-valued shape, so this is a mechanical per-field clone, not a lossy projection
/// (unlike [`request_to_v01`]).
pub(crate) fn request_to_v02(req: &HttpRequest) -> types_v02::HttpRequest {
    types_v02::HttpRequest {
        method: req.method.clone(),
        path: req.path.clone(),
        authority: req.authority.clone(),
        scheme: req.scheme.clone(),
        headers: req
            .headers
            .iter()
            .map(|h| types_v02::Header {
                name: h.name.clone(),
                value: h.value.clone(),
            })
            .collect(),
    }
}

pub(crate) fn response_to_v02(resp: &HttpResponse) -> types_v02::HttpResponse {
    types_v02::HttpResponse {
        status: resp.status,
        headers: resp
            .headers
            .iter()
            .map(|h| types_v02::Header {
                name: h.name.clone(),
                value: h.value.clone(),
            })
            .collect(),
        body: resp.body.clone(),
    }
}

fn header_from_v01(h: types_v01::Header) -> Option<Header> {
    validate_and_header(&h.name, h.value.as_bytes())
}

fn header_from_v02(h: types_v02::Header) -> Option<Header> {
    validate_and_header(&h.name, &h.value)
}

fn header_from_v03(h: types_v03::Header) -> Option<Header> {
    validate_and_header(&h.name, &h.value)
}

const MAX_GUEST_HEADER_NAME_LEN: usize = 256;
const MAX_GUEST_HEADER_VALUE_LEN: usize = 8192;
/// Ceiling on a guest-synthesised response body (`short-circuit` / `replace`). Symmetric to the
/// request-body buffer cap on the fast path: an unbounded guest body is a trivial host OOM.
pub(crate) const MAX_GUEST_RESPONSE_BODY_LEN: usize = 1 << 20; // 1 MiB

/// RFC 9110 §5.6.2 `tchar` — the exact set hyper's `HeaderName::from_bytes` accepts, so a name
/// that passes here can never be silently dropped at the egress `copy_headers` conversion.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

/// RFC 9110 §5.5 field content: HTAB / VCHAR / obs-text (0x80–0xFF permitted — that byte range
/// is the whole point of the `list<u8>` contract). Mirrors hyper's `HeaderValue::from_bytes`,
/// same reason as [`is_tchar`]: no silent drops past this gate.
fn is_field_value_byte(b: u8) -> bool {
    b == b'\t' || (b >= 0x20 && b != 0x7f)
}

/// Hop-by-hop names (RFC 9110 §7.6.1) a guest cannot meaningfully set — the fast path strips
/// them at egress. Kept in sync with the strip list in `plecto-server::headers`. The mappers
/// DROP these instead of failing the decision: the observable behavior (the header never
/// reaches the peer) is what deployments already had, whereas failing closed would turn a
/// filter that harmlessly sets `Connection: close` into an every-request `InvalidOutput`.
const HOP_BY_HOP_GUEST_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];

fn is_hop_by_hop_guest_header(name: &str) -> bool {
    HOP_BY_HOP_GUEST_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
}

/// Validate a guest-supplied header (guest output is untrusted): reject CRLF / CTLs / non-tchar
/// names / oversize, fail-closed instead of trapping. Alignment with hyper's accepted sets means
/// everything admitted here survives egress byte-for-byte. Hop-by-hop names never reach this
/// gate — the mappers below drop them first (see [`HOP_BY_HOP_GUEST_HEADERS`]).
fn validate_and_header(name: &str, value: &[u8]) -> Option<Header> {
    if name.is_empty() || name.len() > MAX_GUEST_HEADER_NAME_LEN {
        return None;
    }
    if !name.bytes().all(is_tchar) {
        return None;
    }
    if value.len() > MAX_GUEST_HEADER_VALUE_LEN {
        return None;
    }
    if !value.iter().all(|b| is_field_value_byte(*b)) {
        return None;
    }
    Some(Header {
        name: name.to_string(),
        value: value.to_vec(),
    })
}

fn request_edit_from_v01(edit: types_v01::RequestEdit) -> Option<RequestEdit> {
    let set_headers = edit
        .set_headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v01)
        .collect::<Option<Vec<_>>>()?;
    Some(RequestEdit {
        set_headers,
        remove_headers: edit.remove_headers,
    })
}

fn request_edit_from_v02(edit: types_v02::RequestEdit) -> Option<RequestEdit> {
    let set_headers = edit
        .set_headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v02)
        .collect::<Option<Vec<_>>>()?;
    Some(RequestEdit {
        set_headers,
        remove_headers: edit.remove_headers,
    })
}

fn response_edit_from_v01(edit: types_v01::ResponseEdit) -> Option<ResponseEdit> {
    let set_headers = edit
        .set_headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v01)
        .collect::<Option<Vec<_>>>()?;
    Some(ResponseEdit {
        set_status: match edit.set_status {
            Some(status) => Some(validated_guest_status(status)?),
            None => None,
        },
        set_headers,
        remove_headers: edit.remove_headers,
    })
}

fn response_edit_from_v02(edit: types_v02::ResponseEdit) -> Option<ResponseEdit> {
    let set_headers = edit
        .set_headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v02)
        .collect::<Option<Vec<_>>>()?;
    Some(ResponseEdit {
        set_status: match edit.set_status {
            Some(status) => Some(validated_guest_status(status)?),
            None => None,
        },
        set_headers,
        remove_headers: edit.remove_headers,
    })
}

fn request_edit_from_v03(edit: types_v03::RequestEdit) -> Option<RequestEdit> {
    let set_headers = edit
        .set_headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v03)
        .collect::<Option<Vec<_>>>()?;
    Some(RequestEdit {
        set_headers,
        remove_headers: edit.remove_headers,
    })
}

fn response_edit_from_v03(edit: types_v03::ResponseEdit) -> Option<ResponseEdit> {
    let set_headers = edit
        .set_headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v03)
        .collect::<Option<Vec<_>>>()?;
    Some(ResponseEdit {
        set_status: match edit.set_status {
            Some(status) => Some(validated_guest_status(status)?),
            None => None,
        },
        set_headers,
        remove_headers: edit.remove_headers,
    })
}

fn validated_guest_body(body: Vec<u8>) -> Option<Vec<u8>> {
    if body.len() > MAX_GUEST_RESPONSE_BODY_LEN {
        return None;
    }
    Some(body)
}

/// Guest-supplied status codes join the header rules at this gate (the module invariant:
/// everything admitted here survives egress): only 100..=599 — the range hyper's
/// `StatusCode::from_u16` accepts — passes; anything else is rejected as `InvalidOutput`
/// instead of being silently rewritten to 502 downstream (the server's clamp stays as
/// defence in depth).
fn validated_guest_status(status: u16) -> Option<u16> {
    (100..=599).contains(&status).then_some(status)
}

fn response_from_v01(resp: types_v01::HttpResponse) -> Option<HttpResponse> {
    let headers = resp
        .headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v01)
        .collect::<Option<Vec<_>>>()?;
    Some(HttpResponse {
        status: validated_guest_status(resp.status)?,
        headers,
        body: validated_guest_body(resp.body)?,
    })
}

fn response_from_v02(resp: types_v02::HttpResponse) -> Option<HttpResponse> {
    let headers = resp
        .headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v02)
        .collect::<Option<Vec<_>>>()?;
    Some(HttpResponse {
        status: validated_guest_status(resp.status)?,
        headers,
        body: validated_guest_body(resp.body)?,
    })
}

fn response_from_v03(resp: types_v03::HttpResponse) -> Option<HttpResponse> {
    let headers = resp
        .headers
        .into_iter()
        .filter(|h| !is_hop_by_hop_guest_header(&h.name))
        .map(header_from_v03)
        .collect::<Option<Vec<_>>>()?;
    Some(HttpResponse {
        status: validated_guest_status(resp.status)?,
        headers,
        body: validated_guest_body(resp.body)?,
    })
}

/// Map a 0.1 guest decision to native byte-valued types. `Continue` does not rewrite headers.
pub(crate) fn request_decision_from_v01(
    decision: types_v01::RequestDecision,
) -> Option<RequestDecision> {
    match decision {
        types_v01::RequestDecision::Continue => Some(RequestDecision::Continue),
        types_v01::RequestDecision::Modified(edit) => {
            Some(RequestDecision::Modified(request_edit_from_v01(edit)?))
        }
        types_v01::RequestDecision::ShortCircuit(resp) => {
            Some(RequestDecision::ShortCircuit(response_from_v01(resp)?))
        }
    }
}

pub(crate) fn response_decision_from_v01(
    decision: types_v01::ResponseDecision,
) -> Option<ResponseDecision> {
    match decision {
        types_v01::ResponseDecision::Continue => Some(ResponseDecision::Continue),
        types_v01::ResponseDecision::Modified(edit) => {
            Some(ResponseDecision::Modified(response_edit_from_v01(edit)?))
        }
    }
}

pub(crate) fn request_body_decision_from_v01(
    decision: types_v01::RequestBodyDecision,
) -> Option<RequestBodyDecision> {
    match decision {
        types_v01::RequestBodyDecision::Continue(body) => Some(RequestBodyDecision::Continue(body)),
        types_v01::RequestBodyDecision::ShortCircuit(resp) => {
            Some(RequestBodyDecision::ShortCircuit(response_from_v01(resp)?))
        }
    }
}

pub(crate) fn request_decision_from_v02(
    decision: types_v02::RequestDecision,
) -> Option<RequestDecision> {
    match decision {
        types_v02::RequestDecision::Continue => Some(RequestDecision::Continue),
        types_v02::RequestDecision::Modified(edit) => {
            Some(RequestDecision::Modified(request_edit_from_v02(edit)?))
        }
        types_v02::RequestDecision::ShortCircuit(resp) => {
            Some(RequestDecision::ShortCircuit(response_from_v02(resp)?))
        }
    }
}

pub(crate) fn response_decision_from_v02(
    decision: types_v02::ResponseDecision,
) -> Option<ResponseDecision> {
    match decision {
        types_v02::ResponseDecision::Continue => Some(ResponseDecision::Continue),
        types_v02::ResponseDecision::Modified(edit) => {
            Some(ResponseDecision::Modified(response_edit_from_v02(edit)?))
        }
    }
}

pub(crate) fn request_body_decision_from_v02(
    decision: types_v02::RequestBodyDecision,
) -> Option<RequestBodyDecision> {
    match decision {
        types_v02::RequestBodyDecision::Continue(body) => Some(RequestBodyDecision::Continue(body)),
        types_v02::RequestBodyDecision::ShortCircuit(resp) => {
            Some(RequestBodyDecision::ShortCircuit(response_from_v02(resp)?))
        }
    }
}

pub(crate) fn request_decision_from_v03(
    decision: types_v03::RequestDecision,
) -> Option<RequestDecision> {
    match decision {
        types_v03::RequestDecision::Continue => Some(RequestDecision::Continue),
        types_v03::RequestDecision::Modified(edit) => {
            Some(RequestDecision::Modified(request_edit_from_v03(edit)?))
        }
        types_v03::RequestDecision::ShortCircuit(resp) => {
            Some(RequestDecision::ShortCircuit(response_from_v03(resp)?))
        }
    }
}

/// Map a 0.3 guest response decision to the validated native one. `replace` output is
/// untrusted guest data on its way to the client, so it passes the SAME fail-closed header
/// validation as a request-side `short-circuit` (ADR 000073 decision 4).
pub(crate) fn response_decision_from_v03(
    decision: types_v03::ResponseDecision,
) -> Option<ResponseDecision> {
    match decision {
        types_v03::ResponseDecision::Continue => Some(ResponseDecision::Continue),
        types_v03::ResponseDecision::Modified(edit) => {
            Some(ResponseDecision::Modified(response_edit_from_v03(edit)?))
        }
        types_v03::ResponseDecision::Replace(resp) => {
            Some(ResponseDecision::Replace(response_from_v03(resp)?))
        }
    }
}

pub(crate) fn request_body_decision_from_v03(
    decision: types_v03::RequestBodyDecision,
) -> Option<RequestBodyDecision> {
    match decision {
        types_v03::RequestBodyDecision::Continue(body) => Some(RequestBodyDecision::Continue(body)),
        types_v03::RequestBodyDecision::ShortCircuit(resp) => {
            Some(RequestBodyDecision::ShortCircuit(response_from_v03(resp)?))
        }
    }
}

/// Build a canonical header (byte-valued) for callers and tests.
pub fn header(name: impl Into<String>, value: impl AsRef<[u8]>) -> Header {
    Header {
        name: name.into(),
        value: value.as_ref().to_vec(),
    }
}

mod v01_host {
    use super::bindings_v01::plecto::filter::{
        host_clock, host_config, host_counter, host_kv, host_log, host_ratelimit,
    };
    use crate::LogLevel;
    use crate::bindings::plecto::filter::{
        host_clock as host_clock_v03, host_config as host_config_v03,
        host_counter as host_counter_v03, host_kv as host_kv_v03, host_log as host_log_v03,
        host_ratelimit as host_ratelimit_v03,
    };
    use crate::state::HostState;

    impl super::bindings_v01::plecto::filter::types::Host for HostState {}

    fn log_level(level: host_log::Level) -> LogLevel {
        match level {
            host_log::Level::Trace => LogLevel::Trace,
            host_log::Level::Debug => LogLevel::Debug,
            host_log::Level::Info => LogLevel::Info,
            host_log::Level::Warn => LogLevel::Warn,
            host_log::Level::Error => LogLevel::Error,
        }
    }

    impl host_log::Host for HostState {
        fn log(&mut self, level: host_log::Level, message: String) {
            host_log_v03::Host::log(self, log_level(level), message);
        }
    }

    impl host_clock::Host for HostState {
        fn now_ms(&mut self) -> u64 {
            host_clock_v03::Host::now_ms(self)
        }
    }

    impl host_kv::Host for HostState {
        fn get(&mut self, key: String) -> Option<Vec<u8>> {
            host_kv_v03::Host::get(self, key)
        }
        fn set(&mut self, key: String, value: Vec<u8>) {
            host_kv_v03::Host::set(self, key, value);
        }
        fn delete(&mut self, key: String) {
            host_kv_v03::Host::delete(self, key);
        }
    }

    impl host_counter::Host for HostState {
        fn increment(&mut self, key: String, delta: i64) -> i64 {
            host_counter_v03::Host::increment(self, key, delta)
        }
        fn get(&mut self, key: String) -> i64 {
            host_counter_v03::Host::get(self, key)
        }
    }

    impl host_ratelimit::Host for HostState {
        fn try_acquire(&mut self, key: String, cost: u64) -> host_ratelimit::Acquire {
            let out = host_ratelimit_v03::Host::try_acquire(self, key, cost);
            host_ratelimit::Acquire {
                allowed: out.allowed,
                remaining: out.remaining,
                retry_after_ms: out.retry_after_ms,
            }
        }
    }

    impl host_config::Host for HostState {
        fn get(&mut self, key: String) -> Option<String> {
            host_config_v03::Host::get(self, key)
        }
    }
}

mod v02_host {
    use super::bindings_v02::plecto::filter::{
        host_clock, host_config, host_counter, host_kv, host_log, host_ratelimit,
    };
    use crate::LogLevel;
    use crate::bindings::plecto::filter::{
        host_clock as host_clock_v03, host_config as host_config_v03,
        host_counter as host_counter_v03, host_kv as host_kv_v03, host_log as host_log_v03,
        host_ratelimit as host_ratelimit_v03,
    };
    use crate::state::HostState;

    impl super::bindings_v02::plecto::filter::types::Host for HostState {}

    fn log_level(level: host_log::Level) -> LogLevel {
        match level {
            host_log::Level::Trace => LogLevel::Trace,
            host_log::Level::Debug => LogLevel::Debug,
            host_log::Level::Info => LogLevel::Info,
            host_log::Level::Warn => LogLevel::Warn,
            host_log::Level::Error => LogLevel::Error,
        }
    }

    impl host_log::Host for HostState {
        fn log(&mut self, level: host_log::Level, message: String) {
            host_log_v03::Host::log(self, log_level(level), message);
        }
    }

    impl host_clock::Host for HostState {
        fn now_ms(&mut self) -> u64 {
            host_clock_v03::Host::now_ms(self)
        }
    }

    impl host_kv::Host for HostState {
        fn get(&mut self, key: String) -> Option<Vec<u8>> {
            host_kv_v03::Host::get(self, key)
        }
        fn set(&mut self, key: String, value: Vec<u8>) {
            host_kv_v03::Host::set(self, key, value);
        }
        fn delete(&mut self, key: String) {
            host_kv_v03::Host::delete(self, key);
        }
    }

    impl host_counter::Host for HostState {
        fn increment(&mut self, key: String, delta: i64) -> i64 {
            host_counter_v03::Host::increment(self, key, delta)
        }
        fn get(&mut self, key: String) -> i64 {
            host_counter_v03::Host::get(self, key)
        }
    }

    impl host_ratelimit::Host for HostState {
        fn try_acquire(&mut self, key: String, cost: u64) -> host_ratelimit::Acquire {
            let out = host_ratelimit_v03::Host::try_acquire(self, key, cost);
            host_ratelimit::Acquire {
                allowed: out.allowed,
                remaining: out.remaining,
                retry_after_ms: out.retry_after_ms,
            }
        }
    }

    impl host_config::Host for HostState {
        fn get(&mut self, key: String) -> Option<String> {
            host_config_v03::Host::get(self, key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_crlf_in_guest_header_value() {
        assert!(validate_and_header("x", b"a\r\nb").is_none());
        assert!(validate_and_header("x", b"a\rb").is_none());
        assert!(validate_and_header("x", b"a\nb").is_none());
        assert!(validate_and_header("x", b"ok").is_some());
    }

    #[test]
    fn hop_by_hop_guest_headers_are_dropped_not_fatal() {
        // RFC 9110 §7.6.1 names never survive the proxy (the fast path strips them at egress),
        // so the mappers drop them instead of failing the whole decision — a deployed filter
        // that harmlessly sets `Connection: close` must not start failing every request.
        let edit = types_v03::RequestDecision::Modified(types_v03::RequestEdit {
            set_headers: vec![
                types_v03::Header {
                    name: "Connection".to_string(),
                    value: b"close".to_vec(),
                },
                types_v03::Header {
                    name: "x-user".to_string(),
                    value: b"alice".to_vec(),
                },
            ],
            remove_headers: vec![],
        });
        match request_decision_from_v03(edit) {
            Some(RequestDecision::Modified(edit)) => {
                assert_eq!(
                    edit.set_headers.len(),
                    1,
                    "hop-by-hop dropped, the rest kept"
                );
                assert_eq!(edit.set_headers[0].name, "x-user");
            }
            other => panic!("expected Modified, got {other:?}"),
        }
        for name in [
            "Keep-Alive",
            "transfer-encoding",
            "proxy-authorization",
            "TE",
            "upgrade",
        ] {
            let sc = types_v03::RequestDecision::ShortCircuit(types_v03::HttpResponse {
                status: 200,
                headers: vec![types_v03::Header {
                    name: name.to_string(),
                    value: b"x".to_vec(),
                }],
                body: Vec::new(),
            });
            match request_decision_from_v03(sc) {
                Some(RequestDecision::ShortCircuit(resp)) => {
                    assert!(resp.headers.is_empty(), "{name} must be dropped, not fatal");
                }
                other => panic!("expected ShortCircuit, got {other:?}"),
            }
        }
    }

    #[test]
    fn validated_guest_status_accepts_only_http_range() {
        assert_eq!(validated_guest_status(100), Some(100));
        assert_eq!(validated_guest_status(200), Some(200));
        assert_eq!(validated_guest_status(599), Some(599));
        assert_eq!(validated_guest_status(99), None);
        assert_eq!(validated_guest_status(600), None);
        assert_eq!(validated_guest_status(0), None);
        assert_eq!(validated_guest_status(1000), None);
    }

    #[test]
    fn rejects_ctl_bytes_and_non_tchar_names_that_hyper_would_silently_drop() {
        // Anything admitted here must survive the egress `HeaderName`/`HeaderValue::from_bytes`
        // conversion — otherwise a header the guest set vanishes silently instead of failing
        // closed at the contract boundary.
        assert!(
            validate_and_header("x:y", b"v").is_none(),
            "':' is not tchar"
        );
        assert!(
            validate_and_header("x y", b"v").is_none(),
            "space is not tchar"
        );
        assert!(validate_and_header("x", b"a\0b").is_none(), "NUL is a CTL");
        assert!(
            validate_and_header("x", b"a\x7fb").is_none(),
            "DEL is rejected"
        );
        assert!(validate_and_header("", b"v").is_none(), "empty name");
    }

    #[test]
    fn accepts_obs_text_bytes_the_contract_exists_to_carry() {
        // Non-UTF-8 / high bytes are legal field content (RFC 9110 obs-text) — rejecting them
        // would defeat the `list<u8>` lift (ADR 000071).
        let raw: &[u8] = &[0xC3, 0x28, 0xFF];
        let h = validate_and_header("x-blob", raw).expect("obs-text is valid field content");
        assert_eq!(h.value, raw);
        assert!(
            validate_and_header("x", b"tab\tok").is_some(),
            "HTAB is legal"
        );
    }

    #[test]
    fn enforces_size_caps() {
        assert!(validate_and_header(&"n".repeat(MAX_GUEST_HEADER_NAME_LEN), b"v").is_some());
        assert!(validate_and_header(&"n".repeat(MAX_GUEST_HEADER_NAME_LEN + 1), b"v").is_none());
        assert!(validate_and_header("x", &vec![b'a'; MAX_GUEST_HEADER_VALUE_LEN]).is_some());
        assert!(validate_and_header("x", &vec![b'a'; MAX_GUEST_HEADER_VALUE_LEN + 1]).is_none());
    }

    #[test]
    fn v01_projection_is_lossy_but_v01_continue_keeps_native_bytes() {
        // The 0.1 guest sees the lossy string; `continue` never rebuilds headers from it —
        // the host's canonical byte header is what flows on (ADR 000071 decision 2).
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            authority: "a".to_string(),
            scheme: "https".to_string(),
            headers: vec![Header {
                name: "x-blob".to_string(),
                value: vec![0xC3, 0x28],
            }],
        };
        let projected = request_to_v01(&req);
        assert_eq!(projected.headers[0].value, "\u{FFFD}(");
        assert!(matches!(
            request_decision_from_v01(types_v01::RequestDecision::Continue),
            Some(RequestDecision::Continue)
        ));
    }

    #[test]
    fn v01_modified_applies_utf8_bytes_and_fails_closed_on_crlf() {
        let ok = types_v01::RequestDecision::Modified(types_v01::RequestEdit {
            set_headers: vec![types_v01::Header {
                name: "x-user".to_string(),
                value: "alice".to_string(),
            }],
            remove_headers: vec![],
        });
        match request_decision_from_v01(ok) {
            Some(RequestDecision::Modified(edit)) => {
                assert_eq!(edit.set_headers[0].value, b"alice");
            }
            other => panic!("expected Modified, got {other:?}"),
        }

        let bad = types_v01::RequestDecision::Modified(types_v01::RequestEdit {
            set_headers: vec![types_v01::Header {
                name: "x-evil".to_string(),
                value: "a\r\nx-smuggled: 1".to_string(),
            }],
            remove_headers: vec![],
        });
        assert!(
            request_decision_from_v01(bad).is_none(),
            "CRLF fails closed"
        );
    }

    #[test]
    fn v03_replace_output_passes_the_same_fail_closed_validation_as_short_circuit() {
        // ADR 000073 decision 4: `replace` output is untrusted guest data headed for the client,
        // so it passes the SAME header validation as a request-side short-circuit. CRLF fails
        // closed (the mapper returns None → RunError::InvalidOutput); clean bytes pass intact.
        let bad = types_v03::ResponseDecision::Replace(types_v03::HttpResponse {
            status: 200,
            headers: vec![types_v03::Header {
                name: "x-evil".to_string(),
                value: b"a\r\nx-smuggled: 1".to_vec(),
            }],
            body: b"payload".to_vec(),
        });
        assert!(
            response_decision_from_v03(bad).is_none(),
            "CRLF in a replace header fails closed"
        );

        let ok = types_v03::ResponseDecision::Replace(types_v03::HttpResponse {
            status: 418,
            headers: vec![types_v03::Header {
                name: "x-blob".to_string(),
                value: vec![0xC3, 0x28, 0xFF],
            }],
            body: b"payload".to_vec(),
        });
        match response_decision_from_v03(ok) {
            Some(ResponseDecision::Replace(resp)) => {
                assert_eq!(resp.status, 418);
                assert_eq!(resp.headers[0].value, vec![0xC3, 0x28, 0xFF]);
                assert_eq!(resp.body, b"payload");
            }
            other => panic!("expected Replace, got {other:?}"),
        }
    }

    #[test]
    fn v02_projection_is_byte_faithful() {
        // The 0.2 projection is a mechanical clone (same byte-valued shape), NOT the 0.1
        // lossy-UTF-8 form: non-UTF-8 header bytes survive verbatim.
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            authority: "a".to_string(),
            scheme: "https".to_string(),
            headers: vec![Header {
                name: "x-blob".to_string(),
                value: vec![0xC3, 0x28],
            }],
        };
        let projected = request_to_v02(&req);
        assert_eq!(projected.headers[0].value, vec![0xC3, 0x28]);
        let resp = HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "x-blob".to_string(),
                value: vec![0xFF],
            }],
            body: b"b".to_vec(),
        };
        assert_eq!(response_to_v02(&resp).headers[0].value, vec![0xFF]);
    }

    #[test]
    fn detects_version_from_decoded_imports_not_bytes() {
        // Real components prune unused imports (the MoonBit fixture has no host-kv/clock/config),
        // so detection must key on ANY plecto:filter interface — and must read the decoded type
        // structure, not raw bytes. Unknown / absent versions fail closed (`None`) at load.
        let engine = wasmtime::Engine::default();
        let cases: &[(&str, Option<ContractVersion>)] = &[
            (
                r#"(component (import "plecto:filter/host-log@0.1.0" (instance)))"#,
                Some(ContractVersion::V01),
            ),
            // a 0.1 guest that never logs: host-log is pruned, another interface remains.
            (
                r#"(component (import "plecto:filter/host-clock@0.1.0" (instance)))"#,
                Some(ContractVersion::V01),
            ),
            (
                r#"(component (import "plecto:filter/host-log@0.2.0" (instance)))"#,
                Some(ContractVersion::V02),
            ),
            (
                r#"(component (import "plecto:filter/host-log@0.3.0" (instance)))"#,
                Some(ContractVersion::V03),
            ),
            (
                r#"(component (import "plecto:filter/host-clock@0.2.0" (instance)))"#,
                Some(ContractVersion::V02),
            ),
            // no plecto import at all: fail closed at load (do not guess V03).
            (r"(component)", None),
            // future / unknown track: fail closed (do not silently bind as V03).
            (
                r#"(component (import "plecto:filter/host-log@0.4.0" (instance)))"#,
                None,
            ),
        ];
        for (wat, want) in cases {
            let component =
                wasmtime::component::Component::new(&engine, wat).expect("valid component wat");
            assert_eq!(
                detect_contract_version(&component, &engine),
                *want,
                "wat: {wat}"
            );
        }
    }

    #[test]
    fn oversize_guest_response_body_fails_closed() {
        let ok = types_v03::ResponseDecision::Replace(types_v03::HttpResponse {
            status: 200,
            headers: vec![],
            body: vec![0u8; MAX_GUEST_RESPONSE_BODY_LEN],
        });
        assert!(response_decision_from_v03(ok).is_some());

        let over = types_v03::ResponseDecision::Replace(types_v03::HttpResponse {
            status: 200,
            headers: vec![],
            body: vec![0u8; MAX_GUEST_RESPONSE_BODY_LEN + 1],
        });
        assert!(
            response_decision_from_v03(over).is_none(),
            "a synthesised body over the cap must fail closed"
        );
    }
}
