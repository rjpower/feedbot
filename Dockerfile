# feedbot ships as one container running two processes: the Rust server, which
# supervises a Node sidecar that drives Chromium. The runtime image is
# Microsoft's Playwright image, whose browser build and npm package version must
# stay in lockstep — bump `fetcher/package.json` and the tag below together.

# ---- the Vue bundle -------------------------------------------------------
FROM node:24-bookworm-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# ---- the Rust binary ------------------------------------------------------
FROM rust:1-bookworm AS server
WORKDIR /src
# Build the dependency graph against a stub main so that editing our own source
# doesn't re-compile every crate we depend on.
COPY server/Cargo.toml server/Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY server/src ./src
RUN touch src/main.rs && cargo build --release --locked

# ---- runtime --------------------------------------------------------------
FROM mcr.microsoft.com/playwright:v1.61.1-noble
WORKDIR /app
ENV NODE_ENV=production \
    FEEDBOT_DB=/data/feedbot.db \
    FEEDBOT_STATIC=/app/static \
    FEEDBOT_FETCHER_SCRIPT=/app/fetcher/server.mjs \
    FEEDBOT_PORT=8000

COPY fetcher/package.json fetcher/package-lock.json ./fetcher/
RUN cd fetcher && npm ci --omit=dev && npm cache clean --force
COPY fetcher/server.mjs ./fetcher/

COPY --from=web /web/dist ./static
COPY --from=server /src/target/release/feedbot /usr/local/bin/feedbot

# `pwuser` (uid 1000) ships with the Playwright image and owns the browsers.
RUN mkdir -p /data && chown -R pwuser:pwuser /data /app
USER pwuser

EXPOSE 8000
VOLUME ["/data"]

# Node has a global fetch, so the image needs no curl just to prove it is alive.
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD node -e "fetch('http://127.0.0.1:8000/healthz').then(r => process.exit(r.ok ? 0 : 1)).catch(() => process.exit(1))"

CMD ["feedbot"]
