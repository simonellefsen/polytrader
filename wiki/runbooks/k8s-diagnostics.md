# Kubernetes Diagnostics — polytrader Namespace

Target cluster: docker-desktop (or whatever context has the polytrader workloads).

## Common Commands

```bash
# Namespace overview
kubectl get all -n polytrader

# Postgres (cnpg) cluster status
kubectl get cluster -n polytrader
kubectl describe cluster polytrader-postgres -n polytrader

# Logs
kubectl logs -n polytrader -l app=polytrader --tail=200 -f
kubectl logs -n polytrader -l app=hermes --tail=100 -f

# Exec into pods
kubectl exec -it -n polytrader deploy/polytrader -- /bin/bash
# or for the postgres primary pod (cnpg naming)
kubectl exec -it -n polytrader <primary-pod> -- psql -U postgres -d polytrader

# Port-forward dashboard (once running)
kubectl port-forward -n polytrader svc/polytrader 8080:80
# then open http://localhost:8080
```

## Postgres Specific (cnpg)

```bash
# Switchover (planned)
kubectl cnpg maintenance set --node <standby> -n polytrader   # or use cnpg plugin
# Check replication lag
kubectl exec ... -- psql -c "SELECT * FROM pg_stat_replication;"
```

## When Things Look Broken

1. Check events: `kubectl get events -n polytrader --sort-by=.lastTimestamp`
2. Describe failing pods.
3. Check PVCs and storage (docker-desktop has limited local storage sometimes).
4. For cnpg bootstrap issues: look at the init job / bootstrap logs.
5. Hermes not reflecting? Verify it has DB read access and LLM API key injected.

## Useful Labels / Selectors

- `app=polytrader`
- `app=hermes`
- `cnpg.io/cluster=polytrader-postgres`

Add more as manifests are written.
