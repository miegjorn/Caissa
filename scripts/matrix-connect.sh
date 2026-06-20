#!/usr/bin/env bash
# Start local port-forwards so the Matrix homeserver and Element Web are
# accessible from the Mac browser.
#
# With Colima + Intel QEMU networking, cluster IPs are not routable from
# the Mac host — port-forward is the only access path.
#
# What this does:
#   1. Adds 127.0.0.1  occitane.guilhem  to /etc/hosts (requires sudo — prompts once)
#   2. Forwards svc/synapse  → localhost:8008   (homeserver)
#   3. Forwards svc/element  → localhost:8080   (Element Web UI)
#
# To talk to Guilhem:
#   1. Run this script in a terminal (keep it open).
#   2. Open http://localhost:8080 in your browser.
#   3. Set homeserver to http://occitane.guilhem:8008
#      (or http://localhost:8008 if /etc/hosts isn't set up yet)
#   4. Log in as pierre-luc.
#   5. Open the DM with @charradissa:occitane.guilhem — that's Guilhem.
#      Send any plain text message; he replies.
#
# Note: @claude:occitane.guilhem is a regular user account, NOT the bot.
#       @charradissa:occitane.guilhem is the Guilhem agent.
set -euo pipefail

NS=occitan-system

# Add occitane.guilhem → localhost if not already there
if ! grep -q "occitane.guilhem" /etc/hosts 2>/dev/null; then
  echo "Adding occitane.guilhem to /etc/hosts (requires sudo)..."
  echo "127.0.0.1  occitane.guilhem" | sudo tee -a /etc/hosts
  echo "✓ /etc/hosts updated"
else
  echo "✓ occitane.guilhem already in /etc/hosts"
fi

echo "Starting port-forwards (Ctrl-C to stop both)..."
trap 'kill %1 %2 2>/dev/null; echo "port-forwards stopped"' INT TERM EXIT

kubectl port-forward svc/synapse -n "$NS" 8008:8008 &
kubectl port-forward svc/element -n "$NS" 8080:80  &

echo "  Synapse (homeserver) → http://localhost:8008"
echo "  Element Web (UI)     → http://localhost:8080"
echo ""
echo "Open http://localhost:8080 and set homeserver to http://occitane.guilhem:8008"
echo "Log in as pierre-luc. DM @charradissa:occitane.guilhem to talk to Guilhem."

wait
