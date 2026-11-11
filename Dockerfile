# syntax=docker/dockerfile:1

# Must equal the channel in rust-toolchain.toml; the build stage fails when they differ.
ARG RUST_VERSION=1.96.1

FROM node:24-trixie-slim AS ui
WORKDIR /src/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

FROM rust:${RUST_VERSION}-trixie AS build
WORKDIR /src

# rust-toolchain.toml is copied aside, not into /src: inside /src rustup would install the
# clippy/rustfmt components it lists, which the release build does not need.
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN rustc_version="$(rustc --version | cut -d ' ' -f 2)" \
    && grep -qx "channel = \"${rustc_version}\"" /tmp/rust-toolchain.toml \
    || { echo "rustc ${rustc_version} does not match rust-toolchain.toml; update RUST_VERSION" >&2; exit 1; }

# Dependency layer: compile every dependency against stub sources so that a source-only change
# reuses this layer and recompiles only the sourcelens crate.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src ui/dist \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY migrations ./migrations
COPY src ./src
COPY --from=ui /src/ui/dist ./ui/dist
# Drop the stub build's fingerprints (lib, bin and build script) so cargo rebuilds the crate from the
# real sources and re-embeds ui/dist, independent of file modification times.
RUN rm -rf target/release/.fingerprint/sourcelens-* \
    && cargo build --release --locked \
    && cp target/release/sourcelens /usr/local/bin/sourcelens

FROM debian:trixie-slim
LABEL org.opencontainers.image.source="https://github.com/AlxBiermann/sourcelens-lite" \
      org.opencontainers.image.title="SourceLens Lite" \
      org.opencontainers.image.description="Small Code-RAG application to provide vector search over multiple github repos via MCP."
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 sourcelens \
    && useradd --system --uid 10001 --gid sourcelens --home-dir /data --no-create-home \
        --shell /usr/sbin/nologin sourcelens \
    && mkdir -p /data \
    && chown sourcelens:sourcelens /data \
    && chmod 700 /data
COPY --from=build /usr/local/bin/sourcelens /usr/local/bin/sourcelens
ENV SOURCELENS_BIND=0.0.0.0:3000 \
    SOURCELENS_DATA_DIR=/data
USER 10001:10001
VOLUME /data
EXPOSE 3000
# Assumes the default port; change it together with SOURCELENS_BIND.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --start-interval=2s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3000/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/sourcelens"]
