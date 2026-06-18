# polytrader Makefile — developer & POC deployment commands
#
# GUARDRAILS (added to prevent repeated CrashLoopBackOff deploys):
#   `make k8s-apply` now depends on `pre-deploy-check`, which runs:
#     - cargo fmt -- --check
#     - cargo check
#     - cargo clippy --all-targets -- -D warnings
#     - cargo test
#   These must all pass cleanly before any image is built or deployed.
#   This catches compilation errors, lints, test failures, and (importantly)
#   the very brittle Dioxus rsx! string parsing issues early.
#   Run `make pre-deploy-check` locally any time before you are ready to deploy.
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
        k8s-logs k8s-port-forward k8s-verify k8s-ngrok-reminder k8s-ngrok-update-policy \
        k8s-delete clean wasm-prep

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
	@echo "  make check              - strict: fmt + check + clippy -D + test (same as pre-deploy)"
	@echo "  make pre-deploy-check   - the strict guardrails (run this before any deploy)"
	@echo "  make test               - cargo test"
	@echo "  make run                - run locally (needs DATABASE_URL)"
	@echo "  make dev                - alias for run"
	@echo ""
	@echo "Docker:"
	@echo "  make docker-build       - build polytrader:local (and hermes:local)"
	@echo ""
	@echo "Kubernetes (docker-desktop, namespace polytrader):"
	@echo "  make k8s-apply          - STRICT guardrails + build + apply (recommended)"
	@echo "                            (will refuse to deploy if fmt/check/clippy/test fail)"
	@echo "  make k8s-deploy         - legacy alias for k8s-apply"
	@echo "  make pre-deploy-check   - run the same strict guardrails locally (no deploy)"
	@echo "  make k8s-status         - show pods, services, agentendpoints, etc."
	@echo "  make k8s-logs           - tail polytrader app logs"
	@echo "  make k8s-port-forward   - forward localhost:8080 -> service"
	@echo "  make k8s-verify         - run post-deploy checks, including dashboard JS syntax"
	@echo "  make k8s-ngrok-reminder - show the one-time shared tunnel policy patch instructions"
	@echo "  make k8s-delete         - delete everything in the base"
	@echo "  make wasm-prep          - next phase WASM prep scaffolding (echo-only/no-op today; guarded in Dockerfile; see wiki/log.md top for gaps + definition)"
	@echo ""
	@echo "  After successful deploy + one-time policy patch by tunnel owner:"
	@echo "    Public POC URL: https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader"
	@echo ""

build:
	cargo build --release

check:
	cargo fmt --all -- --check
	cargo check
	cargo clippy --all-targets -- -D warnings
	# Mirror pre-deploy: threads=1 for clob env tests + native-l2 for real gated path coverage.
	# Native targeted lines are now blocking (remove || true) to make real gated coverage (FILE, signing, place bails) fatal for guardrails.
	cargo test -- --test-threads=1
	cargo check --features native-l2
	cargo test --features native-l2 -- clob::authenticated::tests::place_limit -- --test-threads=1
	cargo test --features native-l2 -- clob::live_sender::tests::gated_real -- --test-threads=1

# Strict guardrails that MUST pass before any deployment.
# This prevents wasting time on CrashLoopBackOff deploys due to
# compilation, lint, test, or formatting issues (especially fragile
# Dioxus rsx! strings and embedded JS/CSS).
pre-deploy-check:
	@echo "==> Running strict pre-deploy guardrails (these MUST pass before deploy)..."
	cargo fmt --all -- --check
	cargo check
	# Env-mutating clob tests (place_bails_*, unlock, gated sender, submit facade) are serialized via TEST_ENV_LOCK.
	# Use --test-threads=1 for the clob filter to prevent races on POLYTRADER_ENABLE_* globals (see authenticated.rs:2248 comment).
	cargo test -- --test-threads=1
	# Exercise native-l2 real gated path (FILE key lookup, signing bails, place_limit under feature, from_current for gated dispatch) in guardrails.
	# This covers the L2 secret volume + signing exercised in the TS pod (see Issue 5 fix round).
	# NOTE: these native lines are now *blocking* (no || true) so real gated coverage is fatal before TS+set-image.
	cargo check --features native-l2
	cargo test --features native-l2 -- clob::authenticated::tests::place_limit -- --test-threads=1
	cargo test --features native-l2 -- clob::live_sender::tests::gated_real -- --test-threads=1
	@echo ""
	@echo "==> Running clippy for visibility (does not block deploy yet)..."
	cargo clippy --all-targets -- -D warnings || echo "    (Clippy warnings exist — consider cleaning, but not blocking deploy for now)"
	@echo ""
	@echo "==> Running native-l2 coverage for real gated path (FILE, signing bails, place under feature, from_current) — now blocking per review hygiene."
	cargo check --features native-l2
	cargo test --features native-l2 -- clob::authenticated::tests::place_limit -- --test-threads=1
	cargo test --features native-l2 -- clob::live_sender::tests::gated_real -- --test-threads=1
	@echo ""
	@echo "==> ✅ Guardrails passed. fmt + check + tests + native-l2 real gated coverage are clean. Safe to build/deploy."
	@echo "    (Clippy is advisory for now due to pre-existing L2/paper engine noise.)"

test:
	cargo test -- --test-threads=1

run:
	POLYTRADER_MODE=paper cargo run

dev: run

docker-build:
	docker build -t polytrader:local -f Dockerfile .
	docker build -t hermes:local -f Dockerfile.hermes .
	@echo "Images built: polytrader:local, hermes:local (hermes ts tag + set-image happens in k8s-apply for robustness)"

# Main deployment target for the POC
# This target ONLY touches the $(NAMESPACE) namespace.
k8s-apply: pre-deploy-check docker-build k8s-check-namespace
	@echo "==> Switching to docker-desktop context..."
	kubectl config use-context docker-desktop || true
	kubectl config set-context --current --namespace=$(NAMESPACE) || true
	@echo "==> Applying kustomize base (scoped to $(NAMESPACE) only)..."
	kubectl apply -k $(K8S_BASE) --namespace=$(NAMESPACE)

	# Unique timestamp tags + explicit set-image for BOTH polytrader and hermes.
	# This is the key pattern from rust_daytrader that defeats Docker Desktop :local caching.
	@POLY_TS="polytrader:local-$$(date +%s)"; \
	HERMES_TS="hermes:local-$$(date +%s)"; \
	docker tag polytrader:local $$POLY_TS; \
	docker tag hermes:local $$HERMES_TS; \
	echo "  tagged $$POLY_TS and $$HERMES_TS"; \
	kubectl set image -n $(NAMESPACE) deployment/polytrader polytrader=$$POLY_TS; \
	kubectl set image -n $(NAMESPACE) deployment/hermes hermes=$$HERMES_TS; \
	kubectl rollout status deploy/polytrader -n $(NAMESPACE) --timeout=180s; \
	kubectl rollout status deploy/hermes   -n $(NAMESPACE) --timeout=120s

	@echo "==> Waiting for CloudNativePG cluster (can take 1-3 min)..."
	kubectl wait --for=condition=ready pod -l cnpg.io/cluster=polytrader-postgres -n $(NAMESPACE) --timeout=300s || true
	@echo ""
	@echo "==> Deployment finished. Current state:"
	@make k8s-status
	@echo ""

	# === L2 Private Key Secret (interactive) ===
	@echo ""
	@read -p "==> Populate/update POLYMARKET_PRIVATE_KEY from .env.local into the cluster now? [y/N] " ans; \
	if [ "$$ans" = "y" ] || [ "$$ans" = "Y" ]; then \
		$(MAKE) k8s-set-l2-key; \
	else \
		echo "   Skipped. Run 'make k8s-set-l2-key' later when you have the key in .env.local."; \
	fi

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

k8s-verify: k8s-check-namespace
	./deploy/verify

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
	@echo "Exact instructions + recommended stanza (raw-prefix forward; app serves /polytrader too):"
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
	@if kubectl get ngroktrafficpolicy daytrader-oauth -n saxo-rust -o yaml | grep -q 'url: http://polytrader.internal:80'; then \
		echo "/polytrader forward rule already present; no patch needed."; \
	else \
		kubectl patch ngroktrafficpolicy daytrader-oauth -n saxo-rust --type='json' -p='[{"op":"add","path":"/spec/policy/on_http_request/-","value":{"actions":[{"config":{"url":"http://polytrader.internal:80"},"type":"forward-internal"}],"expressions":["req.url.path.startsWith(\"/polytrader\")"]}}]'; \
	fi
	@echo "Done. Verify with: kubectl get ngroktrafficpolicy -n saxo-rust daytrader-oauth -o yaml | grep -A 6 polytrader.internal"

k8s-delete: k8s-check-namespace
	kubectl delete -k $(K8S_BASE) --namespace=$(NAMESPACE) --ignore-not-found=true
	@echo "Resources in $(K8S_BASE) deleted from namespace $(NAMESPACE)."

# Safely inject POLYMARKET_PRIVATE_KEY from .env.local into the cluster.
# Never prints the secret value.
k8s-set-l2-key: k8s-check-namespace
	@KEY=$$(grep -E '^POLYMARKET_PRIVATE_KEY=' .env.local 2>/dev/null | cut -d'=' -f2- | tr -d '\r' | tr -d '"' | tr -d "'" || true); \
	if [ -z "$$KEY" ]; then \
		echo "ERROR: POLYMARKET_PRIVATE_KEY not found or empty in .env.local"; \
		exit 1; \
	fi; \
	kubectl create secret generic polytrader-l2-auth \
		--from-literal=private-key="$$KEY" \
		--dry-run=client -o yaml | kubectl apply -f - -n $(NAMESPACE) >/dev/null; \
	echo "✓ polytrader-l2-auth secret updated from .env.local (value never printed)"; \
	kubectl rollout restart deployment/polytrader -n $(NAMESPACE) || true; \
	echo "✓ polytrader deployment restarted to pick up the new key"

# Create/rotate the CNPG backup object-store secret (polytrader-minio-backup) from the running
# shared RustFS container. Kept OUT of the kustomize base on purpose: a CHANGE_ME placeholder in the
# applied manifest would be re-applied by every `make k8s-deploy` and silently reset the live creds,
# breaking WAL archiving + retention with InvalidAccessKeyId (regression seen 2026-06-18). Values are
# read from the container env and never printed. CNPG reloads the secret + retries archiving on its
# own (no DB restart needed). Run once after first deploy and any time RustFS creds rotate.
k8s-set-backup-creds: k8s-check-namespace
	@AK=$$(docker inspect daytrader_rustfs --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null | sed -n 's/^RUSTFS_ACCESS_KEY=//p'); \
	SK=$$(docker inspect daytrader_rustfs --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null | sed -n 's/^RUSTFS_SECRET_KEY=//p'); \
	if [ -z "$$AK" ] || [ -z "$$SK" ]; then \
		echo "ERROR: could not read RUSTFS_ACCESS_KEY/SECRET from the daytrader_rustfs container (is it running?)"; \
		exit 1; \
	fi; \
	kubectl create secret generic polytrader-minio-backup \
		--from-literal=ACCESS_KEY_ID="$$AK" \
		--from-literal=ACCESS_SECRET_KEY="$$SK" \
		--dry-run=client -o yaml | kubectl apply -f - -n $(NAMESPACE) >/dev/null; \
	echo "✓ polytrader-minio-backup secret set from daytrader_rustfs (values never printed)"; \
	echo "  CNPG will reload + resume WAL archiving within ~1-2 min; verify with:"; \
	echo "  kubectl get cluster polytrader-postgres -n $(NAMESPACE) -o jsonpath='{.status.conditions[?(@.type==\"ContinuousArchiving\")].status}'"

clean:
	cargo clean
	@echo "Cargo clean done."

# post-Phase 2 smallest start (post-wiki/log 2026-05-25 deploy entry + fidelity amend; WASM hydration prep per gaps: full client + assets + server_fns; see project-plan Phase 3 for gated real).
# Non-breaking: no impact to docker-build, k8s-apply, hermes ts, existing targets, or any verified Phase 0/1/2 behavior (paper-only, subpath, probes, etc).
wasm-prep:
	@echo "WASM hydration prep (next phase after Phase 2 deploy+docs; see wiki/log.md top entry for gaps + definition)."
	@echo "  - rustup target add wasm32-unknown-unknown (guarded in Dockerfile)"
	@echo "  - dx build / cargo wasm32 + asset copy + axum static serve (future; server_fns for live rsx data)"
	@echo "  - Keep k8s-apply/hermes ts/poly flow 100% intact (no changes to docker-build etc)."
	@echo "Other gaps (resolution-triggered reflections, deeper autonomous local apply, expanded tests w/ wiremock/DB mocks/k8s e2e) defined in wiki for follow-ups."
