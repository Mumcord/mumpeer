# Multi-arch image of the mumpeer relay. The build stage runs on the build machine's own platform
# and cross-compiles a static musl binary with zig, so an arm64 image never needs a slow emulated
# Rust build. The `:-` defaults keep a plain `docker build` (without buildx) working.
FROM --platform=${BUILDPLATFORM:-linux/amd64} ghcr.io/rust-cross/cargo-zigbuild:0.24.0 AS build
ARG TARGETARCH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN case "${TARGETARCH:-amd64}" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && cargo zigbuild --release --locked --target "$target" \
    && mkdir -p /out/data \
    && cp "target/$target/release/mumpeer" /out/mumpeer

# Alpine instead of scratch so operators have a shell and busybox wget for checks.
FROM alpine:3.24
COPY --from=build /out/mumpeer /usr/local/bin/mumpeer
# The relay key and certificate (its identity that clients pin) live here; keep it on a volume,
# or every restart announces a new fingerprint.
COPY --from=build --chown=10001:10001 /out/data /var/lib/mumpeer
ENV MUMPEER_CONFIG=/config/mumpeer.toml \
    RUST_LOG=info
USER 10001:10001
VOLUME /var/lib/mumpeer
# Client relay (QUIC and TLS). The API (default 127.0.0.1:7480) stays inside the container's
# network; the bot reaches it by sharing that network (see README).
EXPOSE 443/udp 443/tcp
ENTRYPOINT ["mumpeer"]
