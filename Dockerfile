FROM node:24-bookworm-slim AS web-build
WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM rust:1.93-bookworm AS server-build
WORKDIR /build
COPY server/ ./server/
RUN cargo build --locked --release --manifest-path server/Cargo.toml

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=server-build /build/server/target/release/share-video /usr/local/bin/share-video
COPY --from=web-build /build/web/dist ./web/dist
ENV BIND_ADDR=0.0.0.0:8080 STATIC_DIR=/app/web/dist
EXPOSE 8080
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/share-video"]
