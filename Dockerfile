FROM rust:latest AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
# caissa-core → amassada-core → fondament-core (transitive path deps), plus
# corrier-core and nervi-core (added for the Corrièr Matrix-gateway cutover:
# caissa-cli depends on corrier-core at ../Corrier/corrier-core, which itself
# depends on nervi-core at ../../nervi/nervi-core relative to Corrier/, and
# caissa-cli also depends on nervi-core directly at ../nervi/nervi-core for
# chat_loop.rs's own NerviClient use). Provide all four upstream repos as
# named build contexts so every relative path resolves: /Fondament →
# fondament-core (amassada-core's dep: ../../../Fondament/fondament-core);
# /Amassada → amassada-core (caissa-core's dep, via workspace:
# ../Amassada/crates/amassada-core); /Corrier → corrier-core
# (../Corrier/corrier-core); /nervi → nervi-core (../nervi/nervi-core).
WORKDIR /Fondament
COPY --from=fondament . .
WORKDIR /Amassada
COPY --from=amassada . .
WORKDIR /Corrier
COPY --from=corrier . .
WORKDIR /nervi
COPY --from=nervi . .
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
