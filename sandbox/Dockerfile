# caissa-sandbox:latest
# Isolated development environment for AI agent tool execution.
#
# Runs with --network none and a memory cap (see caissa sandbox command).
# The sidecar reporter writes tool invocations to a volume-mounted file
# at /caissa/reports/invocations.jsonl; the host picks this up and relays
# to Farga after the sandbox exits.
#
# Build: docker build -f sandbox/Dockerfile -t caissa-sandbox:latest .
#        (run from the Caissa repo root so the caissa binary is available)

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    bash \
    curl \
    git \
    python3 \
    python3-pip \
    nodejs \
    npm \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create the report directory for sidecar output
RUN mkdir -p /caissa/reports

# Copy the caissa binary (built before running docker build)
COPY target/release/caissa /usr/local/bin/caissa

# Default working directory for agent tasks
WORKDIR /workspace

# By default, run bash. The caissa sandbox command passes user args here.
ENTRYPOINT ["/bin/bash", "-c"]
CMD ["echo 'caissa sandbox: no command provided'"]
