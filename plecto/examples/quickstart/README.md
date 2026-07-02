# quickstart — the 5-minute hello

A proxy whose per-request logic is a **sandboxed WASM filter**. One `curl` proves a
Component Model filter — signed, verified, loaded through the production path — touched
your traffic.

## Run

```bash
rustup target add wasm32-unknown-unknown   # once
cargo run -p plecto-server --example quickstart
```

## Try it

```bash
curl -i http://localhost:8080/
```

```text
HTTP/1.1 200 OK
x-plecto: hello-from-wasm      <-- added by the sandboxed filter
content-type: application/json

{"upstream":"hello"}
```

## How it works

`main.rs` spins up a tiny in-process upstream, signs the
[`filter-quickstart`](../filters/filter-quickstart) component, bundles it as an offline
OCI image-layout, and loads it via `Control::from_manifest_path` — the same
verify-then-load entrypoint the real `plecto` binary uses (bad signature → refused,
fail-closed). The filter's whole job is one `on-response` hook returning
`modified` with an extra header.

The manifest is four stanzas: `[trust]` (who may sign filters), one `[[filter]]`
(digest-pinned), one `[[upstream]]`, one `[[route]]`.

## Next

[`wasm-auth`](../wasm-auth) — a real filter doing API-key authentication. Then scaffold
your own filter from [`filters/filter-template`](../filters/filter-template)
(see [docs/writing-a-filter.md](../../../docs/writing-a-filter.md)).
