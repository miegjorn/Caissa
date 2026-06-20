#!/usr/bin/env bash
# bootstrap.sh — stand up the Occitan kind cluster from scratch.
#
# Run from the Caissa repo root:
#   bash scripts/bootstrap.sh
#
# Idempotent: safe to re-run. Skips steps already done.

set -euo pipefail

CLUSTER_NAME="occitan"
METALLB_VERSION="v0.14.9"
ARGOCD_CHART_VERSION="7.8.0"
CERT_MANAGER_VERSION="v1.16.3"
INGRESS_NGINX_VERSION="4.11.3"

# ─── Colour helpers ──────────────────────────────────────────────────────────
green()  { echo -e "\033[0;32m✓ $*\033[0m"; }
blue()   { echo -e "\033[0;34m→ $*\033[0m"; }
yellow() { echo -e "\033[0;33m! $*\033[0m"; }
die()    { echo -e "\033[0;31m✗ $*\033[0m" >&2; exit 1; }

# ─── Prerequisites ────────────────────────────────────────────────────────────
blue "Checking prerequisites..."
command -v docker  >/dev/null || die "Docker is required: https://docs.docker.com/get-docker/"
command -v brew    >/dev/null || die "Homebrew is required: https://brew.sh"

brew_install() {
  local cmd=$1 pkg=${2:-$1}
  command -v "$cmd" >/dev/null && green "$cmd already installed" && return
  blue "Installing $pkg..."
  brew install "$pkg"
}

brew_install kind
brew_install kubectl
brew_install helm
brew_install argocd
brew_install argo          # Argo Workflows CLI

green "All prerequisites satisfied"

# ─── kind cluster ────────────────────────────────────────────────────────────
if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
  green "kind cluster '${CLUSTER_NAME}' already exists"
else
  blue "Creating kind cluster '${CLUSTER_NAME}'..."
  kind create cluster --config deploy/kind/cluster.yaml
  green "Cluster created"
fi

kubectl config use-context "kind-${CLUSTER_NAME}"

# ─── MetalLB ─────────────────────────────────────────────────────────────────
blue "Installing MetalLB ${METALLB_VERSION}..."
kubectl apply -f "https://raw.githubusercontent.com/metallb/metallb/${METALLB_VERSION}/config/manifests/metallb-native.yaml"
kubectl wait -n metallb-system deployment/controller \
  --for=condition=Available --timeout=120s
kubectl apply -f deploy/kind/metallb-pool.yaml
green "MetalLB ready"

# ─── ingress-nginx ────────────────────────────────────────────────────────────
blue "Installing ingress-nginx..."
helm repo add ingress-nginx https://kubernetes.github.io/ingress-nginx --force-update
helm upgrade --install ingress-nginx ingress-nginx/ingress-nginx \
  --namespace ingress-nginx --create-namespace \
  --version "${INGRESS_NGINX_VERSION}" \
  --set controller.hostPort.enabled=true \
  --set controller.service.type=LoadBalancer \
  --wait
green "ingress-nginx ready"

# ─── cert-manager ────────────────────────────────────────────────────────────
blue "Installing cert-manager ${CERT_MANAGER_VERSION}..."
helm repo add jetstack https://charts.jetstack.io --force-update
helm upgrade --install cert-manager jetstack/cert-manager \
  --namespace cert-manager --create-namespace \
  --version "${CERT_MANAGER_VERSION}" \
  --set crds.enabled=true \
  --wait
green "cert-manager ready"

# ─── ArgoCD ──────────────────────────────────────────────────────────────────
blue "Installing ArgoCD..."
kubectl create namespace argocd --dry-run=client -o yaml | kubectl apply -f -
helm repo add argo https://argoproj.github.io/argo-helm --force-update
helm upgrade --install argocd argo/argo-cd \
  --namespace argocd \
  --version "${ARGOCD_CHART_VERSION}" \
  --set server.service.type=LoadBalancer \
  --wait
green "ArgoCD ready"

# ─── Argo Workflows ──────────────────────────────────────────────────────────
blue "Installing Argo Workflows..."
helm upgrade --install argo-workflows argo/argo-workflows \
  --namespace argo --create-namespace \
  --timeout 10m \
  --wait
green "Argo Workflows ready"

# ─── CoreDNS patch ───────────────────────────────────────────────────────────
blue "Patching CoreDNS for occitane.guilhem..."
kubectl patch configmap coredns -n kube-system \
  --patch "$(cat deploy/kind/coredns-patch.yaml)"
kubectl rollout restart deployment/coredns -n kube-system
kubectl rollout status deployment/coredns -n kube-system
green "CoreDNS patched — occitane.guilhem resolves within the cluster"

# ─── Host /etc/hosts ─────────────────────────────────────────────────────────
SYNAPSE_IP="172.18.0.200"
echo ""
yellow "Add this to /etc/hosts for host-side resolution (requires sudo):"
echo "  sudo sh -c 'echo \"${SYNAPSE_IP}  occitane.guilhem\" >> /etc/hosts'"
echo ""
echo "Or with dnsmasq:"
echo "  echo 'address=/occitane.guilhem/${SYNAPSE_IP}' >> /usr/local/etc/dnsmasq.conf"
echo ""

# ─── Helm dependency lock ────────────────────────────────────────────────────
blue "Locking Helm chart dependencies..."
helm repo add bitnami https://charts.bitnami.com/bitnami --force-update
helm dependency update deploy/charts/occitan
green "Chart.lock generated — commit deploy/charts/occitan/Chart.lock and deploy/charts/occitan/charts/"

# ─── ArgoCD: register GitHub repo ────────────────────────────────────────────
ARGOCD_PASSWORD=$(kubectl -n argocd get secret argocd-initial-admin-secret \
  -o jsonpath="{.data.password}" 2>/dev/null | base64 -d 2>/dev/null || echo "")

if [ -n "${ARGOCD_PASSWORD}" ]; then
  blue "Logging into ArgoCD CLI..."
  argocd login \
    "$(kubectl get svc argocd-server -n argocd -o jsonpath='{.status.loadBalancer.ingress[0].ip}')" \
    --username admin \
    --password "${ARGOCD_PASSWORD}" \
    --insecure 2>/dev/null || \
  argocd login localhost:8080 --username admin --password "${ARGOCD_PASSWORD}" --insecure 2>/dev/null || \
  yellow "ArgoCD CLI login failed — run manually after port-forward (see docs/install.md)"

  blue "Registering bitnami Helm repo with ArgoCD..."
  argocd repo add https://charts.bitnami.com/bitnami \
    --type helm --name bitnami 2>/dev/null && green "bitnami repo registered" || \
    yellow "bitnami repo registration skipped (may already exist)"

  blue "Registering GitHub repo with ArgoCD..."
  yellow "If Caissa is a private GitHub repo, provide credentials:"
  yellow "  argocd repo add https://github.com/occitan/Caissa.git --username <user> --password <token>"
  yellow "  or: argocd repo add git@github.com:occitan/Caissa.git --ssh-private-key-path ~/.ssh/id_ed25519"
fi

# ─── ghcr.io pull secret ─────────────────────────────────────────────────────
# Required for pods to pull private ghcr.io/occitan/* images.
# Create a GitHub PAT with read:packages scope and set GHCR_PAT before running.
if [ -n "${GHCR_PAT:-}" ]; then
  blue "Creating ghcr-creds imagePullSecret in occitan-system and agents namespaces..."
  for ns in occitan-system agents; do
    kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f -
    kubectl create secret docker-registry ghcr-creds \
      --namespace "$ns" \
      --docker-server=ghcr.io \
      --docker-username="${GITHUB_USER:-bedardpl}" \
      --docker-password="${GHCR_PAT}" \
      --dry-run=client -o yaml | kubectl apply -f -
  done
  green "ghcr-creds secret created"
else
  yellow "GHCR_PAT not set — skipping ghcr-creds secret."
  yellow "Create it manually before ArgoCD syncs:"
  yellow "  export GHCR_PAT=<token-with-read:packages>  GITHUB_USER=bedardpl"
  yellow "  kubectl create secret docker-registry ghcr-creds -n occitan-system \\"
  yellow "    --docker-server=ghcr.io --docker-username=\$GITHUB_USER --docker-password=\$GHCR_PAT"
  yellow "  kubectl create secret docker-registry ghcr-creds -n agents \\"
  yellow "    --docker-server=ghcr.io --docker-username=\$GITHUB_USER --docker-password=\$GHCR_PAT"
fi

# ─── ArgoCD: bootstrap project + root app ────────────────────────────────────
blue "Applying ArgoCD project and root app..."
kubectl apply -f deploy/argocd/project.yaml
kubectl apply -f deploy/argocd/root-app.yaml
green "Root app applied — ArgoCD will now sync occitan + guilhem from git"

# ─── ArgoCD admin password ───────────────────────────────────────────────────
echo ""
echo "ArgoCD initial admin password: ${ARGOCD_PASSWORD:-run: kubectl -n argocd get secret argocd-initial-admin-secret -o jsonpath='{.data.password}' | base64 -d}"
echo "ArgoCD UI:  https://localhost:8080  (after: kubectl port-forward svc/argocd-server -n argocd 8080:443)"
echo ""
green "Bootstrap complete. Occitan cluster '${CLUSTER_NAME}' is ready."
echo "Guilhem will be live once ArgoCD syncs the first CI-built image from ghcr.io/occitan/."
