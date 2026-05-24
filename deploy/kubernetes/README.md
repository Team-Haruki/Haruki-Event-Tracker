# Kubernetes Deployment

These manifests are intentionally small and plain. They assume PostgreSQL is
provided outside this deployment and mount `haruki-tracker-configs.yaml` from a
Secret so database DSNs and API tokens do not live in a ConfigMap.

```sh
kubectl apply -f deploy/kubernetes/base/secret.example.yaml
kubectl apply -k deploy/kubernetes/base
```

Edit the Secret before applying it. For API-only mode, keep
`servers.<region>.tracker.enabled: false`; Redis and the Sekai API endpoint can
be omitted or left empty because the process skips them when no tracker is
enabled.

The base deployment stays at one replica so it is safe for tracker-enabled
deployments. For API-only replicas, apply the `api-only` overlay:

```sh
kubectl apply -f deploy/kubernetes/base/secret.example.yaml
kubectl apply -k deploy/kubernetes/overlays/api-only
```

Do not scale tracker-enabled pods above one replica until leader election or a
distributed scrape lock exists; otherwise multiple pods can fetch and persist the
same ranking data.

For local OrbStack verification, run:

```sh
scripts/smoke_k8s_orbstack.sh
```

The smoke script builds `haruki-event-tracker:local`, creates a temporary
namespace with PostgreSQL, reuses the base manifests, then checks `/livez` and
`/readyz`. Set `KEEP_NAMESPACE=1` if you want to inspect the temporary resources
after the run.
