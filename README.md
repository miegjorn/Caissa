# Caissa — Container Toolbox

**Caissa** (Occitan: *chest/box*) is the Docker-based container toolbox.

It contains a sandboxed Claude Code runtime, a PII anonymization proxy, and a sidecar reporter.

It is also the **deployment home** for the Occitan stack: the `caissa` CLI builds and spawns
agent images (`build` / `push` / `spawn` / `listen`), and `deploy/` holds the kind cluster
config, Helm charts (`deploy/charts/occitan`), ArgoCD apps, and Argo Workflows.

## Deploying the stack

See **[docs/install.md](docs/install.md)** for the full bare-machine → running-stack guide.

Current state (local kind): the `occitan` app deploys Gardian, Farga, Amassada, Synapse,
Element, an in-chart `postgres:15-alpine`, and **OpenBao** — all Healthy. The four Rust
service images are built locally and `kind load`ed (`imagePullPolicy: Never`);
postgres/synapse/element/openbao pull public images.

- **Secrets** live in OpenBao (file-storage backend, PVC-backed), which **Gardian** fronts.
  First run after a fresh deploy: `bash scripts/init-openbao.sh`. Seed or rotate a secret
  with `scripts/seed-secret.sh` — it reads the root token from the `openbao` k8s secret
  (e.g. `echo -n "$TOKEN" | scripts/seed-secret.sh occitan/gitlab --restart agents/guilhem`).
- **Guilhem is alive** — an always-on `caissa listen` pod (`agents` namespace) that runs a
  Claude chronicle on `POST /trigger/chronicle` (model: `claude-haiku-4-5-20251001` by
  default, overridable via `CHRONICLE_MODEL`) and posts it to Farga. Its image carries
  `git`/`gh`/`glab`, and an initContainer injects the GitHub/GitLab tokens from OpenBao so it
  can work against both forges.
- **Matrix is fully wired**: charradissa runs `1/1`, `@charradissa` and `@claude` users exist,
  and the appservice registration is in Synapse's `/data`. See install.md › "Matrix appservice"
  for rotation instructions.
