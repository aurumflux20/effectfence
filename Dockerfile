# EffectFence MCP server — container image.
#
# Two-stage: compile with the Rust toolchain, ship a slim runtime that just
# runs the stdio MCP server. Useful for anyone running the fence as a
# containerized MCP server (and required by directory listings that boot
# the image and send an introspection request).
#
#   docker build -t effectfence .
#   docker run -i --rm effectfence          # stdio MCP server
#   docker run -i --rm effectfence wrap -- <other-mcp-server-cmd>

FROM rust:1-slim AS build
WORKDIR /build

# Manifest first so dependency compilation caches across source edits.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
COPY examples ./examples
COPY README.md ./

RUN cargo build --release --bin effectfence

FROM debian:stable-slim
# The server speaks JSON-RPC over stdio only: no network listener, no state
# on disk, nothing to mount. Run as a non-root user.
RUN useradd --create-home --uid 10001 fence
COPY --from=build /build/target/release/effectfence /usr/local/bin/effectfence
USER fence
ENTRYPOINT ["/usr/local/bin/effectfence"]
