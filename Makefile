# polytrader Makefile — developer & POC deployment commands
#
# NAMESPACE SAFETY (enforced):
#   This project ONLY operates inside the 'polytrader' namespace.
#   The single exception is the explicit one-time ngrok policy update
#   (target: k8s-ngrok-update-policy), which touches the shared tunnel
#   configuration in the saxo-rust namespace.
#
#   Never use kubectl with --all-namespaces or broad prune flags.
#   Always prefer the make targets below.
#
# Focus: docker-desktop + shared ngrok tunnel (https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader)

NAMESPACE := polytrader
K8S_BASE  := deploy/k8s/base

# Strict namespace guard for polytrader project.
# We ONLY ever touch the $(NAMESPACE) namespace, except for the explicit
# one-time ngrok policy update in the shared tunnel (saxo-rust ns).
.PHONY: help build check test run dev docker-build \
        k8s-check-namespace k8s-apply k8s-deploy k8s-status \
        k8s-logs k8s-port-forward k8s-ngrok-reminder k8s-ngrok-update-policy \
        k8s-delete clean

# Internal guard: ensures we are operating only on the polytrader namespace
k8s-check-namespace:
	@CURRENT_NS=$$(kubectl config view --minify --output 'jsonpath={..namespace}' 2>/dev/null || echo ""); \
	if [ "$$CURRENT_NS" != "$(NAMESPACE)" ] && [ -n "$$CURRENT_NS" ]; then \
		echo "ERROR: Current namespace is '$$CURRENT_NS', expected '$(NAMESPACE)'"; \
		echo "Run: kubectl config set-context --current --namespace=$(NAMESPACE)"; \
		exit 1; \
	fi
	@echo "✓ Operating in namespace: $(NAMESPACE)"

help:
	@echo "polytrader — Makefile targets"
	@echo ""
	@echo "Development:"
	@echo "  make build              - cargo build --release"
	@echo "  make check              - cargo check + clippy"
	@echo "  make test               - cargo test"
	@echo "  make run                - run locally (needs DATABASE_URL)"
	@echo "  make dev                - alias for run"
	@echo ""
	@echo "Docker:"
	@echo "  make docker-build       - build polytrader:local (and hermes:local)"
	@echo ""
	@echo "Kubernetes (docker-desktop, namespace polytrader):"
	@echo "  make k8s-apply          - build images + kubectl apply -k (recommended)"
	@echo "  make k8s-deploy         - legacy alias for k8s-apply"
	@echo "  make k8s-status         - show pods, services, agentendpoints, etc."
	@echo "  make k8s-logs           - tail polytrader app logs"
	@echo "  make k8s-port-forward   - forward localhost:8080 -> service"
	@echo "  make k8s-ngrok-reminder - show the one-time shared tunnel policy patch instructions"
	@echo "  make k8s-delete         - delete everything in the base"
	@echo ""
	@echo "  After successful deploy + one-time policy patch by tunnel owner:"
	@echo "    Public POC URL: https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader"
	@echo ""

build:
	cargo build --release

check:
	cargo check
	cargo clippy -- -D warnings || true

test:
	cargo test

run:
	POLYTRADER_MODE=paper cargo run

dev: run

docker-build:
	docker build -t polytrader:local -f Dockerfile .
	docker build -t hermes:local -f Dockerfile.hermes .
	@echo "Images built: polytrader:local, hermes:local (hermes ts tag + set-image happens in k8s-apply for robustness)"

# Main deployment target for the POC
# This target ONLY touches the $(NAMESPACE) namespace.
k8s-apply: docker-build k8s-check-namespace
	@echo "==> Switching to docker-desktop context..."
	kubectl config use-context docker-desktop || true
	kubectl config set-context --current --namespace=$(NAMESPACE) || true
	@echo "==> Applying kustomize base (scoped to $(NAMESPACE) only)..."
	kubectl apply -k $(K8S_BASE) --namespace=$(NAMESPACE)
	@echo "==> Forcing fresh hermes image (timestamp tag + set-image inside apply; no sentinel file = no race/TOCTOU/leak; addresses stale :local digest on docker-desktop)"
	@HERMES_TS_TAG="hermes:local-$$(date +%s)"; docker tag hermes:local $$HERMES_TS_TAG && { echo "  tagged $$HERMES_TS_TAG"; kubectl set image -n $(NAMESPACE) deployment/hermes hermes=$$HERMES_TS_TAG || true; kubectl rollout status deploy/hermes -n $(NAMESPACE) --timeout=120s || true; }
	@echo "==> Waiting for CloudNativePG cluster (can take 1-3 min)..."
	kubectl wait --for=condition=ready pod -l cnpg.io/cluster=polytrader-postgres -n $(NAMESPACE) --timeout=300s || true
	@echo ""
	@echo "==> Deployment finished. Current state:"
	@make k8s-status
	@echo ""
	@echo "==> Next critical step for public access:"
	@echo "    make k8s-ngrok-reminder"
	@echo ""

k8s-deploy: k8s-apply

k8s-status: k8s-check-namespace
	@echo "=== $(NAMESPACE) namespace ==="
	kubectl get all,agentendpoints,ngroktrafficpolicies -n $(NAMESPACE) 2>/dev/null || true
	@echo ""
	@echo "=== Postgres cluster status ==="
	kubectl get cluster -n $(NAMESPACE) 2>/dev/null || true

k8s-logs: k8s-check-namespace
	kubectl logs -n $(NAMESPACE) -l app=polytrader --tail=200 -f

k8s-port-forward: k8s-check-namespace
	@echo "Forwarding http://localhost:8080 -> polytrader service (namespace $(NAMESPACE))"
	kubectl port-forward -n $(NAMESPACE) svc/polytrader 8080:80

# Reminds the user about the one-time manual step required for the shared ngrok tunnel
k8s-ngrok-reminder:
	@echo "==================================================================="
	@echo "  NGROK SHARED TUNNEL - ONE-TIME MANUAL STEP (required for public URL)"
	@echo "==================================================================="
	@echo ""
	@echo "The AgentEndpoint 'polytrader-internal' has been created in the cluster."
	@echo "For https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader to work,"
	@echo "the owner of the shared tunnel must add a rule to the central policy."
	@echo ""
	@echo "Exact instructions + recommended stanza (with url-rewrite for clean links):"
	@echo "  cat deploy/k8s/base/ngrok/polytrader-agentendpoint.yaml | grep -A 30 'RECOMMENDED'"
	@echo ""
	@echo "Or read the full runbook:"
	@echo "  cat wiki/runbooks/deploy-public-ngrok.md"
	@echo ""
	@echo "After the patch + Google SSO login, the POC should be live at:"
	@echo "  https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader"
	@echo "==================================================================="

# DEDICATED TARGET for the one-time ngrok policy update.
# This is the ONLY target allowed to touch any namespace other than $(NAMESPACE).
# It touches the shared tunnel's NgrokTrafficPolicy in the saxo-rust namespace.
k8s-ngrok-update-policy:
	@echo "!!! WARNING: This command will modify resources in the 'saxo-rust' namespace !!!"
	@echo "This is the one-time manual step required to route /polytrader on the shared tunnel."
	@read -p "Are you sure you want to continue? [y/N] " confirm; \
	if [ "$$confirm" != "y" ] && [ "$$confirm" != "Y" ]; then \
		echo "Aborted."; exit 1; \
	fi
	@echo "Patching NgrokTrafficPolicy in saxo-rust namespace (adding /polytrader rule)..."
	kubectl patch ngroktrafficpolicy daytrader-oauth -n saxo-rust --type='json' -p='[{"op":"add","path":"/spec/policy/on_http_request/-","value":{"actions":[{"config":{"from":"/polytrader/?(.*)","to":"/$1"},"type":"url-rewrite"},{"config":{"url":"http://polytrader.internal:80"},"type":"forward-internal"}],"expressions":["req.url.path.startsWith(\"/polytrader\")"]}}]' || echo "Patch may have partially applied or already exists."
	@echo "Done. Verify with: kubectl get ngroktrafficpolicy -n saxo-rust daytrader-oauth -o yaml | grep -A 20 polytrader"

k8s-delete: k8s-check-namespace
	kubectl delete -k $(K8S_BASE) --namespace=$(NAMESPACE) --ignore-not-found=true
	@echo "Resources in $(K8S_BASE) deleted from namespace $(NAMESPACE)."

clean:
	cargo clean
	@echo "Cargo clean done."
