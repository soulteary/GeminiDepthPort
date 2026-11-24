FROM rust:1.100.0-alpine AS management-build
RUN apk add --no-cache build-base
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY driver_profiles.json ./driver_profiles.json
# Preserve the shared workspace; only the management package is built here.
COPY crates ./crates
RUN cargo build --locked --release -p geminidepthport

# Docker CLI and Compose only; the management image has no Python runtime.
FROM docker:29.10.0-cli
RUN apk add --no-cache bash
COPY --from=management-build /build/target/release/geminidepthport /usr/local/bin/geminidepthport
# Application assets and mutable state use the identical absolute host bind path.
