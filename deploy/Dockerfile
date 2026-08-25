# Build:  docker build -f deploy/Dockerfile -t holos:latest .
# Run:    docker run --rm -p 7878:7878 -v holos-data:/var/lib/holos holos:latest
#
# Two stages: the build needs a C++ toolchain and libclang for RocksDB; the runtime needs
# neither, so it does not carry them.

# ---- build ----------------------------------------------------------------
FROM rust:1.87-bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
        clang libclang-dev cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Copy manifests first so a source-only change does not re-download the index.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY testsuites ./testsuites

RUN cargo build --release --workspace --bin holos --bin holos-server

# ---- runtime --------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /var/lib/holos --shell /usr/sbin/nologin holos \
    && mkdir -p /var/lib/holos/store \
    && chown -R holos:holos /var/lib/holos

COPY --from=build /src/target/release/holos        /usr/local/bin/holos
COPY --from=build /src/target/release/holos-server /usr/local/bin/holos-server
COPY deploy/smoke.sh /usr/local/bin/holos-smoke
COPY examples /opt/holos/examples

RUN chmod +x /usr/local/bin/holos-smoke

USER holos
WORKDIR /var/lib/holos
EXPOSE 7878
VOLUME ["/var/lib/holos"]

# 0.0.0.0 inside a container is the container's own namespace, not the host's network.
# Publishing the port is what exposes it, which stays the operator's decision.
#
# Note there is no --trust-forwarded-identity here. Turn it on only when something in
# front of this container sets the identity headers and strips the client's. Overriding
# the command is how you do that; see docker-compose.yml.
ENTRYPOINT ["holos-server"]
CMD ["--listen", "0.0.0.0:7878", "--store", "/var/lib/holos/store"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:7878/health || exit 1
