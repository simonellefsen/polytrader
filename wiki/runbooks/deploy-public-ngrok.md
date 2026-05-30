# Deploy Public ngrok Subpath Exposure (polytrader)

> **STRICT NAMESPACE SAFETY RULE**  
> All polytrader-related `kubectl` operations **must** be limited to the `polytrader` namespace.  
> The **only** allowed exception is the explicit one-time update of the shared tunnel's `NgrokTrafficPolicy` (in the `saxo-rust` namespace).  
> Never use `--all-namespaces`, `kubectl ... --all`, or broad prune commands.  
> Always prefer `make k8s-*` targets (see root `Makefile`).

Actionable runbook for making the polytrader web UI (currently the axum dashboard serving
health, /markets, /paper/portfolio, and HTML banner at /) reachable at the public shared tunnel URL `https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader` (and subpaths) using the established AgentEndpoint + NgrokTrafficPolicy pattern.

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
   kubectl wait --for=condition=ready pod -l cnpg.io/cluster=polytrader -n polytrader --timeout=300s
   kubectl rollout status deploy/polytrader -n polytrader --timeout=120s
   ```

4. Verify the internal AgentEndpoint is created and ready (this is the polytrader half of the wiring):
   ```bash
   kubectl get agentendpoints -n polytrader polytrader-internal -o wide
   # Expected: READY=True, URL=http://polytrader.internal:80, UPSTREAM=http://polytrader.polytrader:80 (the Service), BINDINGS=["internal"]
   kubectl describe agentendpoints -n polytrader polytrader-internal
   ```

5. **ONE-TIME manual step** — add this route in the shared gateway repo (`../shared-ngrok-gateway`) and apply it from there. For emergency live repair, the equivalent manual edit is:
   ```bash
   kubectl edit ngroktrafficpolicy -n saxo-rust daytrader-oauth
   ```
   Insert the following as a **new list item** in the `spec.policy.on_http_request` array (place it with the other forward rules; order among prefix rules does not matter because expressions are specific). Use the exact 2-space indentation:

   ```yaml
   - actions:
     - config:
         url: http://polytrader.internal:80
       type: forward-internal
     expressions:
     - req.url.path.startsWith("/polytrader")
   ```

   This is the recommended version after the 2026-05-26 regression fix. The app serves both clean root paths and raw `/polytrader/*` paths, so the edge can forward the original public path without relying on `url-rewrite` after SSO. The accompanying `<base href="/polytrader/">` in the dashboard HTML still ensures browser links resolve under the shared public prefix.

   Save and exit the editor. The operator will reconcile quickly.

6. (Optional) Confirm the policy now contains the rule (owner side):
   ```bash
   kubectl get ngroktrafficpolicy -n saxo-rust daytrader-oauth -o yaml | grep -A 20 polytrader
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
  - The root-relative links on the banner correctly target the subpath thanks to the `<base>` tag and raw-prefix `/polytrader` routing.

- curl example (note: full SSO/cookies usually required; useful for header inspection):
  ```bash
  curl -v https://unground-uncraftily-vivienne.ngrok-free.dev/polytrader/health
  ```

- Check probes are active (from kustomize output or describe):
  ```bash
  kubectl describe deploy polytrader -n polytrader | grep -A 20 'Liveness\|Readiness'
  ```

See `wiki/runbooks/k8s-diagnostics.md` for more debugging commands.

## Troubleshooting: public `/polytrader` returns `{"error":"not found"}`

If port-forwarding `svc/polytrader` works but the public shared tunnel returns `{"error":"not found"}`, first check the shared ngrok policy rather than rebuilding the app. This usually means the request is reaching the shared endpoint but the live `saxo-rust/daytrader-oauth` `NgrokTrafficPolicy` no longer has the `/polytrader` forward rule.

```bash
kubectl get ngroktrafficpolicy -n saxo-rust daytrader-oauth -o yaml | grep -A 6 polytrader.internal
kubectl get agentendpoint -n polytrader polytrader-internal -o wide
```

Expected policy rule:

```yaml
- actions:
  - config:
      url: http://polytrader.internal:80
    type: forward-internal
  expressions:
  - req.url.path.startsWith("/polytrader")
```

If the rule is missing, run the guarded target:

```bash
make k8s-ngrok-update-policy
```

If the rule disappears after a later deploy, check the shared policy source in `../shared-ngrok-gateway/deploy/k8s/base/gateway.template.yaml`; it must include the same `/polytrader` forward rule.

An unauthenticated curl to the public URL should generally return the ngrok OAuth redirect (`302`). Validate the actual app through an authenticated browser session, or test `/polytrader/health` after SSO.

## In-app Google OAuth Authentication Flow (dual with edge SSO)

**Added 2026-05-25 (IMPL_ID 5701dfea)**. See full details + commands in the top-level append to `wiki/log.md` ("Next Phase: Auth Flow").

The web UI (Dioxus SSR + Axum) now includes a minimal self-contained Google OAuth2 flow for standalone / local / alternative k8s deploys (works *in addition to* or *independent of* the ngrok edge SSO + allowlist in daytrader-oauth policy).

- **Dual-mode support**: Handlers / UI prefer forwarded identity headers from the ngrok policy (common names: x-auth-request-email, x-forwarded-email, x-forwarded-user, etc. — the policy "add headers" step) when present (edge performed the Google SSO + allowlist). Falls back to in-app session cookie for pure local/dev or other deployments without edge auth.
- **Flow**: /auth/login (302 to accounts.google.com with state nonce + client_id + redirect_uri), /auth/callback (validate state, exchange code via reqwest to oauth2.googleapis.com/token, fetch email via googleapis.com/oauth2/v2/userinfo, allowlist check or "any for paper", create short-lived in-mem session, set cookie), /auth/logout (expire cookie), /auth/whoami (JSON for client script).
- **Cookie details** (critical for subpath): HttpOnly, SameSite=Lax (or Strict), Path= normalized SUBPATH_PREFIX or "/", Secure flag configurable (default false for paper dev http; true for prod https). No signing (in-mem uuid lookup); restart clears sessions (acceptable for paper $150).
- **Config (clap + env, no main change needed)**: GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET, GOOGLE_REDIRECT_URI (MUST be the full public URL e.g. https://.../polytrader/auth/callback for subpath deploys), ALLOWED_EMAILS (comma sep or empty=any in paper mode), AUTH_COOKIE_SECURE.
- **UI integration (smallest)**: "Login with Google" button / "Signed in as you@example.com | Logout" chip in rsx top area (relative links under <base>); existing client <script> extended to fetch /auth/whoami on load and populate placeholder (fits live fetch pattern exactly; no App props change, no SSR string post-proc for user data).
- **Preservation**: 100% of prior (SSR rsx source + <base> injection, relative JS fetches, /health public always, JSON endpoints, k8s probes, subpath routing compat, paper engine/ingester/hermes/strategy untouched, no Cargo deps added, no migs, fmt/clippy clean).
- **Security notes (AGENTS/RISK)**: state param prevents CSRF, no secrets logged or in HTML, redirect_uri from config (intent whitelist), manual parse only, dual trust model documented, paper-only (user identity for future personalization of $150 bankroll/journal, not real funds). See heavy comments in src/server.rs + config.rs + ui/app.rs.

This fulfills the "next phase" request for auth flow *within the web UI* so it can stand alone while coexisting with edge protection.

Update this runbook (and the AgentEndpoint comments) when the pattern evolves or a dedicated tunnel is added, or when auth moves to DB sessions table (future wiki PR + migration).

## Rollback
- Remove the `/polytrader` rule from the traffic policy (via `kubectl edit` on daytrader-oauth in saxo-rust). The public path will stop forwarding.
- If desired: `kubectl delete agentendpoint -n polytrader polytrader-internal` (the Deployment/Service remain; re-apply kustomize to restore the AgentEndpoint).
- Rollback polytrader Deployment if probes cause issues: `kubectl rollout undo deploy/polytrader -n polytrader`.
- The shared tunnel/SSO itself is owned by saxo-rust and unaffected.

## When to page a human
- AgentEndpoint stays "not ready" or shows errors after apply (check RBAC for ngrok-operator in polytrader ns; events: `kubectl get events -n polytrader --sort-by=.lastTimestamp`).
- Public URL returns 404/5xx even after correct policy patch + SSO (verify the raw-prefix forward rule points to `http://polytrader.internal:80`; check polytrader pod logs and that /health responds 200 inside cluster).
- Email allowlist blocks access (SSO succeeds but custom-response 403 from policy).
- Probes cause repeated restarts (tune thresholds in an overlay; server may be slow to bind DB).
- Any change to the shared tunnel structure (new CRD version, policy refactor) — re-discover with the commands listed in the AgentEndpoint yaml comments.

Update this runbook (and the AgentEndpoint comments) when the pattern evolves or a dedicated tunnel is added.
