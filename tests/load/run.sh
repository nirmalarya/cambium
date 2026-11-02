#!/usr/bin/env bash
# 200 concurrent committers against SQLite and against Postgres (release build).
set -u
BIN=${BIN:-${CARGO_TARGET_DIR:-$PWD/target}/release}
PGURL=${PGURL:-postgres://cambium:cambiumtest@127.0.0.1:55432}
D=$(cd "$(dirname "$0")" && pwd); T=$(mktemp -d); OUT=${OUT:-$T/results.jsonl}
ulimit -n 65536 2>/dev/null || true
one(){ # label db
  export CAMBIUM_DATA=$T/$1 CAMBIUM_ADDR=127.0.0.1:7995 CAMBIUM_DB=$2 CAMBIUM_RATE=0 CAMBIUM_LOG=warn
  mkdir -p $CAMBIUM_DATA
  local tok; tok=$($BIN/cmbd token load 2>/dev/null)
  $BIN/cmbd >$T/$1.log 2>&1 & local pid=$!
  for i in $(seq 100); do curl -sf http://$CAMBIUM_ADDR/readyz >/dev/null && break; sleep 0.1; done
  python3 $D/committers.py --server http://$CAMBIUM_ADDR --token $tok --workers ${WORKERS:-180} --hot ${HOT:-20} --commits ${COMMITS:-10} --label $1 | tee -a $OUT
  curl -s http://$CAMBIUM_ADDR/metrics | grep -E "^cambium_(rate_limited_total|db_pool_job_panics_total|http_requests_total\{.*status=\"5)" | sed "s/^/$1 /" >> $T/metrics
  kill -TERM $pid; wait $pid
}
one sqlite ""
sudo docker exec -i cambium-pg psql -U cambium -d cambium -q -c "DROP DATABASE IF EXISTS loadtest" -c "CREATE DATABASE loadtest"
one postgres $PGURL/loadtest
echo "--- server-side counters after each run:"; cat $T/metrics
sudo docker exec -i cambium-pg psql -U cambium -d cambium -q -c "DROP DATABASE IF EXISTS loadtest"
echo "results: $OUT"
