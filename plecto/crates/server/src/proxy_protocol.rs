//! PROXY protocol v2 reception (ADR 000057): restore the real client address behind an L4 load
//! balancer, at the only possible point — after accept, before the TLS handshake / HTTP parse.
//!
//! Two layers, deliberately split:
//! - [`parse_proxy_v2`] — a pure, I/O-free parser over a byte slice (the fuzz target). Written
//!   from the public spec (`proxy-protocol.txt`, HAProxy Technologies) alone; v2 binary form
//!   only, v1 text form is not accepted (ADR 000057).
//! - [`resolve_peer`] — the listener-side I/O: bounded reads under a deadline for trusted peers
//!   (the header is mandatory there), a non-consuming signature peek for untrusted peers (their
//!   bytes belong to TLS/HTTP), every anomaly fail-closed as a typed fault.
//!
//! The module is `pub` so the out-of-workspace fuzz harness (`fuzz/`) can drive the parser; it
//! is not a semver surface (the crate is `publish = false`).

use std::net::SocketAddr;
use std::time::Duration;

use plecto_control::ProxyProtocolTrust;
use tokio::net::TcpStream;

/// The 12-byte v2 signature. Contains an interior NUL — never treat as a C string (spec §2.2).
pub const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// The fixed prefix: signature + version/command + family/protocol + declared length.
pub const PREFIX_LEN: usize = 16;
/// Cap on the self-described length (address block + TLVs). The spec sizes the header to fit a
/// 536-byte minimal TCP segment; 2 KiB is generous for every real sender while bounding the
/// read (ADR 000057 — bounded reads on untrusted input).
pub const MAX_DECLARED_LEN: usize = 2048;

/// A complete, valid v2 header, reduced to what the listener consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyV2 {
    /// `LOCAL`: a proxy-originated connection (health checks). Use the connection's real
    /// endpoints; the declared bytes were skipped as the spec requires.
    Local,
    /// `PROXY` over TCP/IPv4 or TCP/IPv6: the restored original source. The destination
    /// address and any TLVs are parsed past and dropped — nothing downstream consumes them.
    Proxy { src: SocketAddr },
}

/// The parser's verdict over the bytes seen so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    /// A complete, valid header occupying the first `consumed` bytes.
    Complete { decision: ProxyV2, consumed: usize },
    /// Consistent with a v2 header so far, but the buffer holds fewer than `needed` bytes.
    Incomplete { needed: usize },
}

/// Why a byte sequence is not a valid v2 header. Every variant is a connection-fatal fault
/// code (fail-closed, ADR 000057) — there is no "tolerate and pass through".
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProxyV2Error {
    #[error("signature mismatch")]
    BadSignature,
    #[error("unsupported version in version/command byte {0:#04x}")]
    BadVersion(u8),
    #[error("unsupported command in version/command byte {0:#04x}")]
    BadCommand(u8),
    /// `PROXY` with anything but TCP over IPv4/IPv6 (AF_UNSPEC / AF_UNIX / DGRAM / reserved):
    /// rejected per ADR 000057 — the listener is a TCP fast path; an unspecified peer identity
    /// must not silently degrade to the LB's address.
    #[error("unsupported family/protocol byte {0:#04x} (only TCP over IPv4/IPv6 is accepted)")]
    UnsupportedFamilyProtocol(u8),
    #[error("declared length {declared} exceeds the {MAX_DECLARED_LEN}-byte cap")]
    DeclaredLenTooLarge { declared: usize },
    #[error("declared length {declared} is shorter than the {need}-byte address block")]
    AddressBlockTooShort { declared: usize, need: usize },
}

/// Parse a PROXY protocol v2 header from the start of `buf` — pure and total: no I/O, no
/// panics on any input (the fuzz target's invariant, P11). Errors take precedence over
/// `Incomplete` as soon as the offending byte is visible, so a bad peer is cut without
/// waiting for bytes that would change nothing.
pub fn parse_proxy_v2(buf: &[u8]) -> Result<Parsed, ProxyV2Error> {
    // Compare only the bytes present: a mismatch anywhere is final, a strict prefix is not.
    let sig_seen = buf.len().min(SIGNATURE.len());
    if buf.get(..sig_seen) != SIGNATURE.get(..sig_seen) {
        return Err(ProxyV2Error::BadSignature);
    }
    let (Some(&ver_cmd), Some(&fam_proto), Some(len_hi), Some(len_lo)) = (
        buf.get(12),
        buf.get(13),
        buf.get(14).copied(),
        buf.get(15).copied(),
    ) else {
        return Ok(Parsed::Incomplete { needed: PREFIX_LEN });
    };
    if ver_cmd >> 4 != 0x2 {
        return Err(ProxyV2Error::BadVersion(ver_cmd));
    }
    let declared = usize::from(u16::from_be_bytes([len_hi, len_lo]));
    if declared > MAX_DECLARED_LEN {
        return Err(ProxyV2Error::DeclaredLenTooLarge { declared });
    }
    let needed = PREFIX_LEN + declared;
    match ver_cmd & 0x0F {
        // LOCAL: use the real endpoints; family is ignored, but the declared bytes must still
        // be consumed (spec: "must not assume zero is presented for LOCAL connections").
        0x0 => {
            if buf.len() < needed {
                return Ok(Parsed::Incomplete { needed });
            }
            Ok(Parsed::Complete {
                decision: ProxyV2::Local,
                consumed: needed,
            })
        }
        // PROXY: only TCP over IPv4/IPv6 (ADR 000057); anything else is a fault, not a fallback.
        0x1 => {
            let need = match fam_proto {
                0x11 => 12, // AF_INET,  STREAM: src(4) dst(4) src-port(2) dst-port(2)
                0x21 => 36, // AF_INET6, STREAM: src(16) dst(16) src-port(2) dst-port(2)
                other => return Err(ProxyV2Error::UnsupportedFamilyProtocol(other)),
            };
            if declared < need {
                return Err(ProxyV2Error::AddressBlockTooShort { declared, need });
            }
            if buf.len() < needed {
                return Ok(Parsed::Incomplete { needed });
            }
            let block = buf.get(PREFIX_LEN..PREFIX_LEN + need).unwrap_or(&[]);
            let src = match fam_proto {
                0x11 => parse_src_v4(block),
                _ => parse_src_v6(block),
            };
            // `block` is exactly `need` bytes here, so the sub-slices below always exist; the
            // defensive `None` arm keeps the function total without a data-plane panic (P11).
            let Some(src) = src else {
                return Err(ProxyV2Error::AddressBlockTooShort { declared, need });
            };
            Ok(Parsed::Complete {
                decision: ProxyV2::Proxy { src },
                consumed: needed,
            })
        }
        _ => Err(ProxyV2Error::BadCommand(ver_cmd)),
    }
}

/// Source `SocketAddr` out of a 12-byte IPv4 address block (addresses then ports, all
/// network byte order).
fn parse_src_v4(block: &[u8]) -> Option<SocketAddr> {
    let ip: [u8; 4] = block.get(0..4)?.try_into().ok()?;
    let port: [u8; 2] = block.get(8..10)?.try_into().ok()?;
    Some(SocketAddr::from((ip, u16::from_be_bytes(port))))
}

/// Source `SocketAddr` out of a 36-byte IPv6 address block.
fn parse_src_v6(block: &[u8]) -> Option<SocketAddr> {
    let ip: [u8; 16] = block.get(0..16)?.try_into().ok()?;
    let port: [u8; 2] = block.get(32..34)?.try_into().ok()?;
    Some(SocketAddr::from((ip, u16::from_be_bytes(port))))
}

/// Why a connection was cut at the PROXY layer — the fault codes ADR 000057 requires logged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProxyFault {
    #[error("malformed PROXY v2 header: {0}")]
    Header(#[from] ProxyV2Error),
    #[error("PROXY v2 signature from a peer outside the trusted CIDRs")]
    UntrustedHeader,
    #[error("connection ended before a complete PROXY v2 header: {0}")]
    Truncated(std::io::Error),
    #[error("deadline exceeded before a complete PROXY v2 header")]
    Deadline,
}

/// Resolve the peer the rest of the connection should see, per the ADR 000057 receipt rules:
/// a trusted peer MUST present a valid header (`PROXY` → the restored source, `LOCAL` → the
/// real peer); an untrusted peer must NOT (signature detected → cut) — its bytes are only
/// peeked, never consumed, because they belong to the TLS/HTTP stack. Every read is bounded
/// and under `deadline`; every anomaly is a fault (fail-closed), never a pass-through.
pub(crate) async fn resolve_peer(
    stream: &mut TcpStream,
    peer: SocketAddr,
    trusted: &ProxyProtocolTrust,
    deadline: Duration,
) -> Result<SocketAddr, ProxyFault> {
    let work = async {
        if trusted.contains(peer.ip()) {
            read_trusted_header(stream, peer).await
        } else {
            detect_untrusted_header(stream, peer).await
        }
    };
    (tokio::time::timeout(deadline, work).await).unwrap_or(Err(ProxyFault::Deadline))
}

/// A trusted peer MUST open with a valid v2 header: read exactly the fixed prefix, then
/// exactly the self-described remainder — never more, so the bytes after the header (TLS
/// ClientHello / HTTP request) stay in the socket for the next layer.
async fn read_trusted_header(
    stream: &mut TcpStream,
    peer: SocketAddr,
) -> Result<SocketAddr, ProxyFault> {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; PREFIX_LEN];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(ProxyFault::Truncated)?;
    loop {
        match parse_proxy_v2(&buf)? {
            Parsed::Complete { decision, .. } => {
                return Ok(match decision {
                    ProxyV2::Local => peer,
                    ProxyV2::Proxy { src } => src,
                });
            }
            Parsed::Incomplete { needed } => {
                // The parser only reports `Incomplete` with `needed` beyond what it saw, so
                // the loop strictly grows the buffer and ends on the second pass; the guard
                // keeps a (hypothetical) parser bug from spinning instead of failing closed.
                let have = buf.len();
                if needed <= have {
                    return Err(ProxyFault::Truncated(
                        std::io::ErrorKind::UnexpectedEof.into(),
                    ));
                }
                buf.resize(needed, 0);
                if let Some(tail) = buf.get_mut(have..) {
                    stream
                        .read_exact(tail)
                        .await
                        .map_err(ProxyFault::Truncated)?;
                }
            }
        }
    }
}

/// An untrusted peer must NOT speak PROXY v2 — but its bytes belong to the TLS/HTTP stack, so
/// only peek (non-consuming): a signature mismatch at any position clears the connection to
/// proceed with its real peer; a full signature is a fault (ADR 000057 receipt rules).
async fn detect_untrusted_header(
    stream: &TcpStream,
    peer: SocketAddr,
) -> Result<SocketAddr, ProxyFault> {
    let mut probe = [0u8; SIGNATURE.len()];
    loop {
        let n = stream
            .peek(&mut probe)
            .await
            .map_err(ProxyFault::Truncated)?;
        if n == 0 {
            return Err(ProxyFault::Truncated(
                std::io::ErrorKind::UnexpectedEof.into(),
            ));
        }
        if probe.get(..n) != SIGNATURE.get(..n) {
            return Ok(peer);
        }
        if n >= SIGNATURE.len() {
            return Err(ProxyFault::UntrustedHeader);
        }
        // A strict signature prefix: only more bytes can decide. `peek` keeps returning the
        // same data immediately while nothing new arrives, so pace the loop rather than spin;
        // the caller's deadline bounds the wait.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    /// Build a v2 header: signature + `ver_cmd` + `fam_proto` + declared length + `payload`.
    fn header(ver_cmd: u8, fam_proto: u8, declared: u16, payload: &[u8]) -> Vec<u8> {
        let mut h = Vec::with_capacity(PREFIX_LEN + payload.len());
        h.extend_from_slice(&SIGNATURE);
        h.push(ver_cmd);
        h.push(fam_proto);
        h.extend_from_slice(&declared.to_be_bytes());
        h.extend_from_slice(payload);
        h
    }

    fn v4_block(src: SocketAddrV4, dst: SocketAddrV4) -> Vec<u8> {
        let mut b = Vec::with_capacity(12);
        b.extend_from_slice(&src.ip().octets());
        b.extend_from_slice(&dst.ip().octets());
        b.extend_from_slice(&src.port().to_be_bytes());
        b.extend_from_slice(&dst.port().to_be_bytes());
        b
    }

    fn v6_block(src: SocketAddrV6, dst: SocketAddrV6) -> Vec<u8> {
        let mut b = Vec::with_capacity(36);
        b.extend_from_slice(&src.ip().octets());
        b.extend_from_slice(&dst.ip().octets());
        b.extend_from_slice(&src.port().to_be_bytes());
        b.extend_from_slice(&dst.port().to_be_bytes());
        b
    }

    #[test]
    fn parse_table() {
        let src4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 51234);
        let dst4 = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 443);
        let src6 = SocketAddrV6::new("2001:db8::9".parse::<Ipv6Addr>().unwrap(), 51234, 0, 0);
        let dst6 = SocketAddrV6::new("2001:db8::1".parse::<Ipv6Addr>().unwrap(), 443, 0, 0);

        struct Case {
            name: &'static str,
            input: Vec<u8>,
            want: Result<Parsed, ProxyV2Error>,
        }
        let cases = vec![
            Case {
                name: "proxy over tcp/ipv4 restores the source",
                input: header(0x21, 0x11, 12, &v4_block(src4, dst4)),
                want: Ok(Parsed::Complete {
                    decision: ProxyV2::Proxy { src: src4.into() },
                    consumed: 28,
                }),
            },
            Case {
                name: "proxy over tcp/ipv6 restores the source",
                input: header(0x21, 0x21, 36, &v6_block(src6, dst6)),
                want: Ok(Parsed::Complete {
                    decision: ProxyV2::Proxy { src: src6.into() },
                    consumed: 52,
                }),
            },
            Case {
                name: "tlv bytes after the address block are skipped",
                input: header(0x21, 0x11, 12 + 5, &{
                    let mut p = v4_block(src4, dst4);
                    p.extend_from_slice(&[0x04, 0x00, 0x02, 0xAA, 0xBB]); // PP2_TYPE_NOOP
                    p
                }),
                want: Ok(Parsed::Complete {
                    decision: ProxyV2::Proxy { src: src4.into() },
                    consumed: 33,
                }),
            },
            Case {
                name: "local with zero length uses the real endpoints",
                input: header(0x20, 0x00, 0, &[]),
                want: Ok(Parsed::Complete {
                    decision: ProxyV2::Local,
                    consumed: 16,
                }),
            },
            Case {
                name: "local must still skip its declared bytes (spec: never assume zero)",
                input: header(0x20, 0x11, 12, &v4_block(src4, dst4)),
                want: Ok(Parsed::Complete {
                    decision: ProxyV2::Local,
                    consumed: 28,
                }),
            },
            Case {
                name: "empty buffer needs the fixed prefix",
                input: Vec::new(),
                want: Ok(Parsed::Incomplete { needed: PREFIX_LEN }),
            },
            Case {
                name: "a strict signature prefix is incomplete, not a mismatch",
                input: b"\r\n\r".to_vec(),
                want: Ok(Parsed::Incomplete { needed: PREFIX_LEN }),
            },
            Case {
                name: "prefix complete but address block still in flight",
                input: header(0x21, 0x11, 12, &v4_block(src4, dst4)[..4]),
                want: Ok(Parsed::Incomplete { needed: 28 }),
            },
            Case {
                name: "signature mismatch",
                input: b"GET /api HTTP/1.1\r\n".to_vec(),
                want: Err(ProxyV2Error::BadSignature),
            },
            Case {
                name: "v1 text form is not accepted (ADR 000057)",
                input: b"PROXY TCP4 203.0.113.9 10.0.0.1 51234 443\r\n".to_vec(),
                want: Err(ProxyV2Error::BadSignature),
            },
            Case {
                name: "version nibble other than 2 is rejected",
                input: header(0x11, 0x11, 12, &v4_block(src4, dst4)),
                want: Err(ProxyV2Error::BadVersion(0x11)),
            },
            Case {
                name: "command nibble beyond LOCAL/PROXY is rejected",
                input: header(0x22, 0x11, 12, &v4_block(src4, dst4)),
                want: Err(ProxyV2Error::BadCommand(0x22)),
            },
            Case {
                name: "proxy with AF_UNSPEC is cut, not degraded to the LB address",
                input: header(0x21, 0x00, 0, &[]),
                want: Err(ProxyV2Error::UnsupportedFamilyProtocol(0x00)),
            },
            Case {
                name: "proxy over AF_UNIX is rejected",
                input: header(0x21, 0x31, 216, &[0u8; 216]),
                want: Err(ProxyV2Error::UnsupportedFamilyProtocol(0x31)),
            },
            Case {
                name: "proxy over DGRAM is rejected",
                input: header(0x21, 0x12, 12, &v4_block(src4, dst4)),
                want: Err(ProxyV2Error::UnsupportedFamilyProtocol(0x12)),
            },
            Case {
                name: "declared length above the 2 KiB cap is rejected before any read",
                input: header(0x21, 0x11, 2049, &[]),
                want: Err(ProxyV2Error::DeclaredLenTooLarge { declared: 2049 }),
            },
            Case {
                name: "declared length shorter than the ipv4 address block",
                input: header(0x21, 0x11, 11, &[0u8; 11]),
                want: Err(ProxyV2Error::AddressBlockTooShort {
                    declared: 11,
                    need: 12,
                }),
            },
            Case {
                name: "declared length shorter than the ipv6 address block",
                input: header(0x21, 0x21, 12, &v4_block(src4, dst4)),
                want: Err(ProxyV2Error::AddressBlockTooShort {
                    declared: 12,
                    need: 36,
                }),
            },
        ];
        for case in cases {
            let got = parse_proxy_v2(&case.input);
            assert_eq!(got, case.want, "case: {}", case.name);
        }
    }
}
