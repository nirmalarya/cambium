#!/usr/bin/env bash
# Load test of the reference deployment (PostgreSQL + RustFS + cmbd, docker compose)
# with the changeset workflow: agents landing changesets while CI checkouts walk
# the tree protocol. Brings up a throwaway compose project, runs
# tests/load/load.py, records container CPU/memory, tears everything down.
#   bash tests/load/run.sh                 # defaults: 20k files, 32 landers, 16 readers, 120 s
#   LANDERS=64 READERS=32 DURATION=300 bash tests/load/run.sh
set -eu
D=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$D/../.." && pwd)
P=cambium-load; ENVF=$(mktemp); OUT=${OUT:-$ROOT/docs/load-results.jsonl}
printf 'POSTGRES_PASSWORD=%s\nRUSTFS_ACCESS_KEY=load%s\nRUSTFS_SECRET_KEY=%s\nCAMBIUM_RATE=0\n' \
  "$(openssl rand -hex 16)" "$(openssl rand -hex 4)" "$(openssl rand -hex 20)" > "$ENVF"
cleanup(){ docker compose -p $P --env-file "$ENVF" down -v >/dev/null 2>&1 || true; rm -f "$ENVF"; }
trap cleanup EXIT
cd "$ROOT"
docker compose -p $P --env-file "$ENVF" up --build -d >/dev/null
for i in $(seq 120); do curl -sf http://127.0.0.1:7878/readyz >/dev/null && break; sleep 1; done
TOK=$(docker compose -p $P exec -T cmbd cmbd token load 2>/dev/null | tail -1)
docker stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}' > /dev/null
( while sleep 5; do docker stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}' | grep $P; done ) > /tmp/$P-stats.log 2>/dev/null & STATS=$!
python3 "$D/load.py" --server http://127.0.0.1:7878 --token "$TOK" \
  --files ${FILES:-20000} --dirs ${DIRS:-500} --landers ${LANDERS:-32} --readers ${READERS:-16} \
  --duration ${DURATION:-120} --label "${LABEL:-compose postgres+rustfs}" | tee /tmp/$P-result.json
kill $STATS 2>/dev/null || true
python3 - "$OUT" /tmp/$P-stats.log /tmp/$P-result.json <<'PY'
import json, sys, re, collections
out, stats, res = sys.argv[1:]
r = json.load(open(res))
peak = collections.defaultdict(lambda: [0.0, ""])
for l in open(stats):
    n, cpu, mem = l.split(None, 2)
    c = float(cpu.rstrip('%'))
    svc = re.sub(r'^cambium-load-|-\d+$', '', n)
    if c >= peak[svc][0]:
        peak[svc] = [c, mem.split('/')[0].strip()]
r["peak_cpu_pct_mem"] = {k: f"{v[0]:.0f}% {v[1]}" for k, v in peak.items()}
print("peak container usage:", r["peak_cpu_pct_mem"])
open(out, "a").write(json.dumps(r) + "\n")
PY
docker compose -p $P logs cmbd 2>&1 | grep -c '"level":"error"' | sed 's/^/cmbd error log lines: /'
