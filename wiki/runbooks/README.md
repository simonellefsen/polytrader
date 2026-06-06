# Runbooks

Actionable, tested procedures.

## Index

- [build-test-deploy.md](build-test-deploy.md) — Local dev build, test, docker-desktop deploy cycle.
- [k8s-diagnostics.md](k8s-diagnostics.md) — Common debugging commands for the polytrader namespace.
- [deploy-public-ngrok.md](deploy-public-ngrok.md) — End-to-end steps to expose polytrader on the shared ngrok tunnel at /polytrader (after the one-time traffic policy patch).
- [l2-private-key-secrets.md](l2-private-key-secrets.md) — L2 secret management + (extended 2026-06-03) operator approval workflow for gated real CLOB (create human+final approvals with snapshots via UI/curls, feed UUIDs to submit, exercise under unlocks/kill; no raw journal).
- (Add more: incident-response, wallet-key-rotation, data-backfill, hermes-invocation, approval-runbook, etc.)

When creating a new runbook, include:
- Prerequisites
- Step-by-step (copy-pastable where safe)
- Verification commands
- Rollback
- When to page a human
