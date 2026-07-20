# Reference build for the plecto binary (moka-1 field report §3.1: ship a canonical Dockerfile so
# per-user Dockerfiles don't proliferate). Multi-stage: a pinned Rust toolchain builds the release
# binary (or a prebuilt one is copied in, see SOURCE below); the runtime stage is distroless (no
# shell, no package manager — minimal CVE surface).
#
# Build from source (repo root, default — no prerequisites beyond Docker). The default is the
# `minimal` runtime capability profile (ADR 000079: default features, smallest attack surface);
# pass FEATURES to compile a named profile in — e.g. the `capabilities` profile (outbound-http +
# outbound-tcp + fat-guest). Compile-time inclusion is not a runtime grant: capabilities are
# still lent per filter by the manifest's deny-by-default allowlist + SSRF floor.
#   docker build -t plecto .
#   docker build -t plecto:capabilities --build-arg FEATURES=capabilities .
#
# Build from an already-built binary instead of compiling (used by the release workflow's
# native-per-arch matrix, so a multi-arch image is never cross-compiled under QEMU emulation —
# see .github/workflows/release.yml): pass SOURCE=prebuilt and a `prebuilt` build context whose
# root contains a single file named `plecto`:
#   docker build -t plecto --build-arg SOURCE=prebuilt --build-context prebuilt=./dist .
#
# Run:                 docker run -v ./deploy:/etc/plecto:ro -p 8443:8443 plecto
#
# The container binds per the manifest's `[listen] addr` (e.g. `0.0.0.0:8443`) — no entrypoint
# arg gymnastics. Note the runtime user is nonroot, so bind a port >= 1024 and publish it to 443
# on the host; set `[listen] advertised_port = 443` so the Alt-Svc h3 advertisement matches the
# published port.
#
# Base images are referenced by tag for readability; pin them by digest in a production fork
# (`docker buildx imagetools inspect <image>` prints the current digest).

ARG RUST_VERSION=1.97.1
# "source" (default) compiles plecto/ with the pinned Rust toolchain below; "prebuilt" copies a
# binary from the `prebuilt` build context instead. BuildKit only executes the stage that
# `build-${SOURCE}` actually resolves to, so picking "prebuilt" skips the Rust toolchain entirely.
ARG SOURCE=source

FROM rust:${RUST_VERSION}-bookworm AS build-source
# cmake + clang: aws-lc-sys (pulled by sigstore's cosign verification) builds its C crypto with
# cmake; everything else in the tree is pure Rust (rustls rides the `ring` provider).
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY plecto/ plecto/
# --locked: the committed Cargo.lock is the supply-chain pin; a drifted lockfile fails the build.
# FEATURES: empty (default) = the minimal profile; "capabilities" compiles the outbound + fat-guest
# capability code in (ADR 000079). Deliberately expanded unquoted — it is a cargo flag list.
ARG FEATURES=""
RUN cargo build --manifest-path plecto/Cargo.toml --release --locked -p plecto \
    ${FEATURES:+--features ${FEATURES}} \
    && cp plecto/target/release/plecto /plecto

FROM scratch AS build-prebuilt
# --chmod=755: actions/upload-artifact's zip transport does not reliably preserve the executable
# bit (confirmed 2026-07-04 — the release workflow's uploaded binary came back non-executable),
# so force it explicitly rather than trusting whatever mode the artifact round-trip hands back.
COPY --chmod=755 --from=prebuilt plecto /plecto

FROM build-${SOURCE} AS build

# distroless/cc: glibc + libgcc + CA certificates and nothing else — the smallest base that runs
# a dynamically-linked glibc binary. (A static musl build would allow distroless/static, but
# aws-lc-sys makes musl cross-builds toolchain-heavy; revisit if/when that dependency drops out.)
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --chmod=755 --from=build /plecto /usr/local/bin/plecto
ENTRYPOINT ["/usr/local/bin/plecto"]
# The manifest is the single static source of config (`[listen]` included). Mount the deploy
# directory — not the single file — so SIGHUP reload survives editor inode swaps (field report §3.5).
CMD ["/etc/plecto/plecto.toml"]
