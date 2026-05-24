# API-only Overlay

This overlay scales the HTTP API to two replicas. Use it only when all enabled
servers have `tracker.enabled: false`.

```sh
kubectl apply -f ../../base/secret.example.yaml
kubectl apply -k .
```

Tracker-enabled deployments should stay at one replica until a leader-election
or distributed-locking strategy is added; otherwise each replica can scrape and
write the same ranking data.
