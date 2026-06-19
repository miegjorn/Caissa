#!/bin/sh
# Agent container entrypoint.
# Writes the Claude Code MCP config from env vars, then launches claude.
#
# Env vars:
#   FARGA_MCP_URL       — Farga MCP endpoint (default: cluster-internal DNS)
#   DISPATCHER_MCP_URL  — Dispatcher MCP endpoint (default: cluster-internal DNS)
#   ANTHROPIC_API_KEY   — Required by claude

set -e

FARGA_MCP_URL="${FARGA_MCP_URL:-http://farga.occitan-system.svc.cluster.local:7500/mcp}"
DISPATCHER_MCP_URL="${DISPATCHER_MCP_URL:-http://dispatcher.agents.svc.cluster.local:9090/mcp}"

mkdir -p /root/.claude

cat > /root/.claude/claude_desktop_config.json << EOF
{
  "mcpServers": {
    "farga": {
      "type": "http",
      "url": "${FARGA_MCP_URL}"
    },
    "dispatcher": {
      "type": "http",
      "url": "${DISPATCHER_MCP_URL}"
    }
  }
}
EOF

exec claude "$@"
