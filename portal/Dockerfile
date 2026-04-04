# Portal — Being's gateway to the world
# Build: cargo build --release --target x86_64-unknown-linux-musl --bin heart-portal
# Then: docker build -t portal:v1 -f portal/Dockerfile .
#
# Requires pre-built binary at target/x86_64-unknown-linux-musl/release/heart-portal
# (cross-compile on Mac, deploy on Linux)

FROM alpine:3.21

RUN apk add --no-cache curl python3

RUN mkdir -p /workspace/tools

COPY target/x86_64-unknown-linux-musl/release/heart-portal /app/heart-portal
RUN chmod +x /app/heart-portal

WORKDIR /workspace
ENTRYPOINT ["/app/heart-portal"]
# Default: look for portal.toml in /config/ (mount via docker compose)
CMD ["/config/portal.toml"]
