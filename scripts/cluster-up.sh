#!/bin/sh
# cluster-up.sh — start the Occitan kind cluster and restore connectivity.
#
# Order:
#   1. Start Colima
#   2. Wait for Docker and kind node to come up (auto-restores running containers)
#   3. Wait for k8s API and critical workloads
#   4. Restore port-forward LaunchAgents
#   5. Add host route for kind network (needs sudo — skipped silently if not available)

set -e

echo "[occitan] starting Colima..."
colima start

echo "[occitan] waiting for Docker daemon..."
until docker info >/dev/null 2>&1; do sleep 2; done
echo "[occitan] Docker ready."

echo "[occitan] waiting for kind node..."
until docker inspect occitan-control-plane --format='{{.State.Running}}' 2>/dev/null | grep -q true; do
    sleep 2
done
echo "[occitan] kind node running."

echo "[occitan] waiting for k8s API server..."
until kubectl --context kind-occitan get nodes >/dev/null 2>&1; do sleep 3; done

echo "[occitan] nodes:"
kubectl --context kind-occitan get nodes

echo "[occitan] waiting for critical pods to be ready..."
kubectl --context kind-occitan wait pod \
    -n occitan-system \
    -l app.kubernetes.io/component=synapse \
    --for=condition=Ready --timeout=120s 2>/dev/null && echo "[occitan] synapse ready" || echo "[occitan] synapse timeout (continuing)"

kubectl --context kind-occitan wait pod \
    -n occitan-system \
    -l app.kubernetes.io/component=charradissa \
    --for=condition=Ready --timeout=60s 2>/dev/null && echo "[occitan] charradissa ready" || echo "[occitan] charradissa timeout (continuing)"

kubectl --context kind-occitan wait pod \
    -n agents \
    -l app.kubernetes.io/name=guilhem \
    --for=condition=Ready --timeout=60s 2>/dev/null && echo "[occitan] guilhem ready" || echo "[occitan] guilhem timeout (continuing)"

echo "[occitan] reloading port-forward LaunchAgents..."
launchctl unload ~/Library/LaunchAgents/occitan.portforward.synapse.plist 2>/dev/null || true
launchctl unload ~/Library/LaunchAgents/occitan.portforward.element.plist 2>/dev/null || true
launchctl load ~/Library/LaunchAgents/occitan.portforward.synapse.plist
launchctl load ~/Library/LaunchAgents/occitan.portforward.element.plist
echo "[occitan] port-forwards active: synapse → localhost:8008, element → localhost:8080"

# Add host route for the kind docker bridge network (172.19.0.0/16) via the
# Colima VM IP, so LoadBalancer IPs are directly reachable from the Mac.
# Requires sudo — prints a hint if not available rather than failing.
COLIMA_IP=$(colima list --json 2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('address',''))" 2>/dev/null || echo "")
if [ -n "$COLIMA_IP" ]; then
    if sudo -n route -n add -net 172.19.0.0/16 "$COLIMA_IP" 2>/dev/null; then
        echo "[occitan] route 172.19.0.0/16 via $COLIMA_IP added"
    else
        echo "[occitan] hint: run 'sudo route -n add -net 172.19.0.0/16 $COLIMA_IP' to make LoadBalancer IPs directly reachable"
    fi
fi

echo ""
echo "[occitan] cluster up."
echo "  Element:  http://localhost:8080"
echo "  Synapse:  http://occitane.guilhem:8008"
