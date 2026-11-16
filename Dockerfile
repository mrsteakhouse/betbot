# syntax=docker/dockerfile:1

# Keep the builder tag in sync with rust-toolchain.toml (channel). The toolchain
# file itself is deliberately not copied: rustup would try to download its
# extra components (rust-src, rust-analyzer) inside the build.
# Full (non-slim) image: ring needs a C compiler.
FROM rust:1.100.0-trixie AS builder
WORKDIR /build

# 1) Dependency layer: build with a dummy main so it is cached until
#    Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && cargo clean --release --locked -p betbot \
 && rm -rf src

# 2) Real sources.
COPY src ./src
COPY tests ./tests
RUN touch src/main.rs && cargo build --release --locked
# Fail the build if the dummy binary from the dependency layer survived:
# it prints nothing for --version.
RUN ./target/release/betbot --version \
  | grep -qx "betbot $(sed -n 's/^version = \"\(.*\)\"/\1/p' Cargo.toml | head -n1)"

# glibc (trixie) matches the builder; cc-* provides libgcc_s and CA certificates.
FROM gcr.io/distroless/cc-debian13:nonroot
ARG VERSION=dev
ARG REVISION=unknown
ARG SOURCE=https://github.com/mrsteakhouse/betbot
LABEL org.opencontainers.image.title="betbot" \
      org.opencontainers.image.description="Tipping bot (Tippspiel) running as a Kubernetes CronJob" \
      org.opencontainers.image.source="${SOURCE}" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=builder /build/target/release/betbot /usr/local/bin/betbot
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/betbot"]
