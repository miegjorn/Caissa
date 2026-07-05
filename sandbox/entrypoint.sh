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
#   FARGA_MCP_URL             MCP endpoint (interactive mode + task mode MCP config)
#   DISPATCHER_MCP_URL        Dispatcher MCP endpoint (interactive mode)
#   FARGA_URL                 Farga REST base URL (task mode result posting)
#   NERVI_MCP_URL             Nervi MCP endpoint (task mode result publishing, alongside Farga)
#   ASSIGNMENT_REPLY_SUBJECT  NATS subject to publish the task result to via Nervi (task mode);
#                             set by the dispatcher for jobs dispatched through invoke_agent.
#                             Result publishing to Nervi is skipped if unset.
#   TASK                If set: task mode. The prompt passed to claude --print.
#   AGENT_CONTEXT       Markdown context written to /workspace/CLAUDE.md (task mode)
#   SESSION_ID          Farga project to write result Signal under (task mode)
#   DOMAIN              Domain name for Signal source tag (task mode)
#   FACET               Facet name for Signal source tag (task mode)
#   ANTHROPIC_API_KEY   Required by claude

# MODEL selects the LLM backend for model-agnostic complementary runs.
# "claude*" (default) uses the claude CLI (full MCP/agentic loop).
# Anything else uses the OpenAI-compatible shim below (no MCP, no agentic loop).
# All major providers speak OpenAI-compatible APIs: xAI, OpenAI, Qwen, Mistral, etc.
# For proper agentic use with non-Claude models, configure the participant with an
# endpoint pointing at a service implementing the Amassada /turn protocol.
#
# Provider auto-detection (can always override with OPENAI_API_BASE + OPENAI_API_KEY):
#   grok* / xai:*          → api.x.ai                           key: XAI_API_KEY
#   gpt-* / o1* / o3* / o4* / openai:* → api.openai.com        key: OPENAI_API_KEY
#   gemini-* / google:*    → generativelanguage.googleapis.com  key: GEMINI_API_KEY
#   qwen-* / qwq-*         → dashscope (aliyun)                 key: QWEN_API_KEY
#   anything else           → OPENAI_API_BASE required, key: OPENAI_API_KEY
MODEL="${MODEL:-claude}"

# mint_installation_token() — shared with the guilhem/component-agents chart
# wrappers that exec `caissa listen`/`caissa ingest` directly (bypassing this
# script's own modes entirely). Kept in one file so the logic isn't
# triplicated across entrypoint.sh and both charts.
. /usr/local/bin/mint-github-token.sh

set -e

FARGA_MCP_URL="${FARGA_MCP_URL:-http://farga.occitan-system.svc.cluster.local:7500/mcp}"
DISPATCHER_MCP_URL="${DISPATCHER_MCP_URL:-http://dispatcher.agents.svc.cluster.local:9090/mcp}"
FARGA_URL="${FARGA_URL:-http://farga.occitan-system.svc.cluster.local:7500}"

mkdir -p /root/.claude /workspace

# Full bypass — container is a trusted boundary, no per-call prompts for
# MCP, Bash, or kubectl.
cat > /root/.claude/settings.json << 'EOF'
{
  "permissions": {
    "allow": [
      "Bash(*)",
      "Bash(kubectl *)"
    ],
    "defaultMode": "bypassPermissions"
  }
}
EOF

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
  if [ -f /creds/github-app-id ]; then
    mint_installation_token || echo "[entrypoint] continuing without a fresh GitHub token" >&2
  fi
  export GIT_CONFIG_GLOBAL=/creds/.gitconfig

  # Run the task non-interactively, capture output. --mcp-config connects
  # the farga/dispatcher MCP servers configured above; --allowed-tools is
  # required for ANY tool call to succeed in headless mode (no interactive
  # approval is possible). ALLOWED_TOOLS is set by the dispatcher from the
  # facet's tools.always_on list (see Fondament definitions/fondament/*.yaml);
  # the fallback here is intentionally read-only.
  printf '%s' "$TASK" > /tmp/agent-task.txt
  if echo "$MODEL" | grep -qi "^claude"; then
    # Claude path — full MCP/agentic loop via claude CLI.
    OUTPUT=$(claude --print "$(cat /tmp/agent-task.txt)" \
      --mcp-config /root/.claude/claude_desktop_config.json \
      --allowed-tools "${ALLOWED_TOOLS:-mcp__farga__search_signals,mcp__farga__read_context}" \
      2>&1) || true
  else
    # OpenAI-compatible shim — single-shot, no MCP, no agentic loop.
    # Covers xAI (grok*), OpenAI (gpt-*/o1/o3/o4), Qwen (qwen-*/qwq-*), and any
    # other OpenAI-compatible provider. For proper agentic use, route via an
    # Amassada endpoint implementing POST /turn instead.
    case "$MODEL" in
      grok*|xai:*)
        _API_BASE="${OPENAI_API_BASE:-https://api.x.ai/v1}"
        _API_KEY="${XAI_API_KEY:-${OPENAI_API_KEY:-}}"
        ;;
      gpt-*|o1*|o3*|o4*|openai:*)
        _API_BASE="${OPENAI_API_BASE:-https://api.openai.com/v1}"
        _API_KEY="${OPENAI_API_KEY:-}"
        ;;
      gemini-*|google:*)
        _API_BASE="${OPENAI_API_BASE:-https://generativelanguage.googleapis.com/v1beta/openai}"
        _API_KEY="${GEMINI_API_KEY:-${OPENAI_API_KEY:-}}"
        ;;
      qwen-*|qwq-*)
        _API_BASE="${OPENAI_API_BASE:-https://dashscope.aliyuncs.com/compatible-mode/v1}"
        _API_KEY="${QWEN_API_KEY:-${OPENAI_API_KEY:-}}"
        ;;
      *)
        _API_BASE="${OPENAI_API_BASE:-}"
        _API_KEY="${OPENAI_API_KEY:-}"
        ;;
    esac
    echo "[agent] model $MODEL → $_API_BASE"
    SYSTEM=$(cat /workspace/CLAUDE.md 2>/dev/null | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()))' || echo '""')
    USER=$(cat /tmp/agent-task.txt | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()))')
    MODEL_JSON=$(python3 -c 'import sys,json; print(json.dumps(sys.argv[1]))' "$MODEL")
    OUTPUT=$(curl -s -H "Authorization: Bearer ${_API_KEY}" \
      -H "Content-Type: application/json" \
      -d "{\"model\": $MODEL_JSON, \"messages\": [{\"role\":\"system\",\"content\":$SYSTEM},{\"role\":\"user\",\"content\":$USER}]}" \
      "${_API_BASE}/chat/completions" | python3 -c '
import sys, json
data = json.load(sys.stdin)
print(data.get("choices",[{}])[0].get("message",{}).get("content",""))
' 2>&1) || true
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

  # Also publish the result to Nervi on the assignment reply subject the
  # dispatcher minted for this job (see caissa-cli/src/commands/dispatch.rs
  # invoke_agent / create_agent_job), so get_agent_result can read it without
  # polling Farga. No-op unless both NERVI_MCP_URL and ASSIGNMENT_REPLY_SUBJECT
  # are set — i.e. this only fires for jobs dispatched through the assignment
  # mechanism; anything else (e.g. manual/legacy invocations) is unaffected.
  if [ -n "${NERVI_MCP_URL:-}" ] && [ -n "${ASSIGNMENT_REPLY_SUBJECT:-}" ]; then
    python3 - << PYEOF
import urllib.request, json, os, sys

nervi_mcp_url = os.environ.get('NERVI_MCP_URL', '')
assignment_reply_subject = os.environ.get('ASSIGNMENT_REPLY_SUBJECT', '')

output = """${OUTPUT}"""

mcp_body = json.dumps({
    'jsonrpc': '2.0',
    'id': 1,
    'method': 'tools/call',
    'params': {
        'name': 'nervi_publish',
        'arguments': {
            'subject': assignment_reply_subject,
            'qualifier': 'info',
            'payload': output
        }
    }
}).encode('utf-8')

req = urllib.request.Request(
    nervi_mcp_url,
    data=mcp_body,
    headers={
        'Content-Type': 'application/json',
        'Accept': 'application/json, text/event-stream'
    },
    method='POST'
)

def check_nervi_publish_response(body):
    """Classify a raw Nervi MCP tools/call HTTP response body as success or
    failure. Nervi's streamable-http transport frames each response as SSE:
    'event: message\\ndata: {<json-rpc response>}\\n\\n'. HTTP 200 is returned
    even for a JSON-RPC-level or tool-input-validation failure (e.g. a
    rejected nervi_publish call missing the required qualifier argument),
    so the body itself must be parsed and inspected for a top-level 'error'
    field or a 'result.isError == true'. Mirrors
    caissa-cli/src/commands/watch.rs::check_nervi_publish_response.
    Returns None on a clean result, or the error text on a detected failure.
    """
    json_str = body
    for line in body.splitlines():
        if line.startswith('data: '):
            json_str = line[len('data: '):]
            break

    try:
        parsed = json.loads(json_str)
    except Exception as e:
        return 'could not parse nervi MCP response body: %s (body: %s)' % (e, body)

    if isinstance(parsed, dict) and 'error' in parsed:
        err = parsed['error'] or {}
        return 'jsonrpc error: %s' % err.get('message', 'unknown error')

    result = parsed.get('result', {}) if isinstance(parsed, dict) else {}
    if result.get('isError'):
        content = result.get('content') or []
        text = content[0].get('text') if content and isinstance(content[0], dict) else None
        return text or '(no error text in response)'

    return None

try:
    resp = urllib.request.urlopen(req, timeout=10)
    body = resp.read().decode('utf-8', errors='replace')
    error = check_nervi_publish_response(body)
    if error:
        print('[agent] warning: nervi rejected result publish to ' + assignment_reply_subject + ': ' + error, file=sys.stderr)
    else:
        print('[agent] result published to nervi subject: ' + assignment_reply_subject)
except Exception as e:
    print('[agent] warning: failed to publish result to nervi: ' + str(e), file=sys.stderr)
    # Don't fail the job — output already happened (and Farga already has it)
PYEOF
  fi

else
  # ── Interactive mode ───────────────────────────────────────────────────────
  [ -f /creds/tokens.env ] && . /creds/tokens.env
  if [ -f /creds/github-app-id ]; then
    mint_installation_token || echo "[entrypoint] continuing without a fresh GitHub token" >&2
    # Refresh every 45 minutes (installation tokens expire after 1h). This
    # rewrites /creds/tokens.env; BASH_ENV below makes each freshly-spawned
    # bash subshell (i.e. every Bash tool call claude makes) re-read it, so
    # long sessions don't run on a stale token past the 1h mark. The `||`
    # fallback ensures a single failed mint never trips `set -e` (inherited
    # into this backgrounded subshell) and silently kills the refresh loop.
    (while true; do sleep 2700; mint_installation_token || echo "[entrypoint] token refresh failed, will retry next cycle" >&2; done) &
    export BASH_ENV=/creds/tokens.env
  fi
  export GIT_CONFIG_GLOBAL=/creds/.gitconfig
  exec claude "$@"
fi
