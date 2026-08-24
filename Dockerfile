# syntax=docker/dockerfile:1.7

FROM node:24-bookworm-slim AS web-assets
WORKDIR /app

COPY package.json package-lock.json ./
RUN --mount=type=cache,target=/root/.npm \
    npm ci

COPY assets ./assets
COPY templates ./templates
COPY src ./src
RUN npm run css:build

FROM rust:1-bookworm AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install --yes --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY --from=web-assets /app/static/app.css ./static/app.css

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --locked --release \
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
