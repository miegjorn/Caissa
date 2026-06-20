# Caissa — Container Toolbox

**Caissa** (Occitan: *chest/box*) is the Docker-based container toolbox.

It contains a sandboxed Claude Code runtime, a PII anonymization proxy, and a sidecar reporter.

It is also the **deployment home** for the Occitan stack: the `caissa` CLI builds and spawns
agent images (`build` / `push` / `spawn` / `listen`), and `deploy/` holds the kind cluster
config, Helm charts (`deploy/charts/occitan`), ArgoCD apps, and Argo Workflows.

## Deploying the stack

See **[docs/install.md](docs/install.md)** for the full bare-machine → running-stack guide.

Current state (local kind): the `occitan` app deploys Gardian, Farga, Amassada, Synapse,
Element and an in-chart `postgres:15-alpine` — all Healthy. The four Rust service images are
built locally and `kind load`ed (`imagePullPolicy: Never`); postgres/synapse/element pull
public images. The Matrix appservice (charradissa ↔ synapse) is gated off until its
registration token is provisioned — see install.md › "Matrix appservice".
