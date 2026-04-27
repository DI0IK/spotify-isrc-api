# --- Stage 1: Build ---
FROM rust:1.77-slim-bullseye AS builder

# SQLite bindings require a C compiler during the build phase
RUN apt-get update && apt-get install -y build-essential

WORKDIR /usr/src/app

# Copy Cargo.toml to cache dependencies
COPY Cargo.toml ./
# Create a dummy src to download and compile external crates
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

# Copy your actual source code
COPY src ./src
# Touch main.rs to force Cargo to rebuild our actual app
RUN touch src/main.rs
RUN cargo build --release

# --- Stage 2: Runtime ---
FROM debian:bullseye-slim

WORKDIR /app

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/app/target/release/isrc-api /app/isrc-api

# Create a folder where Unraid will mount your database
RUN mkdir /app/data

# Run the binary
CMD ["./isrc-api"]