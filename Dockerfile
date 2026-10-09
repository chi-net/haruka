# syntax=docker/dockerfile:1.7

FROM node:24-bookworm-slim AS web-assets
WORKDIR /app

COPY package.json package-lock.json ./
RUN --mount=type=cache,target=/root/.npm \
    npm ci

COPY assets ./assets
COPY scripts ./scripts
COPY static/service-worker.js ./static/service-worker.js
COPY templates ./templates
COPY src ./src
RUN npm run web:build

FROM lukemathwalker/cargo-chef:0.1.78-rust-1-bookworm@sha256:2ee6e8edf0b91b5a7295071299596601710bccb243bd9735e0b9abf6e114582b AS chef
WORKDIR /app

RUN apt-get update \
    && apt-get install --yes --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS dependencies
COPY --from=planner /app/recipe.json ./recipe.json
# Keep compiled dependencies in the exported layer, not an ephemeral cache mount.
RUN cargo chef cook --locked --release --recipe-path recipe.json

FROM dependencies AS builder

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY --from=web-assets /app/static ./static

RUN cargo build --locked --release \
    && cp target/release/haruka /tmp/haruka \
    && strip /tmp/haruka

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 haruka \
    && useradd --system --uid 10001 --gid haruka --home-dir /nonexistent --shell /usr/sbin/nologin haruka \
    && install -d --owner haruka --group haruka /data

COPY --from=builder --chown=root:root /tmp/haruka /usr/local/bin/haruka

ENV PORT=3000 \
    DATABASE_URL="sqlite:///data/haruka.db?mode=rwc"

USER 10001:10001
VOLUME ["/data"]
EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/haruka"]
