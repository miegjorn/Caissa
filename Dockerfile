FROM rust:latest AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
# caissa-core → amassada-core (path dep). Provide the upstream repo as a named
# build context so the relative path resolves: /Amassada → amassada-core
# (caissa-core's dep, via workspace: ../Amassada/crates/amassada-core).
WORKDIR /Amassada
COPY --from=amassada . .
WORKDIR /app
COPY . .
RUN cargo build --release -p caissa-cli

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 curl jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/caissa /usr/local/bin/caissa
COPY scripts/load-identity.sh /usr/local/bin/load-identity.sh
RUN chmod +x /usr/local/bin/load-identity.sh
ENTRYPOINT ["/usr/local/bin/load-identity.sh"]
