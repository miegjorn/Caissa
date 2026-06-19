#!/bin/sh
# Agent container entrypoint.
#
# Two modes:
#   Interactive (default): Guilhem in conversation with Pierre-Luc.
#     Writes MCP config from env vars, then runs `claude` interactively.
#
#   Task mode (TASK env var set): sub-agent Job dispatched by the dispatcher.
#     Writes AGENT_CONTEXT to /workspace/CLAUDE.md, runs `claude --print`,
#     posts the result to Farga as a Signal, then exits.
#
# Env vars:
#   FARGA_MCP_URL       MCP endpoint (interactive mode + task mode MCP config)
#   DISPATCHER_MCP_URL  Dispatcher MCP endpoint (interactive mode)
#   FARGA_URL           Farga REST base URL (task mode result posting)
#   TASK                If set: task mode. The prompt passed to claude --print.
#   AGENT_CONTEXT       Markdown context written to /workspace/CLAUDE.md (task mode)
#   SESSION_ID          Farga project to write result Signal under (task mode)
#   DOMAIN              Domain name for Signal source tag (task mode)
#   FACET               Facet name for Signal source tag (task mode)
#   ANTHROPIC_API_KEY   Required by claude

set -e

FARGA_MCP_URL="${FARGA_MCP_URL:-http://farga.occitan-system.svc.cluster.local:7500/mcp}"
DISPATCHER_MCP_URL="${DISPATCHER_MCP_URL:-http://dispatcher.agents.svc.cluster.local:9090/mcp}"
FARGA_URL="${FARGA_URL:-http://farga.occitan-system.svc.cluster.local:7500}"

mkdir -p /root/.claude /workspace

# Always write the MCP config — both modes need it.
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

if [ -n "${TASK:-}" ]; then
  # ── Task mode ──────────────────────────────────────────────────────────────
  # Write context as workspace CLAUDE.md if provided.
  if [ -n "${AGENT_CONTEXT:-}" ]; then
    printf '%s' "$AGENT_CONTEXT" > /workspace/CLAUDE.md
  fi

  # Run the task non-interactively, capture output.
  printf '%s' "$TASK" > /tmp/agent-task.txt
  OUTPUT=$(claude --print "$(cat /tmp/agent-task.txt)" 2>&1) || true

  # Post the result to Farga as a Signal under the session project.
  SESSION="${SESSION_ID:-agent-session}"
  SOURCE="${DOMAIN:-agent}/${FACET:-agent}"

  python3 - << PYEOF
import urllib.request, json, os, sys

farga_url = os.environ.get('FARGA_URL', 'http://farga.occitan-system.svc.cluster.local:7500')
session_id = os.environ.get('SESSION_ID', 'agent-session')
source = os.environ.get('DOMAIN', 'agent') + '/' + os.environ.get('FACET', 'agent')

output = """${OUTPUT}"""

payload = json.dumps({
    'project': session_id,
    'signals': [{
        'project': session_id,
        'content': output,
        'source': source
    }]
}).encode('utf-8')

req = urllib.request.Request(
    farga_url + '/signals',
    data=payload,
    headers={'Content-Type': 'application/json'},
    method='POST'
)
try:
    urllib.request.urlopen(req, timeout=10)
    print('[agent] result written to farga project: ' + session_id)
except Exception as e:
    print('[agent] warning: failed to write result to farga: ' + str(e), file=sys.stderr)
    # Don't fail the job — output already happened
PYEOF

else
  # ── Interactive mode ───────────────────────────────────────────────────────
  exec claude "$@"
fi
