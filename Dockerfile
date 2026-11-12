FROM node:26.13.0-bookworm-slim AS web
WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM rust:1.99.1-bookworm AS server
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY contracts/ contracts/
COPY server/ server/
COPY migrations/ migrations/
RUN cargo build --locked --release -p receipt-roots-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends nginx curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home receipt \
    && mkdir -p /data /tmp/nginx && chown receipt:receipt /data /tmp/nginx
COPY --from=server /build/target/release/receipt-roots-server /usr/local/bin/
COPY --from=web /build/web/dist/ /usr/share/nginx/html/
COPY docker/nginx.conf /etc/nginx/nginx.conf
COPY docker/start.sh /usr/local/bin/start-receipt-roots
ENV BIND_ADDR=127.0.0.1:3000 PAYPAL_MODE=mock LLM_MODE=manual TRUST_LOOPBACK_PROXY=true DATABASE_URL=sqlite:///data/receipt-roots.db?mode=rwc
USER receipt
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s CMD curl --fail --silent http://127.0.0.1:8080/health >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/start-receipt-roots"]
