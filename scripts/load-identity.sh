#!/bin/sh
# load-identity.sh — fetches Guilhem's CLAUDE.md from Farga at startup.
# Runs before `caissa` so every instance boots with the current identity,
# regardless of what is baked into the image.
#
# Falls through silently if Farga is unreachable — the image-baked
# CLAUDE.md remains as fallback.
set -e

FARGA_URL="${FARGA_URL:-}"

if [ -n "$FARGA_URL" ]; then
    content=$(curl -sf "${FARGA_URL}/artifacts/occitan" \
        | jq -r '.[] | select(.title == "guilhem-identity") | .content' \
        2>/dev/null || true)
    if [ -n "$content" ]; then
        mkdir -p /root/.claude
        # Strip the HTML comment header written by write_artifact
        content=$(printf '%s' "$content" | sed '/^<!--/,/-->$/d')
        printf '%s\n' "$content" > /root/.claude/CLAUDE.md
    fi
fi

exec caissa "$@"
