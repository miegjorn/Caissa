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

# Mints a short-lived (1h) GitHub App installation token from the App
# credentials fetch-tokens wrote to /creds/, and (re)writes GH_TOKEN /
# GITHUB_TOKEN into /creds/tokens.env. Safe to call repeatedly — each call
# fully overwrites those two lines, preserving GITLAB_*/SYNAPSE_* lines.
mint_installation_token() {
  APP_ID=$(cat /creds/github-app-id)
  INSTALLATION_ID=$(cat /creds/github-app-installation-id)
  PRIVATE_KEY_FILE=/creds/github-app-private-key.pem

  NOW=$(date +%s)
  IAT=$((NOW - 60))
  EXP=$((NOW + 540))

  JWT_HEADER=$(printf '{"alg":"RS256","typ":"JWT"}' | openssl base64 -A | tr '+/' '-_' | tr -d '=')
  JWT_PAYLOAD=$(printf '{"iat":%s,"exp":%s,"iss":"%s"}' "$IAT" "$EXP" "$APP_ID" | openssl base64 -A | tr '+/' '-_' | tr -d '=')
  JWT_UNSIGNED="${JWT_HEADER}.${JWT_PAYLOAD}"
  JWT_SIGNATURE=$(printf '%s' "$JWT_UNSIGNED" | openssl dgst -sha256 -sign "$PRIVATE_KEY_FILE" -binary | openssl base64 -A | tr '+/' '-_' | tr -d '=')
  JWT="${JWT_UNSIGNED}.${JWT_SIGNATURE}"

  RESP_FILE=/tmp/gh-app-token-resp.json
  HTTP_CODE=$(curl -s -o "$RESP_FILE" -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${JWT}" \
    -H "Accept: application/vnd.github+json" \
    "https://api.github.com/app/installations/${INSTALLATION_ID}/access_tokens" || echo "000")

  if [ "$HTTP_CODE" != "201" ]; then
    echo "[entrypoint] failed to mint GitHub App installation token (HTTP ${HTTP_CODE})" >&2
    rm -f "$RESP_FILE"
    return 1
  fi

  INSTALL_TOKEN=$(python3 -c 'import sys, json; print(json.load(sys.stdin).get("token", ""))' < "$RESP_FILE" 2>/dev/null || echo "")
  rm -f "$RESP_FILE"

  if [ -z "$INSTALL_TOKEN" ]; then
    echo "[entrypoint] failed to parse GitHub App installation token response" >&2
    return 1
  fi

  grep -v '^export GH_TOKEN=\|^export GITHUB_TOKEN=' /creds/tokens.env > /creds/tokens.env.tmp 2>/dev/null || true
  {
    cat /creds/tokens.env.tmp 2>/dev/null
    echo "export GH_TOKEN='${INSTALL_TOKEN}'"
    echo "export GITHUB_TOKEN='${INSTALL_TOKEN}'"
  } > /creds/tokens.env
  rm -f /creds/tokens.env.tmp

  # Also refresh the git credential-store file the fetch-tokens initContainer
  # seeds — `git` via the credential.helper reads from this file, not from
  # GH_TOKEN/GITHUB_TOKEN, so it goes stale on its own schedule unless we
  # rewrite it here too. Preserves the GitLab line (not managed by this
  # function) and any other lines untouched.
  grep -v '^https://x-access-token:' /creds/.git-credentials > /creds/.git-credentials.tmp 2>/dev/null || true
  {
    cat /creds/.git-credentials.tmp 2>/dev/null
    echo "https://x-access-token:${INSTALL_TOKEN}@github.com"
  } > /creds/.git-credentials
  rm -f /creds/.git-credentials.tmp

  export GH_TOKEN="$INSTALL_TOKEN"
  export GITHUB_TOKEN="$INSTALL_TOKEN"
}

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
