# Smartflow Halo -- official image. Ships both binaries (`halo` shim and
# `halo-relay`) so one image serves either role. OpenClaw and similar runtimes
# document Docker/K8s deployment; Halo slots in as a sidecar/companion here.
#
# Multi-stage: a Rust builder, then a slim Debian runtime. rustls (no OpenSSL)
# and rusqlite's bundled amalgamation mean the runtime needs only libc +
# ca-certificates -- no system libssl or libsqlite3.

FROM rust:1-bookworm AS builder
WORKDIR /src
# Copy the manifests first for a cached dependency layer, then the sources.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config
RUN cargo build --release --bin halo --bin halo-relay

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/halo /usr/local/bin/halo
COPY --from=builder /src/target/release/halo-relay /usr/local/bin/halo-relay
COPY --from=builder /src/config/halo.example.yaml /etc/halo/halo.example.yaml

# Persist all Halo state (config, ledger, caches, audit log) on a volume so it
# survives container restarts. Real provider secrets still belong in the OS
# keychain or the encrypted-file fallback ($HALO_VAULT_PASSPHRASE) -- never
# baked into the image.
ENV HALO_HOME=/data
VOLUME ["/data"]

# Shim ingress (8787) and relay dashboard/ingest (8080). Publish whichever the
# chosen entrypoint uses.
EXPOSE 8787 8080

# Default to the shim; override with `halo-relay` for the relay role:
#   docker run ... ghcr.io/aperionai/halo halo-relay --bind 0.0.0.0:8080
ENTRYPOINT ["halo"]
CMD ["serve"]
