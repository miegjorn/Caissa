#!/bin/sh
# cluster-down.sh — graceful Occitan kind cluster shutdown.
#
# Order matters:
#   1. Stop port-forward LaunchAgents (prevents reconnection loops during shutdown)
#   2. Drain the kind node gracefully (kubelet gets SIGTERM, pods terminate cleanly)
#   3. Stop Colima VM

set -e

echo "[occitan] unloading port-forward LaunchAgents..."
launchctl unload ~/Library/LaunchAgents/occitan.portforward.synapse.plist 2>/dev/null || true
launchctl unload ~/Library/LaunchAgents/occitan.portforward.element.plist 2>/dev/null || true

# Kill any lingering kubectl port-forward processes that bypassed launchctl
pkill -f "kubectl port-forward -n occitan-system" 2>/dev/null || true

echo "[occitan] stopping kind node (graceful drain)..."
docker stop occitan-control-plane 2>/dev/null && echo "[occitan] kind node stopped" || echo "[occitan] kind node already stopped"

echo "[occitan] stopping Colima..."
colima stop

echo "[occitan] cluster down."
