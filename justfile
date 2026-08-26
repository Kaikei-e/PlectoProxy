# Plecto task shortcuts — run from the repository root.
# The Rust workspace lives in plecto/; every recipe cd's there for you, so you never
# have to remember the working directory. `just` with no args lists all recipes.

plecto := "plecto"

# list available recipes
default:
    @just --list

# full local CI parity: fmt check + clippy (-D warnings) + tests + the drift gates — run before every PR
check: lint test drift-check

# run the test suite
test:
    cd {{plecto}} && cargo test --all

# format the workspace
fmt:
    cd {{plecto}} && cargo fmt --all

# check formatting without writing
fmt-check:
    cd {{plecto}} && cargo fmt --all -- --check

# clippy with warnings as errors
clippy:
    cd {{plecto}} && cargo clippy --all-targets --all-features -- -D warnings

# fmt check + clippy
lint: fmt-check clippy

# run a guided demo end to end: quickstart | wasm-auth | load-balancing | filter-chain |
# tls-http | hot-reload | canary | resilience | production
demo NAME:
    cd {{plecto}} && ./examples/try.sh {{NAME}}

# run every guided demo in turn
demo-all:
    cd {{plecto}} && ./examples/try.sh all

# run an example server directly (Ctrl-C to stop)
example NAME:
    cd {{plecto}} && cargo run -p plecto-server --example {{NAME}}

# build the example filter guests for wasm32-unknown-unknown
build-filters:
    cd {{plecto}}/examples/filters/filter-hello && cargo build --target wasm32-unknown-unknown --release
    cd {{plecto}}/examples/filters/filter-apikey && cargo build --target wasm32-unknown-unknown --release

# refresh the filter template's vendored WIT from the canonical contract (idempotent)
sync-template-wit:
    cp {{plecto}}/wit/world.wit {{plecto}}/examples/filters/filter-template/wit/world.wit
    @echo "synced filter-template/wit/world.wit from plecto/wit/world.wit"

# The CI drift gate requires the two copies byte-identical — run after touching the example.
# refresh the `plecto new-filter` template vendored into the CLI crate from examples/filters/filter-template
sync-template-crate:
    cp {{plecto}}/examples/filters/filter-template/Cargo.toml {{plecto}}/crates/plecto/templates/filter-template/Cargo.toml.template
    cp {{plecto}}/examples/filters/filter-template/src/lib.rs {{plecto}}/crates/plecto/templates/filter-template/src/lib.rs
    @echo "synced crates/plecto/templates/filter-template from examples/filters/filter-template"

# the two cheap CI gates fmt/clippy/test don't cover: vendored WIT/template drift + ADR graph
drift-check:
    python3 scripts/check_wit_vendoring.py
    python3 scripts/check_adr_graph.py

# An in-place update leaves peer deps nested-only, which newer npm rejects under `npm ci`.
# regenerate the JS guests' package-lock.json from scratch (pins in package.json are kept)
regen-js-lockfiles:
    for d in filter-hello-js filter-tokenlimit-js; do \
        (cd {{plecto}}/examples/filters/$d && rm -rf node_modules package-lock.json && npm install); \
    done

# --default-features is required for plecto-host: the all-features build fires its build.rs guest
# build, which fails outside the guest toolchain and is not an API break.
# public-API semver of the crates.io crates vs the newest published version
semver-check:
    cd {{plecto}} && cargo semver-checks check-release -p plecto-host --default-features
    cd {{plecto}} && cargo semver-checks check-release -p plecto-control --default-features
    cd {{plecto}} && cargo semver-checks check-release -p plecto-server --default-features

# --allow-dirty because release prep runs this before the release commit exists.
# package + verify-build every publishable crate without uploading
publish-dry-run:
    cd {{plecto}} && cargo publish --dry-run --workspace --locked --allow-dirty

# Run `check` first; add `bench-build` + `gate` (T1) when the runtime or a hot path moved.
# release preflight: drift gates + semver + publish dry-run
release-check: drift-check semver-check publish-dry-run

# build the release examples the perf runbook drives (run-perf.sh does not build)
bench-build:
    cd {{plecto}} && cargo build --release -p plecto-server --features bench-harnesses \
        --example load-balancing --example bench-server --example tls-http --example swap-bench

# T1 perf gate (~6-7 min): interleaved invariant deltas vs bench/perf/gate_tolerances.toml,
# machine verdict (exit 0 = in band). Run on hot-path changes; see bench/methodology.md § tiers
gate:
    bash bench/perf/run-perf.sh gate

# T2 release-snapshot perf report (~22 min): the full suite at report-tier windows
report:
    bash bench/perf/run-perf.sh all

# T3 deep phase by name (opt-in diagnostics): v03, tls, h3, or any single runbook phase
deep PHASE:
    bash bench/perf/run-perf.sh {{PHASE}}
