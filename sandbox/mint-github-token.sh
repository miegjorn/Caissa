# Mints a short-lived (1h) GitHub App installation token from the App
# credentials fetch-tokens wrote to /creds/, and (re)writes GH_TOKEN /
# GITHUB_TOKEN into /creds/tokens.env. Safe to call repeatedly — each call
# fully overwrites those two lines, preserving GITLAB_*/SYNAPSE_* lines.
#
# Shared by entrypoint.sh (interactive claude sessions and dispatched task
# Jobs) and the guilhem/component-agents chart wrappers that exec `caissa
# listen`/`caissa ingest` directly, bypassing entrypoint.sh entirely. Source
# this file, then call mint_installation_token.
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
    echo "[mint-github-token] failed to mint GitHub App installation token (HTTP ${HTTP_CODE})" >&2
    rm -f "$RESP_FILE"
    return 1
  fi

  INSTALL_TOKEN=$(python3 -c 'import sys, json; print(json.load(sys.stdin).get("token", ""))' < "$RESP_FILE" 2>/dev/null || echo "")
  rm -f "$RESP_FILE"

  if [ -z "$INSTALL_TOKEN" ]; then
    echo "[mint-github-token] failed to parse GitHub App installation token response" >&2
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
