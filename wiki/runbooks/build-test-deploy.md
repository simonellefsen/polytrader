# Build, Test, Deploy Runbook

> **Namespace Safety (Non-Negotiable)**  
> This project is strictly scoped to the `polytrader` namespace.  
> The only exception is the one-time ngrok policy patch (documented in `k8s-ngrok-update-policy` and `deploy-public-ngrok.md`).  
> Never run commands that could affect other namespaces (e.g. `--all-namespaces`, broad deletes, etc.).

## Local Rust + Dioxus Dev

Prerequisites: Rust (stable), cargo, `cargo install dioxus-cli` (or dx), Docker (for k8s later), kubectl + docker-desktop context.

```bash
# One-time
cargo install dioxus-cli

# Dev server (hot reload where supported)
dx serve --platform web

# Or full build
cargo build --release
```

## Database (local for dev, optional)

For quick local iteration without k8s:
- Use `docker run` postgres, or sqlx with sqlite for very early (not recommended long-term).
- Migrations: `cargo sqlx migrate run` (once configured).

## Docker Build

```bash
docker build -f Dockerfile.polytrader -t polytrader:dev .
docker build -f Dockerfile.hermes -t hermes:dev .
```

## Deploy to docker-desktop (k8s)

See deploy/ scripts and k8s/base/.

Typical flow (once manifests exist):

```bash
# 1. Ensure namespace + cnpg operator is present in cluster
kubectl apply -f deploy/k8s/base/namespace.yaml

# 2. Apply cnpg cluster (2 replicas)
kubectl apply -k deploy/k8s/base/postgres/

# 3. Wait for postgres to be ready (primary + standby)
kubectl wait --for=condition=ready pod -l cnpg.io/cluster=polytrader-postgres -n polytrader --timeout=300s

# 4. Apply the apps
kubectl apply -k deploy/k8s/base/

# 5. Verify
kubectl get all -n polytrader
```

## Running Hermes Manually (for testing)

```bash
kubectl exec -n polytrader deploy/hermes -- /app/hermes --once --reflection-type=market-resolution
```

## Verification Checklist After Deploy

- [ ] Postgres primary accepts connections and schema is migrated.
- [ ] polytrader pod serves Dioxus UI (port-forward + browser).
- [ ] Hermes can connect to DB and LLM and write a test reflection.
- [ ] No real-trading code paths are reachable (feature flags / env = paper-only).
- [ ] Logs are structured and volume is reasonable.

## Rollback

- `kubectl rollout undo deploy/polytrader -n polytrader`
- For DB: rely on cnpg PITR / WAL if available; otherwise restore from latest backup job.

Update this runbook as actual scripts and manifests are added.
