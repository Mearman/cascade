# Multi-stage build for the cascade binary used in the P2P e2e tests.
# Uses the pinned rust-toolchain.toml channel via rustup's automatic
# toolchain selection so the image matches CI's compiler exactly.

FROM rust:1.96-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --bin cascade && \
    cp /src/target/release/cascade /usr/local/bin/cascade

# Runtime image is intentionally minimal — the P2P backend speaks plain
# TLS and the WebDAV presenter speaks plain HTTP, so we need neither
# ca-certificates nor curl in the container itself.
FROM debian:bookworm-slim
COPY --from=builder /usr/local/bin/cascade /usr/local/bin/cascade
RUN cascade --version
WORKDIR /data
# WebDAV presenter (HTTP) and BEP listener (TLS over TCP).
EXPOSE 8080 22000
ENV CASCADE_PRESENTER=webdav
ENTRYPOINT ["cascade"]
