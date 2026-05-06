# =============================================================================
# hc-ecowitt — HomeCore Ecowitt Weather Station Plugin
# Alpine Linux — minimal, static-friendly runtime
# =============================================================================
#
# Build:
#   docker build -t hc-ecowitt:latest .
#
# Run:
#   docker run -d \
#     -p 8888:8888 \
#     -v ./config/config.toml:/opt/hc-ecowitt/config/config.toml:ro \
#     -v hc-ecowitt-logs:/opt/hc-ecowitt/logs \
#     hc-ecowitt:latest
#
# Note: port 8888 is the default HTTP receiver port for "Customized"
#       upload from the Ecowitt gateway. Override with [ecowitt]
#       listen_port in config.toml and update the -p mapping to match.
#       Bind to 0.0.0.0 inside the container (set bind_addr) so the
#       gateway on the LAN can POST in.
#
# Volumes:
#   /opt/hc-ecowitt/config   config.toml (listen_port, allowed_source_ips)
#   /opt/hc-ecowitt/logs     rolling log files
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1 — Build
# -----------------------------------------------------------------------------
FROM rust:1.95-alpine3.23@sha256:606fd313a0f49743ee2a7bd49a0914bab7deedb12791f3a846a34a4711db7ed2 AS builder

RUN apk upgrade --no-cache && apk add --no-cache musl-dev openssl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/

RUN cargo build --release --bin hc-ecowitt

# -----------------------------------------------------------------------------
# Stage 2 — Runtime
# -----------------------------------------------------------------------------
FROM alpine:3.23@sha256:5b10f432ef3da1b8d4c7eb6c487f2f5a8f096bc91145e68878dd4a5019afde11

# `apk upgrade` first pulls CVE patches for packages baked into the
# alpine:3 base since the upstream image was last rebuilt. Defense
# in depth — without this, `apk add --no-cache` only refreshes the
# named packages, leaving busybox/musl/etc. on the base's frozen
# versions.
RUN apk upgrade --no-cache && \
    apk add --no-cache \
        ca-certificates \
        libssl3 \
        tzdata

RUN adduser -D -h /opt/hc-ecowitt hcecowitt

COPY --from=builder /build/target/release/hc-ecowitt /usr/local/bin/hc-ecowitt
RUN chmod 755 /usr/local/bin/hc-ecowitt

RUN mkdir -p /opt/hc-ecowitt/config /opt/hc-ecowitt/logs

COPY config/config.toml.example /opt/hc-ecowitt/config/config.toml.example

RUN chown -R hcecowitt:hcecowitt /opt/hc-ecowitt

USER hcecowitt
WORKDIR /opt/hc-ecowitt

VOLUME ["/opt/hc-ecowitt/config", "/opt/hc-ecowitt/logs"]

EXPOSE 8888

ENV RUST_LOG=info

ENTRYPOINT ["hc-ecowitt"]
