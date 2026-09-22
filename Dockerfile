# syntax=docker/dockerfile:1

FROM rust:1.97-bookworm AS builder

WORKDIR /build

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        cmake \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Everything the root package's build reads, and nothing else:
#   - the workspace members, because cargo loads EVERY member's manifest
#     before it builds anything (a missing one fails with "failed to load
#     manifest for workspace member"); keep this in step with `members` in
#     Cargo.toml;
#   - build.rs and the build_info.rs it `include!`s (both shells' build
#     scripts share that one file, so it sits at the root, not in src);
#   - contracts/alto, `include_str!`d by src/worker/executor/deployment.rs.
COPY Cargo.toml Cargo.lock build.rs build_info.rs ./
COPY src ./src
COPY vela-relay-core ./vela-relay-core
COPY vela-relay-cf ./vela-relay-cf
COPY contracts ./contracts

# --bin vela-relay: the package also ships `deploy-simulations`, an operator
# tool the image has no use for.
RUN cargo build --release --locked --bin vela-relay


FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates \
        curl \
    && groupadd --gid 10001 vela \
    && useradd --uid 10001 --gid vela --create-home --shell /usr/sbin/nologin vela \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/vela-relay /usr/local/bin/vela-relay

USER vela

ENV VELA_RELAY_LISTEN_ADDR=0.0.0.0:4567

EXPOSE 4567

HEALTHCHECK --interval=15s --timeout=3s --start-period=15s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:4567/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/vela-relay"]
