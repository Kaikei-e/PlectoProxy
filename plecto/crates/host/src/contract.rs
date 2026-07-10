//! `plecto:filter` contract version detection and 0.1↔0.2 adapters (ADR 000071).

mod bindings_v01 {
    wasmtime::component::bindgen!({
        path: "../../wit/v0.1.0",
        world: "filter",
        exports: { default: async },
    });
}

pub(crate) use crate::bindings::{
    Filter as FilterV02, FilterPre as FilterPreV02, plecto::filter::types as types_v02,
};
pub(crate) use bindings_v01::{
    Filter as FilterV01, FilterPre as FilterPreV01, plecto::filter::types as types_v01,
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
}

/// Detect the contract version from the component's decoded import names (wasmtime's own
/// validated type information, not a byte scan — a scan can false-positive on a string the
/// guest embeds in a data segment). Keyed on ANY `plecto:filter/…@0.1.` import, not one
/// specific interface: componentization prunes unused imports, so a 0.1 guest that never
/// logs has no `host-log` import at all. A component importing neither version defaults to
/// V02 and fails at `instantiate_pre` with wasmtime's own unknown-import error (fail-closed).
pub(crate) fn detect_contract_version(
    component: &wasmtime::component::Component,
    engine: &wasmtime::Engine,
) -> ContractVersion {
    let is_v01 = component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with("plecto:filter/") && name.contains("@0.1."));
    if is_v01 {
        ContractVersion::V01
    } else {
        ContractVersion::V02
    }
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

fn header_from_v01(h: types_v01::Header) -> Option<Header> {
    validate_and_header(&h.name, h.value.as_bytes())
}

fn header_from_v02(h: types_v02::Header) -> Option<Header> {
    validate_and_header(&h.name, &h.value)
}

const MAX_GUEST_HEADER_NAME_LEN: usize = 256;
const MAX_GUEST_HEADER_VALUE_LEN: usize = 8192;

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

/// Validate a guest-supplied header (guest output is untrusted): reject CRLF / CTLs / non-tchar
/// names / oversize, fail-closed instead of trapping. Alignment with hyper's accepted sets means
/// everything admitted here survives egress byte-for-byte.
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
        .map(header_from_v01)
        .collect::<Option<Vec<_>>>()?;
    Some(ResponseEdit {
        set_status: edit.set_status,
        set_headers,
        remove_headers: edit.remove_headers,
    })
}

fn response_edit_from_v02(edit: types_v02::ResponseEdit) -> Option<ResponseEdit> {
    let set_headers = edit
        .set_headers
        .into_iter()
        .map(header_from_v02)
        .collect::<Option<Vec<_>>>()?;
    Some(ResponseEdit {
        set_status: edit.set_status,
        set_headers,
        remove_headers: edit.remove_headers,
    })
}

fn response_from_v01(resp: types_v01::HttpResponse) -> Option<HttpResponse> {
    let headers = resp
        .headers
        .into_iter()
        .map(header_from_v01)
        .collect::<Option<Vec<_>>>()?;
    Some(HttpResponse {
        status: resp.status,
        headers,
        body: resp.body,
    })
}

fn response_from_v02(resp: types_v02::HttpResponse) -> Option<HttpResponse> {
    let headers = resp
        .headers
        .into_iter()
        .map(header_from_v02)
        .collect::<Option<Vec<_>>>()?;
    Some(HttpResponse {
        status: resp.status,
        headers,
        body: resp.body,
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
        host_clock as host_clock_v02, host_config as host_config_v02,
        host_counter as host_counter_v02, host_kv as host_kv_v02, host_log as host_log_v02,
        host_ratelimit as host_ratelimit_v02,
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
            host_log_v02::Host::log(self, log_level(level), message);
        }
    }

    impl host_clock::Host for HostState {
        fn now_ms(&mut self) -> u64 {
            host_clock_v02::Host::now_ms(self)
        }
    }

    impl host_kv::Host for HostState {
        fn get(&mut self, key: String) -> Option<Vec<u8>> {
            host_kv_v02::Host::get(self, key)
        }
        fn set(&mut self, key: String, value: Vec<u8>) {
            host_kv_v02::Host::set(self, key, value);
        }
        fn delete(&mut self, key: String) {
            host_kv_v02::Host::delete(self, key);
        }
    }

    impl host_counter::Host for HostState {
        fn increment(&mut self, key: String, delta: i64) -> i64 {
            host_counter_v02::Host::increment(self, key, delta)
        }
        fn get(&mut self, key: String) -> i64 {
            host_counter_v02::Host::get(self, key)
        }
    }

    impl host_ratelimit::Host for HostState {
        fn try_acquire(&mut self, key: String, cost: u64) -> host_ratelimit::Acquire {
            let out = host_ratelimit_v02::Host::try_acquire(self, key, cost);
            host_ratelimit::Acquire {
                allowed: out.allowed,
                remaining: out.remaining,
                retry_after_ms: out.retry_after_ms,
            }
        }
    }

    impl host_config::Host for HostState {
        fn get(&mut self, key: String) -> Option<String> {
            host_config_v02::Host::get(self, key)
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
    fn detects_version_from_decoded_imports_not_bytes() {
        // Real components prune unused imports (the MoonBit fixture has no host-kv/clock/config),
        // so detection must key on ANY plecto:filter interface — and must read the decoded type
        // structure, not raw bytes.
        let engine = wasmtime::Engine::default();
        let cases: &[(&str, ContractVersion)] = &[
            (
                r#"(component (import "plecto:filter/host-log@0.1.0" (instance)))"#,
                ContractVersion::V01,
            ),
            // a 0.1 guest that never logs: host-log is pruned, another interface remains.
            (
                r#"(component (import "plecto:filter/host-clock@0.1.0" (instance)))"#,
                ContractVersion::V01,
            ),
            (
                r#"(component (import "plecto:filter/host-log@0.2.0" (instance)))"#,
                ContractVersion::V02,
            ),
            // no plecto import at all: default V02; instantiate_pre rejects it later.
            (r"(component)", ContractVersion::V02),
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
}
