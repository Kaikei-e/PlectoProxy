# hot-reload — zero-downtime config swap (SIGHUP)

Edit the manifest, `kill -HUP`, and the change takes effect **atomically** without
dropping a connection: new requests see the new config, in-flight ones finish on the old
(ADR 000008 / 000039). A *broken* edit is **fail-closed** — the reload is rejected and
the running config keeps serving.

## Run

```bash
cargo run -p plecto-server --example hot-reload
# or, fully scripted before/after:
./examples/try.sh hot-reload
```

The banner prints the proxy's **pid** and the on-disk **manifest path**.

## Try it

```bash
curl -s http://localhost:8082/api/hello
# upstream received: /hello              (strip_prefix = "/api" is active)

# edit the printed manifest: strip_prefix = "/api"  →  strip_prefix = "/"
kill -HUP <pid>

curl -s http://localhost:8082/api/hello
# upstream received: /api/hello          (the new config, live, no restart)
```

Break it on purpose — make the manifest invalid TOML and SIGHUP again: the reload is
refused, the proxy keeps serving the last good config, and never goes down.

## How it works

`Control::from_manifest_path` remembers the path; a background `serve_reloads` loop
re-reads it on every SIGHUP and swaps the compiled config in with an atomic pointer
swap. Reloads are reconciled by the manifest's **semantic content hash** — a whitespace
or comment edit is a no-op, not a config change. The real `plecto` binary wires exactly
this loop (plus SIGTERM graceful drain — see [`production`](../production)).

## Next

[`canary`](../canary) — use the same reload to drain or promote a live traffic split.
