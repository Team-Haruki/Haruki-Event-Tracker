#!/usr/bin/env bash
# Old-vs-new timings of the web overview's hot queries on a synthetic
# ~5.8M-row CN event, plus the PostgreSQL run of the edge-query
# equivalence test. Starts a throwaway postgres:17 container and removes it
# (with its anonymous volume) on exit.
#
#   tools/bench-rank-edges.sh            # bench + equivalence
#   HET_BENCH_EXPLAIN=1 tools/bench-rank-edges.sh   # also print plans
set -euo pipefail

cd "$(dirname "$0")/.."
name="het-bench-pg-$$"
cleanup() { docker rm -fv "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker run -d --name "$name" \
  -e POSTGRES_PASSWORD=bench -e POSTGRES_DB=bench \
  -p 127.0.0.1::5432 \
  postgres:17 \
  -c shared_buffers=512MB -c work_mem=16MB -c max_wal_size=4GB \
  -c synchronous_commit=off >/dev/null

port="$(docker port "$name" 5432/tcp | head -n1 | sed 's/.*://')"
for _ in $(seq 1 60); do
  docker exec "$name" pg_isready -U postgres -d bench >/dev/null 2>&1 && break
  sleep 1
done
# pg_isready passes during the init-script restart; wait for the final server.
sleep 2
docker exec "$name" pg_isready -U postgres -d bench >/dev/null

url="postgres://postgres:bench@127.0.0.1:${port}/bench"
HET_TEST_PG_URL="$url" cargo test --release --lib -- --ignored --nocapture \
  edge_queries_match_grouped_forms_on_postgres
HET_BENCH_PG_URL="$url" cargo test --release --lib -- --ignored --nocapture \
  bench_overview_queries_on_postgres
