#!/usr/bin/env bash
# Restore drill: populate Postgres A + blob dir A through the real server, back up,
# restore into a brand-new Postgres database B + empty blob dir B, then prove the
# restored system serves identical content, counts and an intact audit chain.
# Also drills SQLite -> Postgres (migration path). Needs the local test Postgres
# (PGURL, default the throwaway container on 55432).
set -u
BIN=${BIN:-${CARGO_TARGET_DIR:-target}/release}
PGBASE=${PGBASE:-postgres://cambium:cambiumtest@127.0.0.1:55432}
PSQL(){ sudo docker exec -i cambium-pg psql -U cambium -d cambium -tAq "$@"; }
T=$(mktemp -d); pass=0; fail=0
ok(){ if eval "$2" >/dev/null 2>&1; then pass=$((pass+1)); echo "PASS $1"; else fail=$((fail+1)); echo "FAIL $1"; fi; }
for db in drill_a drill_b drill_c; do PSQL -c "DROP DATABASE IF EXISTS $db" -c "CREATE DATABASE $db"; done
run(){ # name db blobdir port
  CAMBIUM_DATA=$T/$1 CAMBIUM_DB=$2 CAMBIUM_BLOB_DIR=$3 CAMBIUM_ADDR=127.0.0.1:$4 $BIN/cmbd >>$T/$1.log 2>&1 & echo $!
}
wait_up(){ for i in $(seq 80); do curl -sf http://127.0.0.1:$1/readyz >/dev/null && return; sleep 0.1; done; }
mkdir -p $T/A $T/B $T/C $T/S
export CAMBIUM_CACHE=$T/cache
# ---- populate A ----
export CAMBIUM_DB=$PGBASE/drill_a CAMBIUM_BLOB_DIR=$T/blobsA CAMBIUM_DATA=$T/A
TOK=$($BIN/cmbd token alice --groups devs 2>/dev/null); $BIN/cmbd token bob >/dev/null 2>&1
PA=$(run A $CAMBIUM_DB $CAMBIUM_BLOB_DIR 7991); wait_up 7991
export CAMBIUM_TOKEN=$TOK CAMBIUM_SERVER=http://127.0.0.1:7991
cd $T; $BIN/cmb init drill w >/dev/null; cd w
for i in $(seq 30); do mkdir -p d$((i%5)); echo "file $i $RANDOM" > d$((i%5))/f$i.txt; done
head -c 300000 /dev/urandom > big.bin
$BIN/cmb add . >/dev/null; $BIN/cmb commit -m base >/dev/null
for i in $(seq 10); do echo "edit $i" >> d1/f1.txt; $BIN/cmb add d1 >/dev/null; $BIN/cmb commit -m "edit $i" >/dev/null; done
$BIN/cmb branch feat >/dev/null; echo f > feat.txt; $BIN/cmb add feat.txt >/dev/null; $BIN/cmb commit -m feat >/dev/null; $BIN/cmb review create -m "drill review" >/dev/null
cd $T; kill $PA; wait $PA 2>/dev/null
counts(){ PSQL -d $1 -c "SELECT (SELECT count(*) FROM changesets)||' '||(SELECT count(*) FROM refs)||' '||(SELECT count(*) FROM tokens)||' '||(SELECT count(*) FROM reviews)||' '||(SELECT count(*) FROM audit_log)"; }
CA=$(counts drill_a)
# ---- backup ----
$BIN/cmbd backup $T/bk > $T/backup.out 2>&1
ok "R01 backup completes with manifest + checksums" 'grep -q "backup ok" $T/backup.out && [ -f $T/bk/manifest.json ] && python3 -c "import json; m=json.load(open(\"$T/bk/manifest.json\")); assert m[\"blobs\"][\"count\"]>0"'
AUDIT_A=$(PSQL -d drill_a -c "SELECT count(*) FROM audit_log")
# ---- disaster: lose A completely, restore into fresh B ----
rm -rf $T/blobsA
export CAMBIUM_DB=$PGBASE/drill_b CAMBIUM_BLOB_DIR=$T/blobsB CAMBIUM_DATA=$T/B
START=$(date +%s.%N)
$BIN/cmbd restore $T/bk > $T/restore.out 2>&1; RC=$?
END=$(date +%s.%N)
ok "R02 restore into empty Postgres + empty blob store" '[ $RC = 0 ] && grep -q "audit chain: intact" $T/restore.out'
CB=$(counts drill_b)
ok "R03 row counts identical (changesets refs tokens reviews; audit +1 for the restore event)" '[ "${CA% *}" = "${CB% *}" ] && [ ${CB##* } = $((${CA##* } + 1)) ]'
PB=$(run B $CAMBIUM_DB $CAMBIUM_BLOB_DIR 7992); wait_up 7992
export CAMBIUM_SERVER=http://127.0.0.1:7992
ok "R04 old token still authenticates after restore" '[ $(curl -s -o /dev/null -w %{http_code} -H "Authorization: Bearer $TOK" $CAMBIUM_SERVER/v1/whoami) = 200 ]'
ok "R05 restored checkout byte-identical to the original working copy" 'cd $T && CAMBIUM_CACHE=$T/c2 $BIN/cmb checkout drill wb >/dev/null && diff -r -x .cambium w wb >/dev/null || (cd $T/w && $BIN/cmb switch main >/dev/null && cd $T && diff -r -x .cambium w wb)'
ok "R06 history, branch and review preserved" 'cd $T/wb && [ $($BIN/cmb log | wc -l) -ge 12 ] && $BIN/cmb branch | grep -q feat && $BIN/cmb review list | grep -q "drill review"'
ok "R07 restored server accepts new commits (sequences continue)" 'cd $T/wb && echo new > n.txt && $BIN/cmb add n.txt && $BIN/cmb commit -m after-restore'
ok "R08 audit verify on restored system" 'CAMBIUM_DB=$PGBASE/drill_b $BIN/cmbd audit verify'
ok "R09 restore refuses a non-empty target" '! $BIN/cmbd restore $T/bk'
kill $PB; wait $PB 2>/dev/null
cp -r $T/bk $T/bk-bad; f=$(find $T/bk-bad/blobs -type f | head -1); echo corrupt >> $f
export CAMBIUM_DB=$PGBASE/drill_c CAMBIUM_BLOB_DIR=$T/blobsC CAMBIUM_DATA=$T/C
ok "R10 corrupted backup refused, target left empty" '! $BIN/cmbd restore $T/bk-bad && [ "$(PSQL -d drill_c -c "SELECT count(*) FROM changesets")" = 0 ]'
# ---- SQLite -> Postgres ----
unset CAMBIUM_DB; export CAMBIUM_DATA=$T/S CAMBIUM_BLOB_DIR=$T/blobsS
ST=$($BIN/cmbd token sam 2>/dev/null)
PS=$(run S "" $T/blobsS 7993); wait_up 7993
(export CAMBIUM_TOKEN=$ST CAMBIUM_SERVER=http://127.0.0.1:7993; cd $T && $BIN/cmb init lite wl >/dev/null && cd wl && echo lite > l.txt && $BIN/cmb add l.txt >/dev/null && $BIN/cmb commit -m lite >/dev/null) 2>&1 | sed "s/^/  sqlite-populate: /"
kill $PS; wait $PS 2>/dev/null
$BIN/cmbd backup $T/bks >/dev/null 2>&1
PSQL -c "DROP DATABASE IF EXISTS drill_c" -c "CREATE DATABASE drill_c"; rm -rf $T/blobsC
export CAMBIUM_DB=$PGBASE/drill_c CAMBIUM_BLOB_DIR=$T/blobsC CAMBIUM_DATA=$T/C
ok "R11 SQLite backup restores into Postgres" '$BIN/cmbd restore $T/bks | grep -q intact'
PC=$(run C $CAMBIUM_DB $T/blobsC 7994); wait_up 7994
ok "R12 migrated content served from Postgres" 'cd $T && CAMBIUM_CACHE=$T/c3 CAMBIUM_TOKEN=$ST CAMBIUM_SERVER=http://127.0.0.1:7994 $BIN/cmb checkout lite wl2 >/dev/null && [ "$(cat wl2/l.txt)" = lite ]'
kill $PC; wait $PC 2>/dev/null
echo "drill: backup $(du -sh $T/bk | cut -f1), $(python3 -c "import json; m=json.load(open('$T/bk/manifest.json')); print(sum(t['rows'] for t in m['tables'].values()) if isinstance(m['tables'],dict) else sum(t['rows'] for t in m['tables']),'rows,',m['blobs']['count'],'blobs')"), restore took $(python3 -c "print(round($END-$START,2))")s"
echo "drill: counts A=[$CA] B=[$CB] (changesets refs tokens reviews audit)"
echo "drill: $pass passed, $fail failed"
for db in drill_a drill_b drill_c; do PSQL -c "DROP DATABASE IF EXISTS $db"; done
[ -n "${KEEP:-}" ] && echo "kept $T" || rm -rf $T
[ $fail = 0 ]
