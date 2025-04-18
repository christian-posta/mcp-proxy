FROM docker.io/library/rust:1.85.1-slim-bookworm AS builder 

ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY src ./src
COPY common ./common

RUN cargo build --release

RUN strip target/release/mcp-proxy

FROM gcr.io/distroless/cc-debian12 AS runner 

ARG TARGETARCH
WORKDIR /app

COPY --from=builder /app/target/release/mcp-proxy /app/mcp-proxy

LABEL org.opencontainers.image.source=https://github.com/mcp-proxy/mcp-proxy
LABEL org.opencontainers.image.description="MCP gw is a proxy for MCP."

ENTRYPOINT ["/app/mcp-proxy"]
