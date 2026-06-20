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

There are three steps: **bootstrap k8s secrets** (4a, which OpenBao itself and the pods
that inject env need), **initialising OpenBao** (4b, one-shot after fresh deploy), and
**seeding application secrets** (4c, the real source of truth that Gardian fronts).

### 4a. Bootstrap k8s secrets

```bash
kubectl create namespace occitan-system
kubectl create namespace agents

# Anthropic API key — consumed directly via env by Amassada/Charradissa/Guilhem.
# (Also seeded into OpenBao below; this env copy is the bootstrap until everything
# resolves through Gardian.)
for ns in occitan-system agents; do
  kubectl create secret generic anthropic -n "$ns" \
    --from-literal=api-key="${ANTHROPIC_API_KEY}"
done
```

OpenBao deploys automatically as part of the occitan chart (`templates/openbao.yaml`,
file-storage backend, PVC-backed). On a fresh deploy it starts **sealed** — run the
one-shot init script after the stack is up (Step 4b).

### 4b. Initialise OpenBao (one-shot after fresh deploy)

`scripts/init-openbao.sh` reads the unseal key and root token from the OpenBao PVC,
patches the `openbao` k8s secret in both `occitan-system` and `agents` namespaces,
enables the KV v2 secrets engine, and restarts Gardian so it picks up the token:

```bash
bash scripts/init-openbao.sh
```

Run this once after every fresh cluster build. If OpenBao's pod restarts but the PVC
survives, it will auto-unseal on the next pod start and the existing k8s secret remains
valid — no need to re-run unless the PVC was wiped.

### 4c. Seed application secrets into OpenBao

`scripts/seed-secret.sh` reads the root token from the `openbao` k8s secret (no
hard-coded token needed). Pipe the secret value over stdin:

```bash
echo -n "$ANTHROPIC_API_KEY"  | scripts/seed-secret.sh occitan/anthropic
echo -n "$GITHUB_TOKEN"       | scripts/seed-secret.sh occitan/github
echo -n "$GITLAB_TOKEN"       | scripts/seed-secret.sh occitan/gitlab --restart agents/guilhem
```

Gardian fronts these: with `BAO_ADDR=http://openbao:8200` set (it is, in values.yaml),
`gardian-server` selects its `OpenBaoBackend` and resolves e.g. `occitan/anthropic` →
`secret/occitan/anthropic` field `value`. Verify:

```bash
GPOD=$(kubectl get pod -n occitan-system -l app.kubernetes.io/name=gardian -o name | head -1)
kubectl exec -n occitan-system "$GPOD" -- sh -c \
  'curl -s -H "X-Vault-Token: $BAO_TOKEN" http://openbao:8200/v1/secret/data/occitan/anthropic' \
  | python3 -c "import sys,json;print('len', len(json.load(sys.stdin)['data']['data']['value']))"
```

**Re-seeding after a cluster rebuild**: run `init-openbao.sh` first (to repopulate the
k8s secret), then `seed-secret.sh` for each secret.

---

## Step 5 — Verify the stack

All six occitan-system services should report `1/1 Running`:

```bash
kubectl get pods -n occitan-system   # gardian, farga, amassada, charradissa, synapse, element, postgres
kubectl get pods -n agents           # dispatcher (+ guilhem once enabled)
```

The four Rust services (gardian, farga, amassada, charradissa) expose `GET /health`;
readiness probes use `httpGet /health`. Confirm a service is up:
```bash
kubectl port-forward svc/farga -n occitan-system 7500:7500 &
curl -sf http://localhost:7500/health && echo "farga healthy"
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
zero when idle. Chronicle runs use `claude-haiku-4-5-20251001` by default; override with
the `chronicle_model` key in `caissa.toml` or the `CHRONICLE_MODEL` env var.

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

The listener returns `202` immediately and runs Claude in the background; watch
`kubectl logs -n agents deployment/guilhem` for `chronicle run complete`. The output is
posted to Farga as a signal — confirm with
`curl http://farga.occitan-system.svc.cluster.local:7500/signals/recent?project=occitan`.
The scheduled CronWorkflow fires every 6 hours automatically.

> The chronicle prompt asks Claude to read/write Farga directly, but in a headless
> `claude --print` run its shell tools are gated. The reliable path today is that `caissa`
> posts Claude's output to Farga for it. Attaching the Farga MCP (`farga_mcp_url`) to the
> chronicle run is the planned fix to give Claude a grounded read of the stack.

### Guilhem's GitHub / GitLab access

The agent image ships `git`, `gh`, and `glab`. The Guilhem pod's initContainer pulls the
`occitan/github` and `occitan/gitlab` tokens from OpenBao into an in-memory `/creds` volume
(`tokens.env` + `.git-credentials` + `.gitconfig`); the listener sources them so Guilhem and
its `claude` subprocess can clone/push and use the CLIs. Verify:

```bash
GP=$(kubectl get pod -n agents -l app.kubernetes.io/name=guilhem -o name | head -1)
kubectl exec -n agents "$GP" -- sh -lc '. /creds/tokens.env; gh api user --jq .login'
kubectl exec -n agents "$GP" -- sh -lc '. /creds/tokens.env; glab api user | python3 -c "import sys,json;print(json.load(sys.stdin)[\"username\"])"'
```

> GitLab needs a token with `read_repository`/`read_user` (a classic PAT, or a fine-grained
> one with those scopes) — a narrowly-scoped token returns `403 insufficient_granular_scope`.
> To rotate a token: `echo -n "$NEW" | scripts/seed-secret.sh occitan/gitlab --restart agents/guilhem`.

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

Charradissa joins Matrix as an **application service** (the orchestrator bot). The
appservice is fully wired: charradissa runs at `1/1`, the `@charradissa` and `@claude`
users exist in Matrix, and the appservice registration lives in Synapse's `/data`
directory.

The registration YAML format (for reference or re-provisioning):

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

The `as_token` value must match the `charradissa` k8s secret (`as-token` key) in
`occitan-system`. To rotate it:

1. Update the k8s secret: `kubectl create secret generic charradissa -n occitan-system --from-literal=as-token="<new>" --dry-run=client -o yaml | kubectl apply -f -`
2. Update the registration file in Synapse's `/data` and restart Synapse.
3. Restart charradissa: `kubectl rollout restart deploy/charradissa -n occitan-system`

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
