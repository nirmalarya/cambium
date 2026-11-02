#!/usr/bin/env bash
# Multi-node: two cmbd processes sharing one Postgres database and one blob directory.
# Starts postgres:16-alpine in a local container (host networking, loopback only),
# runs the Rust Postgres tests, the full e2e suite against Postgres, then cross-node checks.
set -u
BIN=${BIN:-${CARGO_TARGET_DIR:-$PWD/target}/debug}
PGPORT=${PGPORT:-55432}
export CAMBIUM_DB="postgresql://cambium:cambiumtest@127.0.0.1:$PGPORT/cambium"   # local throwaway container only
pass=0; fail=0
ok(){ if eval "$2" >/dev/null 2>&1; then pass=$((pass+1)); echo "PASS $1"; else fail=$((fail+1)); echo "FAIL $1"; fi; }
if ! pg_ready=$(sudo docker exec cambium-pg pg_isready -p $PGPORT 2>/dev/null); then
  sudo docker rm -f cambium-pg >/dev/null 2>&1
  sudo docker run -d --name cambium-pg --network host -e POSTGRES_PASSWORD=cambiumtest -e POSTGRES_USER=cambium -e POSTGRES_DB=cambium \
    -e PGPORT=$PGPORT postgres:16-alpine >/dev/null || { echo "SKIP: cannot start Postgres"; exit 2; }
  for i in $(seq 60); do sudo docker exec cambium-pg pg_isready -p $PGPORT >/dev/null 2>&1 && break; sleep 0.5; done; sleep 1
fi
reset(){ sudo docker exec cambium-pg psql -U cambium -p $PGPORT -d cambium -qc 'DROP SCHEMA public CASCADE; CREATE SCHEMA public;' >/dev/null 2>&1; }
reset
ok "P01 Rust Postgres tests (two engines, one DB)" "CAMBIUM_TEST_PG='$CAMBIUM_DB' cargo test -q -p cambium-core --test pg -- --include-ignored 2>&1 | grep -qE '[0-9]+ passed; 0 failed'"
reset
ok "P02 full e2e suite on Postgres" "PORT=7991 bash $(dirname $0)/e2e.sh 2>&1 | grep -q ', 0 failed'"
reset
# the P02 e2e servers (incl. its rate-limit server on PORT+1) drain for a few seconds after SIGTERM
for i in $(seq 100); do ss -ltn | grep -qE "127.0.0.1:(7991|7992|7993) " || break; sleep 0.2; done
T=$(mktemp -d); export CAMBIUM_BLOB_DIR=$T/blobs CAMBIUM_RATE=0
export CAMBIUM_TOKEN=$(CAMBIUM_DATA=$T/a $BIN/cmbd token alice 2>/dev/null | tail -1)
CAMBIUM_DATA=$T/a CAMBIUM_ADDR=127.0.0.1:7992 $BIN/cmbd >$T/a.log 2>&1 & A=$!
CAMBIUM_DATA=$T/b CAMBIUM_ADDR=127.0.0.1:7993 $BIN/cmbd >$T/b.log 2>&1 & B=$!
trap "kill $A $B 2>/dev/null; [ -n \"${KEEP:-}\" ] || rm -rf $T" EXIT
for p in 7992 7993; do for i in $(seq 50); do curl -sf http://127.0.0.1:$p/v1/health >/dev/null && break; sleep 0.1; done; done
na(){ CAMBIUM_SERVER=http://127.0.0.1:7992 CAMBIUM_CACHE=$T/ca $BIN/cmb "$@"; }
nb(){ CAMBIUM_SERVER=http://127.0.0.1:7993 CAMBIUM_CACHE=$T/cb $BIN/cmb "$@"; }
cd $T
ok "P03 token minted once works on both nodes" 'na whoami | grep -q alice && nb whoami | grep -q alice'
ok "P04 init on node A" 'na init shared wa'
ok "P05 checkout same repo on node B" 'nb checkout shared wb'
(cd wa; echo one > f.txt; na add f.txt; na commit -m one) >/dev/null 2>&1
ok "P06 commit on A visible on B after sync" '(cd wb && nb sync && cat f.txt | grep -q one)'
# race: both nodes commit on the same head at once; exactly one may win
(cd wa; echo a > ra.txt; na add ra.txt) >/dev/null; (cd wb; echo b > rb.txt; nb add rb.txt) >/dev/null
(cd wa; na commit -m race-a >$T/ra 2>&1; echo $? > $T/ea) & R1=$!
(cd wb; nb commit -m race-b >$T/rb 2>&1; echo $? > $T/eb) & R2=$!
wait $R1 $R2
sleep 0.2
ok "P07 concurrent commits via two nodes: exactly one wins" '[ $(( $(cat $T/ea) + $(cat $T/eb) )) != 0 ] && { [ $(cat $T/ea) = 0 ] || [ $(cat $T/eb) = 0 ]; } && nb checkout shared wc2 && [ "$(cd wc2 && nb log -n 5 | grep -c race)" = 1 ]'
ok "P08 both nodes agree on main" '[ "$(curl -s -H "Authorization: Bearer $CAMBIUM_TOKEN" http://127.0.0.1:7992/v1/repos/shared/refs)" = "$(curl -s -H "Authorization: Bearer $CAMBIUM_TOKEN" http://127.0.0.1:7993/v1/repos/shared/refs)" ]'
# 20 raw ref-create CAS calls split over both nodes: exactly one gets 200
HEAD=$(curl -s -H "Authorization: Bearer $CAMBIUM_TOKEN" http://127.0.0.1:7992/v1/repos/shared/refs | python3 -c 'import sys,json; print([r["cs"] for r in json.load(sys.stdin) if r["name"]=="main"][0])')
for i in $(seq 20); do p=$((7992 + i % 2))
  curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $CAMBIUM_TOKEN" -H 'content-type: application/json' \
    -d "{\"expected\":null,\"new\":\"$HEAD\"}" http://127.0.0.1:$p/v1/repos/shared/refs/racer >> $T/codes &
done; wait $(jobs -p | grep -v -e "^$A$" -e "^$B$") 2>/dev/null; sleep 0.5
ok "P09 20 concurrent ref creates across nodes: exactly one 200" '[ "$(grep -c ^200 $T/codes)" = 1 ] && [ "$(grep -c ^409 $T/codes)" = 19 ]'
kill $A $B; wait 2>/dev/null
echo "multinode: $pass passed, $fail failed"
[ $fail = 0 ]
