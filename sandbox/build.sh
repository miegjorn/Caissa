#!/usr/bin/env bash
# Build the caissa-sandbox:latest image.
# Run from the Caissa repo root.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

echo "Building caissa CLI binary..."
cargo build --release -p caissa-cli

echo "Building sandbox image..."
docker build -f sandbox/Dockerfile -t caissa-sandbox:latest .

echo "Done. Test with: docker run --rm caissa-sandbox:latest 'echo hello'"
