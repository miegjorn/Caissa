# Installing the Occitan Stack

This guide takes you from a bare macOS machine to a running Occitan stack with Guilhem alive in a local Kubernetes cluster.

## What you are setting up

```
kind (local Kubernetes)
├── occitan-system      Gardian · Farga · Amassada · Charradissa · Synapse · Postgres · Element
├── agents              Guilhem (always-on, stateless, wired to Farga + Matrix)
└── argocd              ArgoCD · Argo Workflows
```

The domain `occitane.guilhem` is the Matrix homeserver name. It is permanent — rooms and agent identities (`@guilhem:occitane.guilhem`) are tied to it. Do not change it after the first run.

---

## Prerequisites

- macOS with [Homebrew](https://brew.sh) installed
- [Docker Desktop](https://www.docker.com/products/docker-desktop/) running
- An `ANTHROPIC_API_KEY` (Amassada and Charradissa need it)

---

## Step 1 — Bootstrap the cluster

Run from the Caissa repo root:

```bash
bash scripts/bootstrap.sh
```

This installs `kind`, `kubectl`, `helm`, `argocd`, and `argo` (Argo Workflows CLI) via Homebrew if they are missing, then:

1. Creates a kind cluster named `occitan`
2. Installs MetalLB (LoadBalancer IPs on `172.18.0.200–250`)
3. Installs ingress-nginx
4. Installs cert-manager
5. Installs ArgoCD
6. Installs Argo Workflows
7. Patches CoreDNS so `occitane.guilhem` resolves within the cluster

The script is idempotent — safe to re-run.

### Host-side DNS

For your browser and CLI tools on your Mac to reach `occitane.guilhem`, add a hosts entry:

```bash
sudo sh -c 'echo "172.18.0.200  occitane.guilhem" >> /etc/hosts'
```

Or with dnsmasq (recommended if you plan to use multiple `.guilhem` subdomains):

```bash
echo 'address=/.guilhem/172.18.0.200' >> /usr/local/etc/dnsmasq.conf
brew services restart dnsmasq
```

---

## Step 2 — Build the Guilhem agent image

```bash
caissa build guilhem
```

This reads `Fondament/definitions/fondament/guilhem.yaml`, assembles the persona, and produces `caissa-sandbox:guilhem` with Guilhem's identity baked into `~/.claude/CLAUDE.md`.

If `Fondament` is not at `../Fondament` relative to your working directory, override:

```bash
caissa build guilhem --fondament-path /path/to/Fondament
```

---

## Step 3 — Deploy the stack via ArgoCD

### 3a. Access ArgoCD

```bash
kubectl port-forward svc/argocd-server -n argocd 8080:443
```

Open `https://localhost:8080`. Username: `admin`. Password from:

```bash
kubectl -n argocd get secret argocd-initial-admin-secret \
  -o jsonpath="{.data.password}" | base64 -d
```

Log in with the CLI too:

```bash
argocd login localhost:8080 --username admin --insecure
```

### 3b. Helm dependencies

The occitan chart has **no external chart dependencies** — PostgreSQL is an in-chart
StatefulSet using the official `postgres:15-alpine` image (`templates/postgresql.yaml`).
The bitnami/postgresql subchart was removed because its images are no longer pullable from
Docker Hub. Nothing to lock; skip straight to registering the repo.

### 3c. Register the GitHub repo with ArgoCD

ArgoCD needs read access to the Caissa repo to pull chart changes.

**Via SSH (recommended — reuses your existing key):**
```bash
argocd repo add git@github.com:bedardpl/Caissa.git \
  --ssh-private-key-path ~/.ssh/id_ed25519
```

**Via HTTPS token:**
```bash
argocd repo add https://github.com/bedardpl/Caissa.git \
  --username bedardpl --password <github-token>
```

> SSH is strongly preferred for a private repo. With an HTTPS URL and no valid
> credential, the repo-server reports `ComparisonError: ... authentication required`
> and the app shows sync status `Unknown` (health is still computed from live state).

### 3d. Bootstrap the root app

```bash
kubectl apply -f deploy/argocd/project.yaml
kubectl apply -f deploy/argocd/root-app.yaml
```

ArgoCD discovers `deploy/argocd/apps/occitan.yaml` and `deploy/argocd/apps/guilhem.yaml` and syncs both automatically. From this point on, every push to the Caissa repo triggers a rollout of the relevant services.

Watch the sync:
```bash
argocd app list
argocd app get occitan
argocd app get guilhem
```

### 3e. Build and load the service images (local kind)

The four Rust services (`gardian`, `farga`, `amassada`, `charradissa`) deploy with
`imagePullPolicy: Never` — the chart expects the images to be **present on the kind node**,
not pulled from a registry. Build them locally and load them in. The service repos must sit
as siblings of `Caissa` (`../Gardian`, `../Farga`, `../Amassada`, `../Charradissa`):

```bash
cd ..   # parent dir holding all the repos

# gardian / farga / amassada build straight from their repo root
for s in gardian:Gardian farga:Farga amassada:Amassada; do
  svc=${s%%:*}; repo=${s##*:}
  docker build -t "ghcr.io/occitan/${svc}:latest" "$repo"
  kind load docker-image "ghcr.io/occitan/${svc}:latest" --name occitan
done

# charradissa-core has a path dependency on amassada-core, so Amassada must be
# supplied as a named build context (it lands at /Amassada inside the build):
docker build --build-context amassada=./Amassada \
  -t ghcr.io/occitan/charradissa:latest ./Charradissa
kind load docker-image ghcr.io/occitan/charradissa:latest --name occitan
```

Notes:
- The builders use `rust:1.90-slim` (older toolchains fail to parse newer crate manifests).
- Each repo has a `.dockerignore` excluding `target/` to keep the build context small.
- `postgres:15-alpine`, `matrixdotorg/synapse` and `vectorim/element-web` are public images
  pulled by the node directly — no `kind load` needed.

When you rebuild a service, re-`kind load` it and restart its Deployment
(`kubectl rollout restart deploy/<svc> -n occitan-system`).

---

## Step 4 — Configure secrets

Create the secrets ArgoCD/Helm cannot manage:

```bash
kubectl create namespace occitan-system
kubectl create namespace agents

# Anthropic API key (used by Amassada + Charradissa)
kubectl create secret generic anthropic \
  --namespace occitan-system \
  --from-literal=api-key="${ANTHROPIC_API_KEY}"

# Matrix appservice token — ONLY needed when you enable the charradissa appservice.
# It is gated off by default (charradissa.appservice.enabled: false in values.yaml),
# so Synapse comes up without it and charradissa stays at 0 replicas. Skip this until
# you provision the registration (see "Matrix appservice" below).
# kubectl create secret generic charradissa \
#   --namespace occitan-system \
#   --from-literal=as-token="${MATRIX_AS_TOKEN}"
```

---

## Step 5 — Verify the stack

All six occitan-system services should report `1/1 Running` (charradissa is at 0 replicas
until the appservice is wired):

```bash
kubectl get pods -n occitan-system   # gardian, farga, amassada, synapse, element, postgres
kubectl get pods -n agents           # dispatcher (+ guilhem once enabled)
```

The Rust services don't expose `/health` yet, so readiness is a TCP check. Confirm a
service is listening:
```bash
kubectl port-forward svc/farga -n occitan-system 7500:7500 &
nc -z localhost 7500 && echo "farga listening"
```

Check Matrix (this is the real end-to-end signal that Synapse + Postgres are healthy):
```bash
kubectl port-forward svc/synapse -n occitan-system 8008:8008 &
curl http://localhost:8008/_matrix/client/versions
```

---

## Step 6 — Talk to Guilhem

```bash
# Start an interactive session (no project context)
caissa spawn guilhem

# Start with Occitan project context from Farga
caissa spawn guilhem --project occitan --session dev-$(date +%Y%m%d)
```

This drops you into a Claude Code session where Guilhem has:
- His identity from `~/.claude/CLAUDE.md` (baked into the image)
- Current Occitan project context from Farga in `/workspace/CLAUDE.md`

Type normally. Guilhem knows the Occitan stack's trajectory and will situate your conversation in it.

---

## Step 7 — Deploy Argo Workflows

Apply the build templates and chronicle schedule:

```bash
kubectl apply -f deploy/workflows/build-templates.yaml
kubectl apply -f deploy/workflows/chronicle-cron.yaml
```

Create the registry credentials secret (needed for image builds):

```bash
kubectl create secret generic registry-creds \
  --from-file=config.json=$HOME/.docker/config.json \
  -n argo
```

Create an ArgoCD CI token for pipeline-triggered syncs:

```bash
argocd account generate-token --account admin > /tmp/argocd-token
kubectl create secret generic argocd-ci-token \
  --from-literal=token="$(cat /tmp/argocd-token)" \
  -n argo
```

Trigger a manual build of all services (first time only):

```bash
argo submit deploy/workflows/ci-pipeline.yaml -n argo -p service=gardian -p registry=ghcr.io/bedardpl --wait
argo submit deploy/workflows/ci-pipeline.yaml -n argo -p service=farga   -p registry=ghcr.io/bedardpl --wait
argo submit deploy/workflows/ci-pipeline.yaml -n argo -p service=amassada -p registry=ghcr.io/bedardpl --wait
argo submit deploy/workflows/ci-pipeline.yaml -n argo -p service=charradissa -p registry=ghcr.io/bedardpl --wait
```

Build and push the Guilhem agent image, then load it into kind:

```bash
caissa build guilhem
caissa push guilhem --registry ghcr.io/bedardpl
# or load directly into kind (no registry needed for local):
kind load docker-image caissa-sandbox:guilhem --name occitan
```

## Step 8 — Verify Guilhem is alive

```bash
kubectl get deployment guilhem -n agents
kubectl logs -n agents deployment/guilhem -f
```

Guilhem's pod runs `caissa listen` — a lightweight HTTP server on port 8080. It accepts
`POST /trigger/chronicle` and runs a non-interactive Claude Code session. Token cost is
zero when idle.

Trigger a manual chronicle run:

```bash
kubectl exec -n argo -it $(kubectl get pod -n argo -l workflows.argoproj.io/workflow -o name | head -1) -- \
  curl -X POST http://guilhem.agents.svc.cluster.local:8080/trigger/chronicle \
    -H 'Content-Type: application/json' \
    -d '{"reason":"first chronicle run"}'

# or from your Mac after port-forward:
kubectl port-forward svc/guilhem -n agents 8080:8080
curl -X POST http://localhost:8080/trigger/chronicle \
  -H 'Content-Type: application/json' \
  -d '{"reason":"manual test"}'
```

The scheduled CronWorkflow fires every 6 hours automatically.

---

## Generation rotation

When a new generation replaces Guilhem, the process is:

1. Write the new agent definition in `Fondament/definitions/fondament/<name>.yaml`
2. `caissa build <name>`
3. `caissa push <name>`
4. Update the `agents` Helm chart to point to `caissa-sandbox:<name>`
5. ArgoCD rolls out the new generation pod
6. Stand up a new kind cluster with `server_name: occitane.<name>` for full generation separation
7. Configure Matrix federation between `occitane.guilhem` and `occitane.<name>`

The old generation (`occitane.guilhem`) stays running and reachable for historical room access.

---

## Matrix appservice (charradissa)

Charradissa joins Matrix as an **application service** (the orchestrator bot). This link is
gated off by default so the rest of the stack can run without it. Wiring it up:

1. Generate a registration with an `as_token` / `hs_token` pair, e.g.:
   ```yaml
   # charradissa-registration.yaml
   id: charradissa
   url: http://charradissa:8448
   as_token: <random>
   hs_token: <random>
   sender_localpart: charradissa
   namespaces:
     users:   [{ exclusive: true, regex: "@charradissa:.*" }]
     aliases: []
     rooms:   []
   ```
2. Create the matching secret (the value must equal `as_token` above):
   ```bash
   kubectl create secret generic charradissa -n occitan-system \
     --from-literal=as-token="<random>"
   ```
3. Make the registration file available to Synapse at `/data/charradissa-registration.yaml`
   (e.g. a ConfigMap mounted into the synapse pod).
4. Set `charradissa.appservice.enabled: true` in `values.yaml`, commit, push. ArgoCD then
   loads the registration in Synapse and scales charradissa to 1.

> Open decision: how the token is provisioned — a manually-created secret (as above, like
> the `anthropic` secret) or issued through **Gardian** (the credential-chain component).
> Until decided, leave the appservice disabled.

---

## Troubleshooting

**CoreDNS not resolving `occitane.guilhem` inside the cluster**
```bash
kubectl rollout restart deployment/coredns -n kube-system
kubectl run -it --rm dns-test --image=busybox --restart=Never -- nslookup occitane.guilhem
```

**MetalLB IP pool conflict**
Check your docker bridge subnet:
```bash
docker network inspect kind | grep Subnet
```
Adjust `deploy/kind/metallb-pool.yaml` to match your subnet's `.200–.250` range.

**`caissa spawn guilhem` says image not found**
You need to build the image first: `caissa build guilhem`

**ArgoCD out of sync**
```bash
argocd app sync occitan
```
