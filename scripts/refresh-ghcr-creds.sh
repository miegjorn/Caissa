#!/usr/bin/env bash
# Recreate the ghcr-creds imagePullSecret from OpenBao (or env var for cold-start).
#
# Normal use (after seeding occitan/ghcr into OpenBao):
#   scripts/refresh-ghcr-creds.sh
#
# Cold-start / rotation outside OpenBao:
#   GHCR_PAT=<token> scripts/refresh-ghcr-creds.sh --from-env
#
# Seeding the token into OpenBao first:
#   echo -n "$GHCR_PAT" | scripts/seed-secret.sh occitan/ghcr
#
# Flags:
#   --from-env           read from GHCR_PAT env var instead of OpenBao
#   --username <user>    docker-username for ghcr.io (default: bedardpl)
#   --namespace <ns>     additional namespace (repeatable)
#   --ob-ns <namespace>  namespace OpenBao runs in (default: occitan-system)
set -euo pipefail

NAMESPACES=(occitan-system agents)
OB_NS=occitan-system
FROM_ENV=0
DOCKER_USER="${GHCR_USER:-bedardpl}"
MOUNT=secret
FIELD=value

while [[ $# -gt 0 ]]; do
  case "$1" in
    --from-env)     FROM_ENV=1;         shift   ;;
    --username)     DOCKER_USER="$2";   shift 2 ;;
    --namespace|-n) NAMESPACES+=("$2"); shift 2 ;;
    --ob-ns)        OB_NS="$2";         shift 2 ;;
    -h|--help)      sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

if [[ $FROM_ENV -eq 1 ]]; then
  [[ -n "${GHCR_PAT:-}" ]] || { echo "error: GHCR_PAT env var required with --from-env" >&2; exit 1; }
  TOKEN="$GHCR_PAT"
  echo "→ reading GHCR PAT from env (cold-start mode)"
else
  OBPOD="$(kubectl get pod -n "$OB_NS" -l app.kubernetes.io/name=openbao -o name | head -1)"
  [[ -n "$OBPOD" ]] || { echo "error: no openbao pod in $OB_NS — use --from-env for cold-start" >&2; exit 1; }
  ROOT_TOKEN="$(kubectl get secret openbao -n "$OB_NS" -o jsonpath='{.data.token}' | base64 -d)"
  TOKEN="$(kubectl exec -n "$OB_NS" "$OBPOD" -- sh -c \
    "BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN='$ROOT_TOKEN' bao kv get -field=$FIELD $MOUNT/occitan/ghcr")"
  [[ -n "$TOKEN" ]] || {
    echo "error: empty value at secret/occitan/ghcr — seed it first:" >&2
    echo "  echo -n \"\$GHCR_PAT\" | scripts/seed-secret.sh occitan/ghcr" >&2
    exit 1
  }
  echo "→ reading GHCR PAT from OpenBao (secret/occitan/ghcr, len=${#TOKEN})"
fi

for ns in "${NAMESPACES[@]}"; do
  kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f - 2>/dev/null | grep -v "^$" || true
  kubectl create secret docker-registry ghcr-creds \
    --namespace "$ns" \
    --docker-server=ghcr.io \
    --docker-username="$DOCKER_USER" \
    --docker-password="$TOKEN" \
    --dry-run=client -o yaml | kubectl apply -f -
  echo "✓ ghcr-creds updated in $ns"
done

echo ""
echo "Pods in ImagePullBackoff will retry automatically within ~5 min."
echo "To force immediate retry after a long outage:"
echo "  kubectl delete pods -n occitan-system -l app.kubernetes.io/part-of=occitan"
echo "  kubectl rollout restart deployment/guilhem -n agents"
