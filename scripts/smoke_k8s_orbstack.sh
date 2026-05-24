#!/usr/bin/env bash
set -euo pipefail

IMAGE="${IMAGE:-haruki-event-tracker:local}"
VERSION="${VERSION:-3.0.0-dev}"
POSTGRES_IMAGE="${POSTGRES_IMAGE:-postgres:16-alpine}"
TARGET_CONTEXT="${KUBE_CONTEXT:-$(kubectl config current-context)}"
NAMESPACE="${NAMESPACE:-haruki-smoke-$(date +%s)}"
LOCAL_PORT="${LOCAL_PORT:-$((18080 + (RANDOM % 1000)))}"
BUILD_IMAGE="${BUILD_IMAGE:-1}"
KEEP_NAMESPACE="${KEEP_NAMESPACE:-0}"

if [[ "${TARGET_CONTEXT}" != "orbstack" && "${ALLOW_OTHER_CONTEXT:-0}" != "1" ]]; then
  echo "Refusing to run against Kubernetes context '${TARGET_CONTEXT}'. Set ALLOW_OTHER_CONTEXT=1 to override." >&2
  exit 1
fi

for cmd in docker kubectl curl; do
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "Missing required command: ${cmd}" >&2
    exit 1
  fi
done

KUBECTL=(kubectl --context "${TARGET_CONTEXT}")
PF_PID=""

cleanup() {
  if [[ -n "${PF_PID}" ]]; then
    kill "${PF_PID}" >/dev/null 2>&1 || true
    wait "${PF_PID}" >/dev/null 2>&1 || true
  fi
  if [[ "${KEEP_NAMESPACE}" != "1" ]]; then
    "${KUBECTL[@]}" delete namespace "${NAMESPACE}" --ignore-not-found >/dev/null 2>&1 || true
  else
    echo "Keeping namespace ${NAMESPACE}"
  fi
}
trap cleanup EXIT

if [[ "${BUILD_IMAGE}" == "1" ]]; then
  docker build --build-arg "VERSION=${VERSION}" -t "${IMAGE}" .
fi

"${KUBECTL[@]}" create namespace "${NAMESPACE}" >/dev/null

"${KUBECTL[@]}" -n "${NAMESPACE}" apply -f - <<YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: postgres
spec:
  replicas: 1
  selector:
    matchLabels:
      app: postgres
  template:
    metadata:
      labels:
        app: postgres
    spec:
      containers:
        - name: postgres
          image: ${POSTGRES_IMAGE}
          imagePullPolicy: IfNotPresent
          env:
            - name: POSTGRES_DB
              value: haruki_tracker_jp
            - name: POSTGRES_USER
              value: haruki
            - name: POSTGRES_PASSWORD
              value: haruki
          ports:
            - name: postgres
              containerPort: 5432
          readinessProbe:
            exec:
              command:
                - pg_isready
                - -U
                - haruki
                - -d
                - haruki_tracker_jp
            initialDelaySeconds: 3
            periodSeconds: 5
            timeoutSeconds: 3
---
apiVersion: v1
kind: Service
metadata:
  name: postgres
spec:
  selector:
    app: postgres
  ports:
    - name: postgres
      port: 5432
      targetPort: postgres
YAML

"${KUBECTL[@]}" -n "${NAMESPACE}" rollout status deployment/postgres --timeout=180s

"${KUBECTL[@]}" -n "${NAMESPACE}" apply -f - <<'YAML'
apiVersion: v1
kind: Secret
metadata:
  name: haruki-event-tracker-config
type: Opaque
stringData:
  haruki-tracker-configs.yaml: |
    backend:
      host: 0.0.0.0
      port: 8080
      ssl: false
      log_level: INFO
      main_log_file: ""
      access_log_path: ""
      enable_trust_proxy: true
      trusted_proxies:
        - "10.0.0.0/8"
        - "172.16.0.0/12"
        - "192.168.0.0/16"
      proxy_header: "X-Forwarded-For"

    servers:
      jp:
        enabled: true
        master_data_dir: "/tmp/master"
        tracker:
          enabled: false
          use_second_level_cron: false
          cron: "*/2 * * * *"
        gorm_config:
          dialect: postgres
          dsn: "postgres://haruki:haruki@postgres:5432/haruki_tracker_jp?sslmode=disable"
          max_open_conns: 8
          max_idle_conns: 1
          conn_max_lifetime: 1h
YAML

"${KUBECTL[@]}" -n "${NAMESPACE}" apply -f deploy/kubernetes/base/serviceaccount.yaml
"${KUBECTL[@]}" -n "${NAMESPACE}" apply -f deploy/kubernetes/base/service.yaml
"${KUBECTL[@]}" -n "${NAMESPACE}" apply -f deploy/kubernetes/base/pdb.yaml
kubectl set image --local -f deploy/kubernetes/base/deployment.yaml "app=${IMAGE}" -o yaml \
  | "${KUBECTL[@]}" -n "${NAMESPACE}" apply -f -
"${KUBECTL[@]}" -n "${NAMESPACE}" rollout status deployment/haruki-event-tracker --timeout=180s

"${KUBECTL[@]}" -n "${NAMESPACE}" logs deployment/haruki-event-tracker --tail=50

"${KUBECTL[@]}" -n "${NAMESPACE}" port-forward service/haruki-event-tracker "${LOCAL_PORT}:8080" >/tmp/haruki-event-tracker-port-forward.log 2>&1 &
PF_PID="$!"

for _ in {1..30}; do
  if curl -fsS "http://127.0.0.1:${LOCAL_PORT}/livez" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

echo "livez:  $(curl -fsS "http://127.0.0.1:${LOCAL_PORT}/livez")"
echo "readyz: $(curl -fsS "http://127.0.0.1:${LOCAL_PORT}/readyz")"
echo "OrbStack Kubernetes smoke passed in namespace ${NAMESPACE}"
