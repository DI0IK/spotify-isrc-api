# --- Stage 1: Build ---
# UPDATED: Use the latest stable 1.x Rust compiler on Debian Bookworm
FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y build-essential

WORKDIR /usr/src/app

COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

COPY src ./src
RUN touch src/main.rs
RUN cargo build --release

# --- Stage 2: Runtime ---
# UPDATED: Use Bookworm to match the builder's libc version
FROM debian:bookworm-slim

WORKDIR /app

COPY --from=builder /usr/src/app/target/release/isrc-api /app/isrc-api

RUN mkdir /app/data

CMD ["./isrc-api"]