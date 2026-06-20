#!/usr/bin/env bash
# Put (or rotate) a secret value in OpenBao, then optionally restart the consumers
# that read it (their initContainers re-pull on the next pod start).
#
# The value is read from STDIN so it never lands in argv, shell history, or `ps`.
#
# Usage:
#   echo -n "<value>" | scripts/seed-secret.sh <path> [--restart <ns>/<deployment>]...
#
# Examples:
#   echo -n "$GITHUB_TOKEN"             | scripts/seed-secret.sh occitan/github
#   echo -n "$GITLAN_PAT_CLASSIC_TOKEN" | scripts/seed-secret.sh occitan/gitlab --restart agents/guilhem
#   echo -n "$ANTHROPIC_API_KEY"        | scripts/seed-secret.sh occitan/anthropic
#
# Flags:
#   --restart <ns>/<deployment>   restart a deployment after seeding (repeatable)
#   --mount <mount>               KV v2 mount (default: secret)
#   --field <field>               field name within the secret (default: value)
#   --ns <namespace>              namespace OpenBao runs in (default: occitan-system)
set -euo pipefail

MOUNT=secret
FIELD=value
OB_NS=occitan-system
SECRET_PATH=""
RESTARTS=()
HAVE_RESTARTS=0

usage() { sed -n '2,18p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --restart) RESTARTS+=("$2"); shift 2 ;;
    --mount)   MOUNT="$2";       shift 2 ;;
    --field)   FIELD="$2";       shift 2 ;;
    --ns)      OB_NS="$2";       shift 2 ;;
    -h|--help) usage; exit 0 ;;
    -*)        echo "unknown flag: $1" >&2; usage; exit 1 ;;
    *)         SECRET_PATH="$1"; shift ;;
  esac
done

[[ -n "$SECRET_PATH" ]] || { echo "error: secret path required (e.g. occitan/gitlab)" >&2; usage; exit 1; }
if [[ -t 0 ]]; then
  echo "error: pipe the value on stdin, e.g.: echo -n \"\$TOKEN\" | $0 $SECRET_PATH" >&2
  exit 1
fi
VALUE="$(cat)"
[[ -n "$VALUE" ]] || { echo "error: empty value on stdin" >&2; exit 1; }

OBPOD="$(kubectl get pod -n "$OB_NS" -l app.kubernetes.io/name=openbao -o name | head -1)"
[[ -n "$OBPOD" ]] || { echo "error: no openbao pod found in namespace $OB_NS" >&2; exit 1; }

# Resolve the root token: prefer the k8s secret (file-storage mode), fall back to
# BAO_DEV_ROOT_TOKEN_ID env (dev mode — only set inside the pod in dev mode).
ROOT_TOKEN="$(kubectl get secret openbao -n "$OB_NS" -o jsonpath='{.data.token}' 2>/dev/null | base64 -d)"
[[ -n "$ROOT_TOKEN" ]] || { echo "error: could not read openbao token from secret/$OB_NS/openbao" >&2; exit 1; }

printf '%s' "$VALUE" | kubectl exec -i -n "$OB_NS" "$OBPOD" -- sh -c \
  "BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN='$ROOT_TOKEN' bao kv put $MOUNT/$SECRET_PATH $FIELD=-" >/dev/null

echo "✓ seeded $MOUNT/$SECRET_PATH (len=${#VALUE}, prefix=${VALUE:0:6}…)"

if [[ ${#RESTARTS[@]} -gt 0 ]]; then
  for r in "${RESTARTS[@]}"; do
    ns="${r%%/*}"; dep="${r#*/}"
    kubectl rollout restart -n "$ns" "deployment/$dep" >/dev/null
    echo "✓ restarted deployment $ns/$dep (initContainer re-pulls the secret)"
  done
fi
