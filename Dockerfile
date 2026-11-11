FROM node:24-bookworm-slim AS ui
WORKDIR /src/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

FROM rust:1.96-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY migrations ./migrations
COPY src ./src
COPY --from=ui /src/ui/dist ./ui/dist
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data sourcelens \
    && mkdir -p /data \
    && chown sourcelens /data
COPY --from=build /src/target/release/sourcelens /usr/local/bin/sourcelens
USER sourcelens
ENV SOURCELENS_DATA_DIR=/data
VOLUME /data
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/sourcelens"]
