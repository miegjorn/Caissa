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

# MODEL selects the LLM backend for model-agnostic complementary runs.
# "claude*" (default) uses the claude CLI (full MCP/agentic).
# "grok*" uses the basic xAI curl shim below (no MCP, no full agentic loop).
# For proper agentic Grok use, configure the participant with an endpoint that
# points at a Grok-backed service implementing the Amassada /turn protocol.
MODEL="${MODEL:-claude}"

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

  # Source OpenBao-provided git/gh credentials if the fetch-tokens init
  # container ran (it always does for dispatched agent Jobs — see
  # build_job in caissa-cli/src/commands/dispatch.rs).
  [ -f /creds/tokens.env ] && . /creds/tokens.env
  export GIT_CONFIG_GLOBAL=/creds/.gitconfig

  # Run the task non-interactively, capture output. --mcp-config connects
  # the farga/dispatcher MCP servers configured above; --allowed-tools is
  # required for ANY tool call to succeed in headless mode (no interactive
  # approval is possible). ALLOWED_TOOLS is set by the dispatcher from the
  # facet's tools.always_on list (see Fondament definitions/fondament/*.yaml);
  # the fallback here is intentionally read-only.
  printf '%s' "$TASK" > /tmp/agent-task.txt
  if echo "$MODEL" | grep -qi "^grok"; then
    # Basic Grok one-shot via xAI API (OpenAI-compatible).
    #
    # This is a lightweight, non-agentic shim for simple task execution.
    # It has no MCP tool access and no full agentic loop (no claude --print + tools).
    #
    # Full agentic/MCP support for Grok models is intended to be provided by
    # routing through an Amassada endpoint (POST /turn) to a dedicated Grok-backed
    # service (e.g. something like grok-adversary.agents.svc...).
    # The example endpoint in tests/canvases is illustrative; the service does not
    # exist yet. Until such an endpoint service is built, this basic curl path is
    # the only way to invoke grok* models from dispatched tasks.
    #
    # Requires XAI_API_KEY. Uses /workspace/CLAUDE.md as system prompt if present.
    echo "[agent] Grok model $MODEL detected - basic xAI call"
    SYSTEM=$(cat /workspace/CLAUDE.md 2>/dev/null | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()))' || echo '""')
    USER=$(cat /tmp/agent-task.txt | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()))')
    MODEL_JSON=$(python3 -c 'import sys,json; print(json.dumps(sys.argv[1]))' "$MODEL")
    OUTPUT=$(curl -s -H "Authorization: Bearer ${XAI_API_KEY}" \
      -H "Content-Type: application/json" \
      -d "{\"model\": $MODEL_JSON, \"messages\": [{\"role\":\"system\",\"content\":$SYSTEM}, {\"role\":\"user\",\"content\":$USER}] }" \
      https://api.x.ai/v1/chat/completions | python3 -c '
import sys, json
data = json.load(sys.stdin)
print(data.get("choices",[{}])[0].get("message",{}).get("content",""))
' 2>&1) || true
  else
    OUTPUT=$(claude --print "$(cat /tmp/agent-task.txt)" \
      --mcp-config /root/.claude/claude_desktop_config.json \
      --allowed-tools "${ALLOWED_TOOLS:-mcp__farga__search_signals,mcp__farga__read_context}" \
      2>&1) || true
  fi

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
