#!/usr/bin/env bash
# Run ONCE after the first fresh OpenBao deployment (or after a cluster rebuild that
# creates a new PVC). Reads the generated unseal-key and root token from the PVC and
# patches the 'openbao' k8s Secret in both occitan-system and agents namespaces so
# that Gardian (BAO_TOKEN) and Guilhem's initContainer stay in sync.
#
# Usage:
#   scripts/init-openbao.sh
#   scripts/init-openbao.sh --namespace occitan-system --namespace agents
set -euo pipefail

NAMESPACES=(occitan-system agents)
OB_NS=occitan-system

usage() { sed -n '2,8p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --namespace|-n) NAMESPACES+=("$2"); shift 2 ;;
    --ob-ns)        OB_NS="$2"; shift 2 ;;
    -h|--help)      usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage; exit 1 ;;
  esac
done

OBPOD=$(kubectl get pod -n "$OB_NS" -l app.kubernetes.io/name=openbao -o name | head -1)
[[ -n "$OBPOD" ]] || { echo "error: no openbao pod found in $OB_NS" >&2; exit 1; }

echo "=== reading keys from OpenBao PVC ==="
UNSEAL=$(kubectl exec -n "$OB_NS" "$OBPOD" -- cat /bao/data/.unseal-key 2>/dev/null) \
  || { echo "error: /bao/data/.unseal-key not found — has OpenBao been initialised?" >&2; exit 1; }
TOKEN=$(kubectl exec -n "$OB_NS" "$OBPOD" -- cat /bao/data/.root-token 2>/dev/null) \
  || { echo "error: /bao/data/.root-token not found — has OpenBao been initialised?" >&2; exit 1; }

echo "  unseal-key: len=${#UNSEAL} prefix=${UNSEAL:0:8}…"
echo "  root-token: len=${#TOKEN} prefix=${TOKEN:0:8}…"

echo "=== patching openbao k8s Secret in namespaces: ${NAMESPACES[*]} ==="
for ns in "${NAMESPACES[@]}"; do
  kubectl create secret generic openbao -n "$ns" \
    --from-literal=token="$TOKEN" \
    --from-literal=unseal-key="$UNSEAL" \
    --dry-run=client -o yaml | kubectl apply -f - 2>&1 | tail -1
  echo "  ✓ $ns"
done

echo "=== enabling KV v2 secrets engine at secret/ (no-op if already enabled) ==="
# In file-storage mode, no secrets engines are pre-mounted (unlike dev mode).
# Read token from the secret we just patched.
NEW_TOKEN=$(kubectl get secret openbao -n "$OB_NS" -o jsonpath='{.data.token}' | base64 -d)
kubectl exec -n "$OB_NS" "$OBPOD" -- sh -c \
  "BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN='$NEW_TOKEN' bao secrets enable -path=secret kv-v2 2>&1" \
  | grep -v "path is already in use" | head -1 || true

echo "=== restarting Gardian so it picks up the new BAO_TOKEN ==="
kubectl rollout restart deploy/gardian -n occitan-system 2>&1 | tail -1
echo "=== re-seed application secrets into OpenBao ==="
echo "  Run:"
echo "    echo -n \"\$ANTHROPIC_API_KEY\" | scripts/seed-secret.sh occitan/anthropic"
echo "    echo -n \"\$XAI_API_KEY\"         | scripts/seed-secret.sh occitan/xai"
echo "    echo -n \"\$GITHUB_TOKEN\"       | scripts/seed-secret.sh occitan/github"
echo "    echo -n \"\$GITLAB_TOKEN\"       | scripts/seed-secret.sh occitan/gitlab --restart agents/guilhem"
echo "    echo -n \"\$GHCR_PAT\"           | scripts/seed-secret.sh occitan/ghcr"
echo ""
echo "  Then recreate the ghcr-creds imagePullSecret from OpenBao:"
echo "    scripts/refresh-ghcr-creds.sh"
echo ""
echo "  To rotate the GHCR PAT later:"
echo "    echo -n \"\$NEW_PAT\" | scripts/seed-secret.sh occitan/ghcr"
echo "    scripts/refresh-ghcr-creds.sh"
