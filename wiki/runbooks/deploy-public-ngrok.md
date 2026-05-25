# Deploy Public ngrok Subpath Exposure (polytrader)

> **STRICT NAMESPACE SAFETY RULE**  
> All polytrader-related `kubectl` operations **must** be limited to the `polytrader` namespace.  
> The **only** allowed exception is the explicit one-time update of the shared tunnel's `NgrokTrafficPolicy` (in the `saxo-rust` namespace).  
> Never use `--all-namespaces`, `kubectl ... --all`, or broad prune commands.  
> Always prefer `make k8s-*` targets (see root `Makefile`).

Actionable runbook for making the polytrader dashboard reachable at the public shared tunnel URL `https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader` (and subpaths) using the established AgentEndpoint + NgrokTrafficPolicy pattern.

See also: `deploy/k8s/base/ngrok/polytrader-agentendpoint.yaml` (the authoritative source for the exact patch stanza and discovery history) and `wiki/log.md`.

## Prerequisites
- docker-desktop kubectl context active and connected to the cluster that has:
  - ngrok-operator installed and the shared public tunnel + `daytrader-oauth` NgrokTrafficPolicy in `saxo-rust` namespace (with Google SSO + email allowlist).
  - cnpg operator (for the polytrader postgres dependency).
- polytrader Deployment/Service + the internal `polytrader-internal` AgentEndpoint will be created by the base kustomize (in `polytrader` namespace).
- Your email must be in the allowlist for the Google OAuth rule (coordinate with saxo-rust tunnel owner if first time).
- Local: docker, Rust toolchain (for build), make or the deploy script.
- The one-time traffic policy patch can *only* be performed by the tunnel owner (cross-namespace write); this runbook documents the exact stanza to give them.

## Step-by-step

1. (Optional but recommended) Build the image:
   ```bash
   docker build -t polytrader:local -f Dockerfile .
   # hermes if also deploying: docker build -t hermes:local -f Dockerfile.hermes .
   ```
   (docker-desktop shares the local images.)

2. Deploy the full base (includes namespace, postgres cluster, polytrader Deployment+Service, hermes, and the ngrok AgentEndpoint):
   ```bash
   ./deploy/scripts/deploy-docker-desktop.sh
   # or equivalently:
   # kubectl apply -k deploy/k8s/base
   ```

3. Wait for core components (the script does most of this):
   ```bash
   kubectl wait --for=condition=ready pod -l cnpg.io/cluster=polytrader-postgres -n polytrader --timeout=300s
   kubectl rollout status deploy/polytrader -n polytrader --timeout=120s
   ```

4. Verify the internal AgentEndpoint is created and ready (this is the polytrader half of the wiring):
   ```bash
   kubectl get agentendpoints -n polytrader polytrader-internal -o wide
   # Expected: READY=True, URL=http://polytrader.internal:80, UPSTREAM=http://polytrader.polytrader:80 (the Service), BINDINGS=["internal"]
   kubectl describe agentendpoints -n polytrader polytrader-internal
   ```

5. **ONE-TIME manual step** — give this exact instruction + stanza to the saxo-rust tunnel owner (or perform if you are the owner):
   ```bash
   kubectl edit ngroktrafficpolicy -n saxo-rust daytrader-oauth
   ```
   Insert the following as a **new list item** in the `spec.policy.on_http_request` array (place it with the other forward rules; order among prefix rules does not matter because expressions are specific). Use the exact 2-space indentation:

   ```yaml
   - actions:
     - config:
         from: /polytrader/?(.*)
         to: /$1
       type: url-rewrite
     - config:
         url: http://polytrader.internal:80
       type: forward-internal
     expressions:
     - req.url.path.startsWith("/polytrader")
   ```

   This is the *recommended usable version* (includes rewrite to strip the prefix, modeled exactly on the live `/saxo-daytrader` rule in the same policy). The accompanying `<base href="/polytrader/">` in the dashboard HTML ensures that the page's navigation links (`/markets`, etc.) resolve to subpaths that the policy will match and rewrite.

   Save and exit the editor. The operator will reconcile quickly.

6. (Optional) Confirm the policy now contains the rule (owner side):
   ```bash
   kubectl get ngroktrafficpolicy -n saxo-rust daytrader-oauth -o yaml | grep -A 20 -E 'polytrader|url-rewrite'
   ```

## Verification (end-to-end public URL)

- Local cluster check (probes ensure k8s sees it live):
  ```bash
  kubectl get all,agentendpoints -n polytrader
  kubectl logs -n polytrader deploy/polytrader --tail=20
  # Port-forward test (bypasses ngrok):
  kubectl port-forward -n polytrader svc/polytrader 8080:80
  # then in another shell: curl http://localhost:8080/health ; open http://localhost:8080
  ```

- Public URL (after the policy patch + browser Google SSO login with allowed email):
  - Open: https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader
  - Expect: The Phase 0 HTML safety banner ("PAPER TRADING ONLY"), live snapshot numbers, and the three links.
  - Click the links or manually test subpaths — they must work without 404:
    - https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader/markets  (JSON list)
    - https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader/paper/portfolio
    - https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader/health  (JSON)
  - The root-relative links on the banner now correctly target the subpath thanks to the `<base>` + rewrite combination.

- curl example (note: full SSO/cookies usually required; useful for header inspection):
  ```bash
  curl -v https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader/health
  ```

- Check probes are active (from kustomize output or describe):
  ```bash
  kubectl describe deploy polytrader -n polytrader | grep -A 20 'Liveness\|Readiness'
  ```

See `wiki/runbooks/k8s-diagnostics.md` for more debugging commands.

## Rollback
- Remove the `/polytrader` rule from the traffic policy (via `kubectl edit` on daytrader-oauth in saxo-rust). The public path will stop forwarding.
- If desired: `kubectl delete agentendpoint -n polytrader polytrader-internal` (the Deployment/Service remain; re-apply kustomize to restore the AgentEndpoint).
- Rollback polytrader Deployment if probes cause issues: `kubectl rollout undo deploy/polytrader -n polytrader`.
- The shared tunnel/SSO itself is owned by saxo-rust and unaffected.

## When to page a human
- AgentEndpoint stays "not ready" or shows errors after apply (check RBAC for ngrok-operator in polytrader ns; events: `kubectl get events -n polytrader --sort-by=.lastTimestamp`).
- Public URL returns 404/5xx even after correct policy patch + SSO (verify rewrite rule syntax exactly matches the saxo-daytrader example; check polytrader pod logs and that /health responds 200 inside cluster).
- Email allowlist blocks access (SSO succeeds but custom-response 403 from policy).
- Probes cause repeated restarts (tune thresholds in an overlay; server may be slow to bind DB).
- Any change to the shared tunnel structure (new CRD version, policy refactor) — re-discover with the commands listed in the AgentEndpoint yaml comments.

Update this runbook (and the AgentEndpoint comments) when the pattern evolves or a dedicated tunnel is added.
