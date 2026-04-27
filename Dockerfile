# --- Stage 1: Build ---
# UPDATED: Use the latest stable 1.x Rust compiler on Debian Bookworm
FROM rust:1-slim-bookworm AS builder

RUN apt-get update \
	&& apt-get install -y --no-install-recommends \
		build-essential \
		pkg-config \
		libssl-dev \
	&& rm -rf /var/lib/apt/lists/*

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

RUN apt-get update \
	&& apt-get install -y --no-install-recommends \
		ca-certificates \
		libssl3 \
	&& rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /usr/src/app/target/release/spotify-isrc-api /app/isrc-api

RUN mkdir /app/data

CMD ["./isrc-api"]