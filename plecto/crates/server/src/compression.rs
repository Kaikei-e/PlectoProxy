//! Native response compression (`[route.compression]`, ADR 000074): RFC 9110 §12.5.3 content
//! negotiation against the client's `Accept-Encoding`, applied AFTER the response filter chain —
//! filters always see the identity representation, and only the streamed upstream body (never a
//! filter-synthesised `replace` / fail-closed response, which the host frames itself) is
//! transformed.
//!
//! Best-effort *before* commit: ineligible response, no acceptable coding, or codec init failure
//! → serve identity (the deliverable response is unchanged). Once `Content-Encoding` is set,
//! the representation is committed — a later encode error can only fail the stream; rewriting
//! to identity mid-body is impossible without lying about the coding already advertised.
//!
//! The encoders run inline on the async thread, one frame at a time with a sync flush per frame:
//! frame-sized CPU (the levels here compress a 16 KiB frame in tens of µs), and the flush keeps
//! a trickling upstream trickling to the client instead of stalling inside a codec buffer.

use std::collections::VecDeque;
use std::io::Write;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::Frame;
use hyper::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE,
    ETAG, HeaderMap, HeaderValue, VARY,
};
use hyper::{Response, StatusCode};
use plecto_control::{CompressionAlgorithm, CompressionConfig, RouteInfo};

use crate::{BoxError, ResponseBody};

/// gzip level 5: the balanced on-the-fly default (zlib's own default is 6; level 1 is the
/// throughput-first extreme). Matches contemporary proxy practice.
const GZIP_LEVEL: u32 = 5;
/// Brotli quality 4: the highest quality band that stays in gzip's CPU class for dynamic
/// content (q10–11 are for ahead-of-time static assets).
const BROTLI_QUALITY: u32 = 4;
/// Brotli window 2^22 = 4 MiB.
const BROTLI_LGWIN: u32 = 22;
/// zstd level 3 — libzstd's own default, and the converged on-the-fly proxy setting.
const ZSTD_LEVEL: i32 = 3;
/// zstd window cap 2^23 = 8 MiB: RFC 9659 forbids serving web content with a larger window
/// (browser decoders reject it), so pin it rather than trust a level's implicit choice.
const ZSTD_WINDOW_LOG: u32 = 23;
/// Brotli's internal buffer size (bytes) between our writes and its emit callback.
const BROTLI_BUFFER: usize = 4096;

/// Apply this route's `[route.compression]` (if opted in) to an about-to-be-sent response.
/// Negotiates against the ORIGINAL client request — the party the compressed bytes are for —
/// not the chain-edited forward. HEAD is skipped whole: there is no body to transform, and the
/// head must keep describing the identity representation a GET would have returned.
pub(crate) fn apply(
    resp: Response<ResponseBody>,
    route: &RouteInfo,
    request: &hyper::http::request::Parts,
) -> Response<ResponseBody> {
    let Some(cfg) = route.compression.as_deref() else {
        return resp;
    };
    if request.method == hyper::Method::HEAD {
        return resp;
    }
    compress_response(resp, &request.headers, cfg)
}

fn compress_response(
    resp: Response<ResponseBody>,
    request_headers: &HeaderMap,
    cfg: &CompressionConfig,
) -> Response<ResponseBody> {
    let (mut parts, body) = resp.into_parts();
    if !response_eligible(parts.status, &parts.headers, cfg) {
        return Response::from_parts(parts, body);
    }
    // An eligible response varies by Accept-Encoding even when identity is chosen below — a
    // shared cache holding these identity bytes must not serve them to a gzip-capable client
    // as THE representation (nor a compressed one to a client that never asked).
    add_vary_accept_encoding(&mut parts.headers);
    let Some(algo) = negotiate(request_headers, cfg.algorithms()) else {
        return Response::from_parts(parts, body);
    };
    // Codec init failure (allocation, parameter rejection): serve identity, never a 5xx — the
    // response itself is fine, only the optimisation is unavailable.
    let Ok(encoder) = Encoder::new(algo) else {
        return Response::from_parts(parts, body);
    };
    mark_compressed(&mut parts.headers, algo);
    let compressed = CompressBody {
        inner: body,
        encoder: Some(parking_lot::Mutex::new(encoder)),
        queue: VecDeque::new(),
    };
    Response::from_parts(parts, http_body_util::BodyExt::boxed(compressed))
}

/// RFC 9110 §12.5.3: pick the coding to send from what the route offers ∩ what the client
/// accepts. The client's non-zero maximum qvalue wins; a tie falls to the route's configured
/// order (server preference — qvalues order nothing among equals). `*` covers content codings
/// not explicitly listed; `q=0` excludes. An explicitly weighted `identity` competes on the
/// same qvalue scale: a coding is chosen only when its q is strictly greater than identity's
/// (equal q → server preference may still compress). Absence of `Accept-Encoding` → `None`
/// (identity): RFC 9110 treats absence as "any coding acceptable", but transforming without a
/// stated preference is a conservative decline — a proxy MAY transform, and declining is always
/// safe (never a 406).
fn negotiate(
    request_headers: &HeaderMap,
    algorithms: &[CompressionAlgorithm],
) -> Option<CompressionAlgorithm> {
    // Thousandths (RFC 9110 §12.4.2 allows 3 decimals), one slot per coding we can produce.
    let mut explicit = CodingWeights::default();
    let mut star: Option<u16> = None;
    let mut identity_q: Option<u16> = None;
    let mut saw_header = false;
    for value in request_headers.get_all(ACCEPT_ENCODING) {
        saw_header = true;
        // A non-ASCII Accept-Encoding is malformed; skip the value, not the whole negotiation.
        let Ok(s) = value.to_str() else { continue };
        for member in s.split(',') {
            let mut params = member.split(';');
            let coding = params.next().unwrap_or("").trim();
            if coding.is_empty() {
                continue;
            }
            let mut q: u16 = 1000;
            let mut malformed = false;
            for param in params {
                let Some((name, val)) = param.split_once('=') else {
                    malformed = true;
                    break;
                };
                if name.trim().eq_ignore_ascii_case("q") {
                    match parse_qvalue(val.trim()) {
                        Some(v) => q = v,
                        // A garbled weight must not grant acceptance at full strength.
                        None => {
                            malformed = true;
                            break;
                        }
                    }
                }
            }
            if malformed {
                continue;
            }
            if coding == "*" {
                star = Some(q);
            } else if coding.eq_ignore_ascii_case("identity") {
                identity_q = Some(q);
            } else if let Some(algo) = coding_algo(coding) {
                *explicit.slot(algo) = Some(q);
            }
        }
    }
    // No Accept-Encoding field at all → do not transform (see fn docs).
    if !saw_header {
        return None;
    }

    let mut best: Option<(u16, CompressionAlgorithm)> = None;
    for algo in algorithms {
        let q = explicit.slot(*algo).or(star).unwrap_or(0);
        if q == 0 {
            continue;
        }
        // Strictly greater keeps the FIRST (most-preferred) algorithm on a qvalue tie.
        if best.is_none_or(|(bq, _)| q > bq) {
            best = Some((q, *algo));
        }
    }
    let (bq, algo) = best?;
    // Explicit `identity` is a peer on the qvalue scale (RFC 9110 §12.5.3). Only compress when
    // the chosen coding is *strictly* preferred; equal q leaves the choice to server preference
    // (we may compress). When `identity` is omitted, it stays acceptable by default (rule 2)
    // but does not invent a competing weight against explicitly listed content codings.
    if identity_q.is_some_and(|iq| bq < iq) {
        return None;
    }
    Some(algo)
}

/// The client's explicit weight per coding we can produce (thousandths). A struct, not an array
/// indexed by discriminant: no indexing on the data plane, and `match` keeps a future coding a
/// compile error instead of a silent slot mismatch.
#[derive(Default)]
struct CodingWeights {
    zstd: Option<u16>,
    br: Option<u16>,
    gzip: Option<u16>,
}

impl CodingWeights {
    fn slot(&mut self, algo: CompressionAlgorithm) -> &mut Option<u16> {
        match algo {
            CompressionAlgorithm::Zstd => &mut self.zstd,
            CompressionAlgorithm::Br => &mut self.br,
            CompressionAlgorithm::Gzip => &mut self.gzip,
        }
    }
}

/// The coding a client token names, if it is one we can produce. `x-gzip` is gzip's deprecated
/// alias a recipient SHOULD still honour (RFC 9110 §18.6).
fn coding_algo(token: &str) -> Option<CompressionAlgorithm> {
    if token.eq_ignore_ascii_case("zstd") {
        Some(CompressionAlgorithm::Zstd)
    } else if token.eq_ignore_ascii_case("br") {
        Some(CompressionAlgorithm::Br)
    } else if token.eq_ignore_ascii_case("gzip") || token.eq_ignore_ascii_case("x-gzip") {
        Some(CompressionAlgorithm::Gzip)
    } else {
        None
    }
}

/// Parse an RFC 9110 §12.4.2 qvalue (`0`–`1`, up to 3 decimals) into thousandths, or `None` for
/// anything outside the grammar.
fn parse_qvalue(s: &str) -> Option<u16> {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match int {
        "0" => {
            let mut thousandths: u16 = 0;
            let mut place: u16 = 100;
            for b in frac.bytes() {
                thousandths += u16::from(b - b'0') * place;
                place /= 10;
            }
            Some(thousandths)
        }
        "1" => frac.bytes().all(|b| b == b'0').then_some(1000),
        _ => None,
    }
}

/// Is this response one we may and should compress? Everything here is a SKIP condition —
/// declining to transform is always correct.
fn response_eligible(status: StatusCode, headers: &HeaderMap, cfg: &CompressionConfig) -> bool {
    // 204/304 have no content; 206 is a byte range OF the identity representation — compressing
    // the fragment would desync it from the Content-Range math and from other ranges.
    if status.is_informational() || matches!(status.as_u16(), 204 | 206 | 304) {
        return false;
    }
    // Already encoded (e.g. the upstream honoured the forwarded Accept-Encoding itself).
    if headers.contains_key(CONTENT_ENCODING) {
        return false;
    }
    // `Cache-Control: no-transform` — a proxy MUST NOT transform the content (RFC 9110 §7.7).
    if cache_control_no_transform(headers) {
        return false;
    }
    // Content-type allowlist, on the essence (`type/subtype`, parameters ignored). No declared
    // type → unknown payload → leave it alone.
    let essence_eligible = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s))
        .is_some_and(|essence| cfg.content_type_eligible(essence));
    if !essence_eligible {
        return false;
    }
    // The declared-length floor. An unparseable declared length is a response weird enough not
    // to touch; NO declared length (chunked / h2 stream) is normal and stays eligible.
    match headers.get(CONTENT_LENGTH) {
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|len| len >= cfg.min_length()),
        None => true,
    }
}

fn cache_control_no_transform(headers: &HeaderMap) -> bool {
    headers.get_all(CACHE_CONTROL).iter().any(|v| {
        v.to_str().is_ok_and(|s| {
            s.split(',').any(|directive| {
                directive
                    .split('=')
                    .next()
                    .is_some_and(|name| name.trim().eq_ignore_ascii_case("no-transform"))
            })
        })
    })
}

/// Declare that this response varies by `Accept-Encoding`, once — an existing `accept-encoding`
/// member (or a `Vary: *`) already says so.
fn add_vary_accept_encoding(headers: &mut HeaderMap) {
    let already = headers.get_all(VARY).iter().any(|v| {
        v.to_str().is_ok_and(|s| {
            s.split(',').any(|member| {
                let member = member.trim();
                member == "*" || member.eq_ignore_ascii_case("accept-encoding")
            })
        })
    });
    if !already {
        headers.append(VARY, HeaderValue::from_static("accept-encoding"));
    }
}

/// Rewrite the headers for the transformed representation: the new coding, no stale identity
/// `Content-Length` (hyper re-frames the stream), drop `Accept-Ranges` (byte ranges named the
/// identity representation; advertising them on a content-coded body invites mismatched Range
/// math — RFC 9110 §14), and a WEAKENED `ETag` — the compressed bytes are a different
/// representation, so a strong validator minted for the identity bytes must not survive onto
/// them (RFC 9110 §8.8.3); prefixing `W/` keeps `If-None-Match` revalidation working, unlike
/// stripping.
fn mark_compressed(headers: &mut HeaderMap, algo: CompressionAlgorithm) {
    headers.insert(CONTENT_ENCODING, HeaderValue::from_static(algo.token()));
    headers.remove(CONTENT_LENGTH);
    headers.remove(ACCEPT_RANGES);
    if let Some(etag) = headers.get(ETAG) {
        let bytes = etag.as_bytes();
        if !bytes.starts_with(b"W/") {
            let mut weak = Vec::with_capacity(bytes.len() + 2);
            weak.extend_from_slice(b"W/");
            weak.extend_from_slice(bytes);
            match HeaderValue::from_bytes(&weak) {
                Ok(v) => {
                    headers.insert(ETAG, v);
                }
                // Unreachable (a valid value stays valid under an ASCII prefix), but stay total:
                // dropping the validator beats forwarding a strong one for transformed bytes.
                Err(_) => {
                    headers.remove(ETAG);
                }
            }
        }
    }
}

/// One streaming encoder. All three write into an owned `Vec<u8>` the caller drains after each
/// flush — so a frame in yields at most one (compressed) frame out, no internal frame queue.
enum Encoder {
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Br(Box<brotli::CompressorWriter<Vec<u8>>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl Encoder {
    fn new(algo: CompressionAlgorithm) -> std::io::Result<Self> {
        match algo {
            CompressionAlgorithm::Gzip => Ok(Encoder::Gzip(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::new(GZIP_LEVEL),
            ))),
            CompressionAlgorithm::Br => Ok(Encoder::Br(Box::new(brotli::CompressorWriter::new(
                Vec::new(),
                BROTLI_BUFFER,
                BROTLI_QUALITY,
                BROTLI_LGWIN,
            )))),
            CompressionAlgorithm::Zstd => {
                let mut enc = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL)?;
                enc.set_parameter(zstd::stream::raw::CParameter::WindowLog(ZSTD_WINDOW_LOG))?;
                Ok(Encoder::Zstd(enc))
            }
        }
    }

    /// Compress one body frame and hand back everything the codec emitted for it. The sync
    /// flush trades a few percent of ratio for streaming honesty: every upstream frame yields
    /// client-decodable bytes now, instead of parking inside the codec until some later frame.
    fn write_flush(&mut self, data: &[u8]) -> std::io::Result<Bytes> {
        let buf = match self {
            Encoder::Gzip(w) => {
                w.write_all(data)?;
                w.flush()?;
                w.get_mut()
            }
            Encoder::Br(w) => {
                w.write_all(data)?;
                w.flush()?;
                w.get_mut()
            }
            Encoder::Zstd(w) => {
                w.write_all(data)?;
                w.flush()?;
                w.get_mut()
            }
        };
        Ok(Bytes::from(std::mem::take(buf)))
    }

    /// Terminate the stream (final block + checksum/trailer) and return the closing bytes.
    fn finish(self) -> std::io::Result<Bytes> {
        let buf = match self {
            Encoder::Gzip(w) => w.finish()?,
            Encoder::Br(w) => w.into_inner(),
            Encoder::Zstd(w) => w.finish()?,
        };
        Ok(Bytes::from(buf))
    }
}

/// The compressed view of a streamed upstream body: pulls the inner body's frames, compresses
/// data frames one-to-one (sync flush per frame), passes trailers through AFTER terminating the
/// coded stream, and appends the codec's closing bytes at end-of-body.
///
/// The `Mutex` is a zero-cost `Sync` wrapper, never contended: `ResponseBody` is a `BoxBody`
/// (`dyn Body + Send + Sync`), but a zstd context is `Send`-only — and `poll_frame` holds
/// `&mut self`, so every access goes through `Mutex::get_mut` (no lock instruction at all).
struct CompressBody {
    inner: ResponseBody,
    encoder: Option<parking_lot::Mutex<Encoder>>,
    queue: VecDeque<Frame<Bytes>>,
}

impl hyper::body::Body for CompressBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        loop {
            if let Some(frame) = this.queue.pop_front() {
                return Poll::Ready(Some(Ok(frame)));
            }
            // Encoder gone = the coded stream was terminated (end-of-body or trailers seen) and
            // the queue is drained: the body is complete. Trailers are terminal in `http_body`,
            // so the inner body is never polled past this point.
            if this.encoder.is_none() {
                return Poll::Ready(None);
            }
            match std::task::ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        if data.is_empty() {
                            continue;
                        }
                        let Some(encoder) = this.encoder.as_mut() else {
                            continue;
                        };
                        match encoder.get_mut().write_flush(&data) {
                            Ok(out) => {
                                if !out.is_empty() {
                                    this.queue.push_back(Frame::data(out));
                                }
                            }
                            Err(e) => return Poll::Ready(Some(Err(Box::new(e)))),
                        }
                    }
                    // A non-data frame (trailers): terminate the coded stream first, then let
                    // the trailers follow the closing bytes.
                    Err(other) => {
                        if let Some(encoder) = this.encoder.take() {
                            match encoder.into_inner().finish() {
                                Ok(out) => {
                                    if !out.is_empty() {
                                        this.queue.push_back(Frame::data(out));
                                    }
                                }
                                Err(e) => return Poll::Ready(Some(Err(Box::new(e)))),
                            }
                        }
                        this.queue.push_back(other);
                    }
                },
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => {
                    if let Some(encoder) = this.encoder.take() {
                        match encoder.into_inner().finish() {
                            Ok(out) => {
                                if !out.is_empty() {
                                    this.queue.push_back(Frame::data(out));
                                }
                            }
                            Err(e) => return Poll::Ready(Some(Err(Box::new(e)))),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn accept(value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(ACCEPT_ENCODING, HeaderValue::from_str(value).unwrap());
        map
    }

    fn all() -> Vec<CompressionAlgorithm> {
        vec![
            CompressionAlgorithm::Zstd,
            CompressionAlgorithm::Br,
            CompressionAlgorithm::Gzip,
        ]
    }

    #[test]
    fn qvalue_grammar_is_enforced() {
        // RFC 9110 §12.4.2: 0/1 with up to three decimals; anything else is not a qvalue.
        for (s, expect) in [
            ("0", Some(0)),
            ("0.", Some(0)),
            ("0.5", Some(500)),
            ("0.85", Some(850)),
            ("0.855", Some(855)),
            ("1", Some(1000)),
            ("1.0", Some(1000)),
            ("1.000", Some(1000)),
            ("1.001", None),
            ("0.8555", None),
            ("2", None),
            ("-1", None),
            ("abc", None),
            ("0.x", None),
        ] {
            assert_eq!(parse_qvalue(s), expect, "qvalue {s:?}");
        }
    }

    #[test]
    fn negotiate_follows_qvalues_then_server_preference() {
        // Highest non-zero qvalue wins regardless of listing order.
        assert_eq!(
            negotiate(&accept("gzip;q=1.0, br;q=0.5"), &all()),
            Some(CompressionAlgorithm::Gzip)
        );
        // A tie falls to the route's order (zstd first in the default).
        assert_eq!(
            negotiate(&accept("gzip, br, zstd"), &all()),
            Some(CompressionAlgorithm::Zstd)
        );
        // q=0 excludes; all-zero → identity.
        assert_eq!(negotiate(&accept("gzip;q=0"), &all()), None);
        // `*` covers unlisted codings — and q=0 on `*` excludes them.
        assert_eq!(
            negotiate(&accept("*"), &all()),
            Some(CompressionAlgorithm::Zstd)
        );
        assert_eq!(
            negotiate(&accept("gzip;q=0.5, *;q=0"), &all()),
            Some(CompressionAlgorithm::Gzip)
        );
        // An explicit q=0 beats a permissive `*` for that coding.
        assert_eq!(
            negotiate(&accept("zstd;q=0, *;q=0.1"), &all()),
            Some(CompressionAlgorithm::Br)
        );
        // Explicit `identity` competes on qvalue (RFC 9110 §12.5.3).
        assert_eq!(
            negotiate(&accept("identity;q=1.0, gzip;q=0.5"), &all()),
            None,
            "identity preferred → do not transform"
        );
        assert_eq!(
            negotiate(&accept("gzip;q=1.0, identity;q=0.5"), &all()),
            Some(CompressionAlgorithm::Gzip),
            "gzip preferred → compress"
        );
        assert_eq!(
            negotiate(&accept("identity;q=1.0, gzip;q=1.0"), &all()),
            Some(CompressionAlgorithm::Gzip),
            "equal q → server preference may compress"
        );
        assert_eq!(
            negotiate(&accept("identity;q=0, gzip"), &all()),
            Some(CompressionAlgorithm::Gzip),
            "identity excluded → compress"
        );
        // Omitted identity does not invent a competing weight against listed codings.
        assert_eq!(
            negotiate(&accept("gzip, br, zstd"), &all()),
            Some(CompressionAlgorithm::Zstd)
        );
        // No header → conservative identity (see negotiate docs).
        assert_eq!(negotiate(&HeaderMap::new(), &all()), None);
        // The deprecated alias still names gzip (RFC 9110 §18.6).
        assert_eq!(
            negotiate(&accept("x-gzip"), &all()),
            Some(CompressionAlgorithm::Gzip)
        );
        // A malformed weight must not grant acceptance.
        assert_eq!(negotiate(&accept("gzip;q=banana"), &all()), None);
        assert_eq!(negotiate(&accept("gzip;q"), &all()), None);
        // The route only offers what it configured.
        assert_eq!(
            negotiate(&accept("zstd, gzip;q=0.1"), &[CompressionAlgorithm::Gzip]),
            Some(CompressionAlgorithm::Gzip)
        );
    }

    fn eligible_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("text/html"));
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("5000"));
        h
    }

    fn cfg() -> CompressionConfig {
        CompressionConfig::new(&plecto_control::RouteCompression {
            algorithms: all(),
            min_length: 1024,
            content_types: vec!["text/html".to_string()],
        })
    }

    #[test]
    fn eligibility_is_a_conjunction_of_skips() {
        let cfg = cfg();
        assert!(response_eligible(StatusCode::OK, &eligible_headers(), &cfg));

        for status in [
            StatusCode::NO_CONTENT,
            StatusCode::PARTIAL_CONTENT,
            StatusCode::NOT_MODIFIED,
        ] {
            assert!(
                !response_eligible(status, &eligible_headers(), &cfg),
                "{status} must not be transformed"
            );
        }

        let mut h = eligible_headers();
        h.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        assert!(
            !response_eligible(StatusCode::OK, &h, &cfg),
            "an already-encoded response is skipped"
        );

        let mut h = eligible_headers();
        h.insert(
            CACHE_CONTROL,
            HeaderValue::from_static("max-age=60, No-Transform"),
        );
        assert!(
            !response_eligible(StatusCode::OK, &h, &cfg),
            "no-transform is a MUST NOT, matched as a token case-insensitively"
        );

        let mut h = eligible_headers();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
        assert!(!response_eligible(StatusCode::OK, &h, &cfg));

        let mut h = eligible_headers();
        h.remove(CONTENT_TYPE);
        assert!(
            !response_eligible(StatusCode::OK, &h, &cfg),
            "no declared type — unknown payloads are left alone"
        );

        let mut h = eligible_headers();
        h.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("TEXT/HTML; charset=utf-8"),
        );
        assert!(
            response_eligible(StatusCode::OK, &h, &cfg),
            "the essence match ignores case and parameters"
        );

        let mut h = eligible_headers();
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("100"));
        assert!(!response_eligible(StatusCode::OK, &h, &cfg), "below floor");

        let mut h = eligible_headers();
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("banana"));
        assert!(
            !response_eligible(StatusCode::OK, &h, &cfg),
            "an unparseable declared length is too weird to touch"
        );

        let mut h = eligible_headers();
        h.remove(CONTENT_LENGTH);
        assert!(
            response_eligible(StatusCode::OK, &h, &cfg),
            "no declared length (chunked / h2 stream) stays eligible"
        );
    }

    #[test]
    fn vary_is_added_once_and_respects_existing_declarations() {
        let mut h = HeaderMap::new();
        add_vary_accept_encoding(&mut h);
        add_vary_accept_encoding(&mut h);
        assert_eq!(h.get_all(VARY).iter().count(), 1);

        let mut h = HeaderMap::new();
        h.insert(VARY, HeaderValue::from_static("Accept-Encoding, Origin"));
        add_vary_accept_encoding(&mut h);
        assert_eq!(h.get_all(VARY).iter().count(), 1, "already declared");

        let mut h = HeaderMap::new();
        h.insert(VARY, HeaderValue::from_static("*"));
        add_vary_accept_encoding(&mut h);
        assert_eq!(h.get_all(VARY).iter().count(), 1, "`*` already covers it");

        let mut h = HeaderMap::new();
        h.insert(VARY, HeaderValue::from_static("origin"));
        add_vary_accept_encoding(&mut h);
        assert_eq!(h.get_all(VARY).iter().count(), 2, "appended alongside");
    }

    #[test]
    fn mark_compressed_rewrites_the_representation_headers() {
        let mut h = eligible_headers();
        h.insert(ETAG, HeaderValue::from_static("\"v1\""));
        h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        mark_compressed(&mut h, CompressionAlgorithm::Br);
        assert_eq!(h.get(CONTENT_ENCODING).unwrap(), "br");
        assert!(h.get(CONTENT_LENGTH).is_none());
        assert!(
            h.get(ACCEPT_RANGES).is_none(),
            "Accept-Ranges named the identity representation — drop it on transform"
        );
        assert_eq!(h.get(ETAG).unwrap(), "W/\"v1\"");

        // An already-weak validator is left as-is (no W/W/ stutter).
        let mut h = eligible_headers();
        h.insert(ETAG, HeaderValue::from_static("W/\"v1\""));
        mark_compressed(&mut h, CompressionAlgorithm::Gzip);
        assert_eq!(h.get(ETAG).unwrap(), "W/\"v1\"");
    }

    /// A hand-rolled inner body: data frames, then optional trailers — the shapes hyper's
    /// `Incoming` can yield, without a socket.
    struct ScriptedBody {
        frames: VecDeque<Frame<Bytes>>,
    }

    impl hyper::body::Body for ScriptedBody {
        type Data = Bytes;
        type Error = BoxError;
        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
            Poll::Ready(self.get_mut().frames.pop_front().map(Ok))
        }
    }

    fn compress_scripted(
        algo: CompressionAlgorithm,
        frames: Vec<Frame<Bytes>>,
    ) -> (Vec<Bytes>, Option<HeaderMap>) {
        let inner = http_body_util::BodyExt::boxed(ScriptedBody {
            frames: frames.into(),
        });
        let mut body = CompressBody {
            inner,
            encoder: Some(parking_lot::Mutex::new(Encoder::new(algo).unwrap())),
            queue: VecDeque::new(),
        };
        let mut data = Vec::new();
        let mut trailers = None;
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            match hyper::body::Body::poll_frame(Pin::new(&mut body), &mut cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(d) => data.push(d),
                    Err(f) => trailers = f.into_trailers().ok(),
                },
                Poll::Ready(Some(Err(e))) => panic!("body error: {e}"),
                Poll::Ready(None) => break,
                Poll::Pending => panic!("scripted body never pends"),
            }
        }
        (data, trailers)
    }

    #[test]
    fn every_frame_yields_decodable_bytes_and_the_stream_terminates() {
        // Two data frames: the per-frame sync flush must emit compressed bytes for EACH (a
        // trickling upstream keeps trickling), and end-of-body must close the coded stream.
        let frames = vec![
            Frame::data(Bytes::from(vec![b'a'; 4096])),
            Frame::data(Bytes::from(vec![b'b'; 4096])),
        ];
        let (chunks, trailers) = compress_scripted(CompressionAlgorithm::Gzip, frames);
        assert!(
            chunks.len() >= 2,
            "each input frame must yield its own compressed output (got {})",
            chunks.len()
        );
        assert!(trailers.is_none());
        let wire: Vec<u8> = chunks.concat();
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(wire.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out.len(), 8192);
        assert!(out[..4096].iter().all(|&b| b == b'a'));
        assert!(out[4096..].iter().all(|&b| b == b'b'));
    }

    #[test]
    fn trailers_pass_through_after_the_coded_stream_closes() {
        // `Frame` is not `Clone` — build the script fresh per codec.
        let frames = || {
            let mut trailer_map = HeaderMap::new();
            trailer_map.insert("grpc-status", HeaderValue::from_static("0"));
            vec![
                Frame::data(Bytes::from(vec![b'x'; 2048])),
                Frame::trailers(trailer_map),
            ]
        };
        for algo in all() {
            let (chunks, trailers) = compress_scripted(algo, frames());
            let trailers = trailers.unwrap_or_else(|| panic!("{algo:?}: trailers dropped"));
            assert_eq!(
                trailers.get("grpc-status").map(|v| v.as_bytes()),
                Some(b"0".as_slice())
            );
            assert!(!chunks.is_empty(), "{algo:?}: no compressed output");
        }
        // The gzip wire must be COMPLETE (trailer written) even though trailers followed it.
        let (chunks, _) = compress_scripted(CompressionAlgorithm::Gzip, frames());
        let wire: Vec<u8> = chunks.concat();
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(wire.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, vec![b'x'; 2048]);
    }

    #[test]
    fn all_three_codecs_roundtrip_via_the_reference_decoders() {
        let payload = b"plecto ".repeat(1000);
        for algo in all() {
            let frames = vec![Frame::data(Bytes::from(payload.clone()))];
            let (chunks, _) = compress_scripted(algo, frames);
            let wire: Vec<u8> = chunks.concat();
            let out = match algo {
                CompressionAlgorithm::Gzip => {
                    let mut out = Vec::new();
                    flate2::read::GzDecoder::new(wire.as_slice())
                        .read_to_end(&mut out)
                        .unwrap();
                    out
                }
                CompressionAlgorithm::Br => {
                    let mut out = Vec::new();
                    brotli::Decompressor::new(wire.as_slice(), 4096)
                        .read_to_end(&mut out)
                        .unwrap();
                    out
                }
                CompressionAlgorithm::Zstd => zstd::decode_all(wire.as_slice()).unwrap(),
            };
            assert_eq!(out, payload, "{algo:?} roundtrip");
            assert!(wire.len() < payload.len(), "{algo:?} actually compressed");
        }
    }
}
