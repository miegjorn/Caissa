FROM rust:1.90-slim AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
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
