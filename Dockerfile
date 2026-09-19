FROM rust:1.85.1-bookworm@sha256:bf7d87666c4da6eace19e06d21bc4859c6e2a5c97a21ac273b0e082112753cf0 AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY src ./src
COPY migrations ./migrations
COPY admin-ui/dist ./admin-ui/dist
RUN SOURCE_DATE_EPOCH=0 cargo build --release --locked

FROM debian:bookworm-slim@sha256:5ae3c39ebd15e229dcedd5cee596b2497182493d41ff162e824ba13fc1b2b867
RUN useradd --system --uid 10001 --create-home kiro
COPY --from=builder /src/target/release/kiro-gateway /usr/local/bin/kiro-gateway
USER kiro
WORKDIR /home/kiro
EXPOSE 8990
ENV RUST_LOG=info
ENV KIRO_HOST=0.0.0.0
ENTRYPOINT ["/usr/local/bin/kiro-gateway"]
