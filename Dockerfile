# Reference build for the plecto binary (moka-1 field report §3.1: ship a canonical Dockerfile so
# per-user Dockerfiles don't proliferate). Multi-stage: a pinned Rust toolchain builds the release
# binary; the runtime stage is distroless (no shell, no package manager — minimal CVE surface).
#
# Build (repo root):   docker build -t plecto .
# Run:                 docker run -v ./deploy:/etc/plecto:ro -p 8443:8443 plecto
#
# The container binds per the manifest's `[listen] addr` (e.g. `0.0.0.0:8443`) — no entrypoint
# arg gymnastics. Note the runtime user is nonroot, so bind a port >= 1024 and publish it to 443
# on the host; set `[listen] advertised_port = 443` so the Alt-Svc h3 advertisement matches the
# published port.
#
# Base images are referenced by tag for readability; pin them by digest in a production fork
# (`docker buildx imagetools inspect <image>` prints the current digest).

ARG RUST_VERSION=1.96.0

FROM rust:${RUST_VERSION}-bookworm AS build
# cmake + clang: aws-lc-sys (pulled by sigstore's cosign verification) builds its C crypto with
# cmake; everything else in the tree is pure Rust (rustls rides the `ring` provider).
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY plecto/ plecto/
# --locked: the committed Cargo.lock is the supply-chain pin; a drifted lockfile fails the build.
RUN cargo build --manifest-path plecto/Cargo.toml --release --locked -p plecto-server

# distroless/cc: glibc + libgcc + CA certificates and nothing else — the smallest base that runs
# a dynamically-linked glibc binary. (A static musl build would allow distroless/static, but
# aws-lc-sys makes musl cross-builds toolchain-heavy; revisit if/when that dependency drops out.)
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/plecto/target/release/plecto /usr/local/bin/plecto
ENTRYPOINT ["/usr/local/bin/plecto"]
# The manifest is the single static source of config (`[listen]` included). Mount the deploy
# directory — not the single file — so SIGHUP reload survives editor inode swaps (field report §3.5).
CMD ["/etc/plecto/plecto.toml"]
