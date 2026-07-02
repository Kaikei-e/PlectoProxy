# tls-http — TLS termination across HTTP/1.1, HTTP/2, HTTP/3

With `[[tls]]` in the manifest, **one port carries all three HTTP versions**: HTTP/1.1
and HTTP/2 over TCP (ALPN-negotiated), and HTTP/3 over QUIC on the *same port number*
(UDP). TCP responses advertise h3 via `Alt-Svc` (ADR 000014 / 000015 / 000016).

## Run

```bash
cargo run -p plecto-server --example tls-http
# or, visualized (skips --http3 if your curl lacks it):
./examples/try.sh tls-http
```

## Try it

The cert is self-signed for `localhost`, hence `-k`:

```bash
curl -sk --http1.1 -o /dev/null -w 'negotiated HTTP/%{http_version}\n' https://localhost:8443/api/hello
# negotiated HTTP/1.1

curl -sk --http2  -o /dev/null -w 'negotiated HTTP/%{http_version}\n' https://localhost:8443/api/hello
# negotiated HTTP/2

curl -sk -D - -o /dev/null https://localhost:8443/api/hello | grep -i alt-svc
# alt-svc: h3=":8443"; ma=...        <- the same port, over UDP

curl -sk --http3 -o /dev/null -w 'negotiated HTTP/%{http_version}\n' https://localhost:8443/api/hello
# negotiated HTTP/3   (needs an h3-enabled curl)
```

## How it works

The manifest declares one `[[tls]]` cert (rustls terminates; SNI selects certs when you
declare several) and routes `/api/*` through a signed pass-through filter to the
upstream. The QUIC listener shares the cert resolver with the TCP one. Everything —
cert generation, filter signing, OCI layout — happens in a temp dir on startup.

## Next

[`hot-reload`](../hot-reload) — change the manifest under a live proxy with SIGHUP.
