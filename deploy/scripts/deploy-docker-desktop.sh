#!/usr/bin/env bash
# Deploy polytrader stack to docker-desktop k8s.
#
# SAFETY RULE: This script ONLY operates on the 'polytrader' namespace.
# The only exception is the one-time ngrok policy update, which is explicitly
# documented and requires manual confirmation.
set -euo pipefail

NAMESPACE=polytrader
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
K8S_BASE="${SCRIPT_DIR}/../k8s/base"

echo "==> Enforcing strict namespace: ${NAMESPACE}"
kubectl config use-context docker-desktop || true
kubectl config set-context --current --namespace="${NAMESPACE}" || true

# Verify we are not accidentally in another namespace
CURRENT_NS=$(kubectl config view --minify -o jsonpath='{..namespace}' 2>/dev/null || echo "")
if [ "$CURRENT_NS" != "$NAMESPACE" ]; then
  echo "ERROR: Failed to switch to namespace ${NAMESPACE}. Current: ${CURRENT_NS}"
  exit 1
fi

echo "==> Applying polytrader k8s base (scoped to ${NAMESPACE} only)..."
kubectl apply -k "${K8S_BASE}" --namespace="${NAMESPACE}"

echo "==> Waiting for postgres cluster (this can take 1-3 minutes)..."
kubectl wait --for=condition=ready pod -l cnpg.io/cluster=polytrader-postgres -n "${NAMESPACE}" --timeout=300s || {
  echo "Postgres not ready yet — check logs with: kubectl logs -n ${NAMESPACE} -l cnpg.io/cluster=polytrader-postgres"
}

echo "==> Current state:"
kubectl get all,agentendpoints -n "${NAMESPACE}"

echo ""
echo "Next steps:"
echo "  make k8s-ngrok-reminder     # Shows exact instructions for the shared tunnel"
echo "  make k8s-port-forward       # Local access"
echo "  See wiki/runbooks/k8s-diagnostics.md and wiki/runbooks/deploy-public-ngrok.md"
echo ""
echo "  IMPORTANT: Never run 'kubectl apply' or 'kubectl delete' with --all or across namespaces."
echo "             Always use 'make k8s-*' targets or 'kubectl apply -k deploy/k8s/base'."
