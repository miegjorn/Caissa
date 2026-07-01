#!/usr/bin/env bash
# matrix-reset.sh — wipe and reconstruct the Occitan Matrix space.
#
# Run AFTER deploying a new Charradissa image that has the updated namespace
# (guilhem + charradissa replacing charradissa-agent).
#
# What this does:
#   1. Drops and recreates the synapse PostgreSQL database (all users/rooms/messages gone)
#   2. Restarts Synapse (initContainer regenerates registration from current image)
#   3. Creates users: admin, pierre-luc, guilhem (virtual via AS), claude-session
#   4. Creates 8 component rooms + Occitan space
#   5. Outputs a charradissa.toml [agents.routes] stanza with new room IDs
#   6. Updates the charradissa-config ConfigMap in-cluster with new room IDs
#   7. Restarts Charradissa to pick up the new routing
#
# Usage: bash scripts/matrix-reset.sh
#
# Prerequisites:
#   - kubectl pointing to the occitan kind cluster
#   - Port-forward to Synapse will be started automatically
#   - The charradissa AS secret must already exist (it is not recreated here)

set -euo pipefail

NS=occitan-system
HOMESERVER="http://localhost:8008"
SERVER_NAME="occitane.guilhem"

green()  { echo -e "\033[0;32m✓ $*\033[0m"; }
blue()   { echo -e "\033[0;34m→ $*\033[0m"; }
yellow() { echo -e "\033[0;33m! $*\033[0m"; }
die()    { echo -e "\033[0;31m✗ $*\033[0m" >&2; exit 1; }

# ─── Step 1: Drop and recreate the synapse database ──────────────────────────
blue "Scaling down Synapse..."
kubectl scale deployment synapse -n "$NS" --replicas=0
kubectl rollout status deployment/synapse -n "$NS" --timeout=60s || true

blue "Dropping and recreating synapse database in PostgreSQL..."
kubectl exec -n "$NS" statefulset/occitan-postgresql -- \
  psql -U postgres -c "DROP DATABASE IF EXISTS synapse;" 2>/dev/null || \
  kubectl exec -n "$NS" statefulset/occitan-postgresql -- \
    psql -U synapse -d postgres -c "DROP DATABASE IF EXISTS synapse;" 2>/dev/null || \
    die "Could not drop synapse database. Check PostgreSQL pod name."
kubectl exec -n "$NS" statefulset/occitan-postgresql -- \
  psql -U postgres -c "CREATE DATABASE synapse OWNER synapse;" 2>/dev/null || \
  kubectl exec -n "$NS" statefulset/occitan-postgresql -- \
    psql -U synapse -d postgres -c "CREATE DATABASE synapse;" 2>/dev/null

green "Database wiped"

# ─── Step 2: Restart Synapse (initContainer regenerates registration) ─────────
blue "Scaling Synapse back up..."
kubectl scale deployment synapse -n "$NS" --replicas=1

blue "Waiting for Synapse to be ready (initContainer runs first)..."
kubectl rollout status deployment/synapse -n "$NS" --timeout=120s

# ─── Start port-forward ───────────────────────────────────────────────────────
blue "Starting port-forward to Synapse..."
kubectl port-forward svc/synapse -n "$NS" 8008:8008 &
PF_PID=$!
trap "kill $PF_PID 2>/dev/null; echo 'port-forward stopped'" EXIT

sleep 3
until curl -sf "${HOMESERVER}/health" > /dev/null 2>&1; do
  echo "  waiting for Synapse..."
  sleep 2
done
green "Synapse is ready"

# ─── Fetch registration shared secret ────────────────────────────────────────
SHARED_SECRET=$(kubectl get secret synapse-registration-secret -n "$NS" \
  -o jsonpath='{.data.registration-shared-secret}' | base64 -d)

# Helper: register a user with the shared secret
register_user() {
  local username="$1" password="$2" admin="${3:-false}"
  local nonce
  nonce=$(curl -sf "${HOMESERVER}/_synapse/admin/v1/register" | jq -r '.nonce')
  local mac
  mac=$(echo -n "${nonce}\x00${username}\x00${password}\x00$([ "$admin" = "true" ] && echo admin || echo notadmin)" \
    | openssl dgst -sha1 -hmac "${SHARED_SECRET}" | awk '{print $2}')
  curl -sf -X POST "${HOMESERVER}/_synapse/admin/v1/register" \
    -H "Content-Type: application/json" \
    -d "{\"nonce\":\"${nonce}\",\"username\":\"${username}\",\"password\":\"${password}\",\"admin\":${admin},\"mac\":\"${mac}\"}" \
    | jq -r '.access_token'
}

# ─── Step 3: Create users ─────────────────────────────────────────────────────
blue "Creating admin user..."
ADMIN_TOKEN=$(register_user "occitan-admin" "$(openssl rand -hex 16)" "true")
green "Admin user created"

blue "Creating pierre-luc user..."
PIERRE_LUC_TOKEN=$(register_user "pierre-luc" "pierre-luc-password" "false")
green "pierre-luc user created"

blue "Creating claude-session user..."
CLAUDE_SESSION_TOKEN=$(register_user "claude-session" "$(openssl rand -hex 16)" "false")
green "claude-session user created (save token for ~/.claude/secrets/occitan-matrix-claude-session.env)"
echo "  MATRIX_ACCESS_TOKEN=${CLAUDE_SESSION_TOKEN}" > /tmp/claude-session-token.env
echo "  MATRIX_USER_ID=@claude-session:${SERVER_NAME}"

# ─── Step 4: Create rooms ─────────────────────────────────────────────────────
# Rooms are created by pierre-luc (so AS can join/manage them).
# The AS will be invited after creation.

blue "Creating component rooms..."
AS_TOKEN=$(kubectl get secret charradissa -n "$NS" -o jsonpath='{.data.as-token}' | base64 -d)

create_room() {
  local name="$1" alias="$2"
  curl -sf -X POST "${HOMESERVER}/_matrix/client/v3/createRoom" \
    -H "Authorization: Bearer ${PIERRE_LUC_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "{
      \"room_alias_name\": \"${alias}\",
      \"name\": \"${name}\",
      \"preset\": \"trusted_private_chat\",
      \"visibility\": \"private\"
    }" | jq -r '.room_id'
}

ROOM_GUILHEM=$(create_room "Guilhem" "guilhem");           green "guilhem room: $ROOM_GUILHEM"
ROOM_GARDIAN=$(create_room "Gardian" "gardian");           green "gardian room: $ROOM_GARDIAN"
ROOM_FONDAMENT=$(create_room "Fondament" "fondament");     green "fondament room: $ROOM_FONDAMENT"
ROOM_FARGA=$(create_room "Farga" "farga");                 green "farga room: $ROOM_FARGA"
ROOM_AMASSADA=$(create_room "Amassada" "amassada");        green "amassada room: $ROOM_AMASSADA"
ROOM_COR=$(create_room "Cor" "cor");                       green "cor room: $ROOM_COR"
ROOM_CAISSA=$(create_room "Caissa" "caissa");              green "caissa room: $ROOM_CAISSA"
ROOM_CHARRADISSA=$(create_room "Charradissa" "charradissa"); green "charradissa room: $ROOM_CHARRADISSA"
ROOM_NERVI=$(create_room "Nervi" "nervi");                 green "nervi room: $ROOM_NERVI"
ROOM_APPROVAL=$(create_room "Code Approval" "occitan-code-approval"); green "approval room: $ROOM_APPROVAL"

# ─── Step 5: Create Occitan space ─────────────────────────────────────────────
blue "Creating Occitan space..."
SPACE_ID=$(curl -sf -X POST "${HOMESERVER}/_matrix/client/v3/createRoom" \
  -H "Authorization: Bearer ${PIERRE_LUC_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "room_alias_name": "occitan",
    "name": "Occitan",
    "creation_content": {"type": "m.space"},
    "topic": "Occitan stack",
    "visibility": "private"
  }' | jq -r '.room_id')
green "Occitan space: $SPACE_ID"

# Add all component rooms as space children
for ROOM_ID in "$ROOM_GUILHEM" "$ROOM_GARDIAN" "$ROOM_FONDAMENT" "$ROOM_FARGA" \
               "$ROOM_AMASSADA" "$ROOM_COR" "$ROOM_CAISSA" "$ROOM_CHARRADISSA" "$ROOM_NERVI" "$ROOM_APPROVAL"; do
  ENCODED=$(python3 -c "import urllib.parse; print(urllib.parse.quote('$ROOM_ID'))")
  curl -sf -X PUT "${HOMESERVER}/_matrix/client/v3/rooms/${ENCODED}/state/m.space.child/${ENCODED}" \
    -H "Authorization: Bearer ${PIERRE_LUC_TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"via": ["occitane.guilhem"]}' > /dev/null
done
green "All rooms added to Occitan space"

# ─── Step 6: Output new room IDs for config update ────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "New room IDs — paste into charradissa.toml [agents.routes] and guilhem.yaml:"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "[agents.routes]"
echo "\"${ROOM_GARDIAN}\"    = \"http://gardian-agent.agents.svc.cluster.local:8080\""
echo "\"${ROOM_FONDAMENT}\"  = \"http://fondament-agent.agents.svc.cluster.local:8080\""
echo "\"${ROOM_FARGA}\"      = \"http://farga-agent.agents.svc.cluster.local:8080\""
echo "\"${ROOM_AMASSADA}\"   = \"http://amassada-agent.agents.svc.cluster.local:8080\""
echo "\"${ROOM_COR}\"        = \"http://cor-agent.agents.svc.cluster.local:8080\""
echo "\"${ROOM_CAISSA}\"     = \"http://caissa-agent.agents.svc.cluster.local:8080\""
echo "\"${ROOM_CHARRADISSA}\" = \"http://charradissa-agent.agents.svc.cluster.local:8080\""
echo "\"${ROOM_NERVI}\"      = \"http://nervi-agent.agents.svc.cluster.local:8080\""
echo ""
echo "charradissa.approvalRoomId (paste into Caissa/deploy/charts/occitan/values.yaml or values-production.yaml, then commit — this is a Helm value, NOT part of the ConfigMap patch below):"
echo "  approvalRoomId: \"${ROOM_APPROVAL}\""
echo ""

# ─── Step 7: Patch the charradissa-config ConfigMap in-cluster ───────────────
blue "Patching charradissa-config ConfigMap with new room IDs..."

NEW_ROUTES=$(cat <<TOML
[agents.routes]
"${ROOM_GARDIAN}" = "http://gardian-agent.agents.svc.cluster.local:8080"
"${ROOM_FONDAMENT}" = "http://fondament-agent.agents.svc.cluster.local:8080"
"${ROOM_FARGA}" = "http://farga-agent.agents.svc.cluster.local:8080"
"${ROOM_AMASSADA}" = "http://amassada-agent.agents.svc.cluster.local:8080"
"${ROOM_COR}" = "http://cor-agent.agents.svc.cluster.local:8080"
"${ROOM_CAISSA}" = "http://caissa-agent.agents.svc.cluster.local:8080"
"${ROOM_CHARRADISSA}" = "http://charradissa-agent.agents.svc.cluster.local:8080"
"${ROOM_NERVI}" = "http://nervi-agent.agents.svc.cluster.local:8080"
TOML
)

# Build complete new charradissa.toml (preserving non-routes sections)
NEW_TOML=$(cat <<TOML
[org]
name = "occitan"
homeserver = "http://synapse:8008"

[backend]
type = "matrix"

[concierge]
archival_interval_hours = 24
convergence_interval_hours = 6
daily_token_budget = 100000

[approval]
timeout_minutes = 60

[tasks]
type = "none"

[projects]
autodiscover = false

[agents]
default = "http://guilhem.agents.svc.cluster.local:8080"

${NEW_ROUTES}
TOML
)

kubectl create configmap charradissa-config \
  --from-literal="charradissa.toml=${NEW_TOML}" \
  -n "$NS" --dry-run=client -o yaml | kubectl apply -f -

green "charradissa-config updated"

# ─── Step 8: Restart Charradissa ─────────────────────────────────────────────
blue "Restarting Charradissa to pick up new room routing..."
kubectl rollout restart deployment/charradissa -n "$NS"
kubectl rollout status deployment/charradissa -n "$NS" --timeout=60s
green "Charradissa restarted"

# ─── Summary ──────────────────────────────────────────────────────────────────
echo ""
green "Matrix reconstruction complete."
echo ""
echo "Space:          $SPACE_ID"
echo "Guilhem room:   $ROOM_GUILHEM"
echo ""
echo "Next: update guilhem.yaml component room IDs and commit to Fondament."
echo "  - gardian:     $ROOM_GARDIAN"
echo "  - fondament:   $ROOM_FONDAMENT"
echo "  - farga:       $ROOM_FARGA"
echo "  - amassada:    $ROOM_AMASSADA"
echo "  - cor:         $ROOM_COR"
echo "  - caissa:      $ROOM_CAISSA"
echo "  - charradissa: $ROOM_CHARRADISSA"
echo "  - nervi:       $ROOM_NERVI"
echo ""
echo "Claude-session token saved to /tmp/claude-session-token.env"
echo "Update ~/.claude/secrets/occitan-matrix-claude-session.env with that token."
