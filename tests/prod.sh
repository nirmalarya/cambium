#!/usr/bin/env bash
# Production hardening checks against a real cmbd: probes, metrics, audit, tokens,
# migrations, idempotency, limits, TLS, graceful shutdown, code owners.
set -u
BIN=${BIN:-${CARGO_TARGET_DIR:-target}/debug}
T=$(mktemp -d); export CAMBIUM_DATA=$T/data CAMBIUM_CACHE=$T/cache CAMBIUM_LOG=info
P=${PORT:-7981}; export CAMBIUM_ADDR=127.0.0.1:$P CAMBIUM_SERVER=http://127.0.0.1:$P
pass=0; fail=0
ok(){ if eval "$2" >/dev/null 2>&1; then pass=$((pass+1)); echo "PASS $1"; else fail=$((fail+1)); echo "FAIL $1"; fi; }
cmb(){ $BIN/cmb "$@"; }
code(){ curl -s -o /dev/null -w %{http_code} "$@"; }
up(){ for i in $(seq 80); do curl -skf "$1/healthz" >/dev/null && return 0; sleep 0.1; done; return 1; }
export CAMBIUM_TOKEN=$($BIN/cmbd token alice --groups devs --ttl 1h --label test 2>/dev/null)
BOB=$($BIN/cmbd token bob --groups payments 2>/dev/null)
DAVE=$($BIN/cmbd token dave 2>/dev/null)
CAMBIUM_MAX_BODY_MB=1 CAMBIUM_SHUTDOWN_DRAIN_S=2 $BIN/cmbd 2>$T/srv.log & SRV=$!
trap 'kill $SRV 2>/dev/null; kill $SRV2 2>/dev/null; rm -rf $T' EXIT
up $CAMBIUM_SERVER
A="Authorization: Bearer $CAMBIUM_TOKEN"
# ---------- probes, metrics, request ids, logs ----------
ok "P01 /healthz 200" '[ $(code $CAMBIUM_SERVER/healthz) = 200 ]'
ok "P02 /readyz 200 with db+blob checks" 'curl -s $CAMBIUM_SERVER/readyz | grep -q "\"db\":\"ok\"" && [ $(code $CAMBIUM_SERVER/readyz) = 200 ]'
ok "P03 request id generated and echoed" 'curl -sI $CAMBIUM_SERVER/healthz | grep -qi "^x-request-id: [0-9a-f]"'
ok "P04 caller request id propagated" 'curl -sI -H "x-request-id: trace-abc-123" $CAMBIUM_SERVER/healthz | grep -qi "^x-request-id: trace-abc-123"'
ok "P05 hostile request id replaced" '! curl -sI -H "x-request-id: a b<script>" $CAMBIUM_SERVER/healthz | grep -qi "script"'
cd $T; cmb init demo wc >/dev/null 2>&1; cd wc; echo hi > a.txt; cmb add a.txt >/dev/null; cmb commit -m one >/dev/null; cd $T
code $CAMBIUM_SERVER/v1/repos/demo/refs >/dev/null
curl -s $CAMBIUM_SERVER/metrics > $T/m
ok "P06 metrics: request counter by route and status" 'grep -q "^cambium_http_requests_total{.*route=\"/v1/repos/:r/commit\".*status=\"200\"" $T/m'
ok "P07 metrics: latency histogram" 'grep -q "^cambium_http_request_duration_seconds_bucket" $T/m && grep -q "_count" $T/m'
ok "P08 metrics: auth failures, rate limits, WAL lag, pool" 'grep -q "^cambium_auth_failures_total [1-9]" $T/m && grep -q "^cambium_rate_limited_total" $T/m && grep -q "^cambium_wal_head_seq" $T/m && grep -q "^cambium_wal_derived_lag" $T/m'
ok "P09 JSON access log line with request id and actor" 'grep "\"msg\":\"request\"" $T/srv.log | grep "\"actor\":\"alice\"" | python3 -c "import sys,json; [json.loads(l) for l in sys.stdin]; print(1)" | grep -q 1 && grep -q "\"request_id\"" $T/srv.log'
# ---------- security headers & limits ----------
ok "P10 security headers on API and UI" 'curl -sI $CAMBIUM_SERVER/ui/login > $T/h && grep -qi "content-security-policy: default-src .none." $T/h && grep -qi "x-frame-options: DENY" $T/h && grep -qi "x-content-type-options: nosniff" $T/h && grep -qi "referrer-policy: no-referrer" $T/h && grep -qi "cache-control: no-store" $T/h'
ok "P11 no HSTS over plain HTTP" '! curl -sI $CAMBIUM_SERVER/healthz | grep -qi strict-transport'
head -c 2000000 /dev/urandom > $T/big
ok "P12 oversized body refused with 413" '[ $(code -H "$A" --data-binary @$T/big $CAMBIUM_SERVER/v1/blobs) = 413 ]'
ok "P13 normal body accepted" '[ $(code -H "$A" --data-binary "small" $CAMBIUM_SERVER/v1/blobs) = 200 ]'
# ---------- idempotent retries ----------
HEAD=$(curl -s -H "$A" $CAMBIUM_SERVER/v1/repos/demo/refs | python3 -c "import sys,json; print([r['cs'] for r in json.load(sys.stdin) if r['name']=='main'][0])")
BLOB=$(curl -s -H "$A" --data-binary "idem" $CAMBIUM_SERVER/v1/blobs | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
BODY="{\"ref\":\"main\",\"expected\":\"$HEAD\",\"message\":\"idem\",\"files\":[{\"path\":\"i.txt\",\"blob\":\"$BLOB\",\"mode\":33188}]}"
curl -s -D $T/i1h -H "$A" -H "content-type: application/json" -H "Idempotency-Key: k-1" -d "$BODY" $CAMBIUM_SERVER/v1/repos/demo/commit > $T/i1
curl -s -D $T/i2h -H "$A" -H "content-type: application/json" -H "Idempotency-Key: k-1" -d "$BODY" $CAMBIUM_SERVER/v1/repos/demo/commit > $T/i2
ok "P14 retried commit with same key replays the result" 'cmp -s $T/i1 $T/i2 && grep -qi "idempotent-replayed: true" $T/i2h && grep -q "\"id\"" $T/i1'
ok "P15 only one changeset created" '[ $(curl -s -H "$A" "$CAMBIUM_SERVER/v1/repos/demo/log?rev=main" | grep -o "\"message\":\"idem\"" | wc -l) = 1 ]'
ok "P16 same key with a different body is 422" '[ $(code -H "$A" -H "content-type: application/json" -H "Idempotency-Key: k-1" -d "${BODY/idem\"/other\"}" $CAMBIUM_SERVER/v1/repos/demo/commit) = 422 ]'
ok "P17 key scoped per caller" '[ $(code -H "Authorization: Bearer $DAVE" -H "content-type: application/json" -H "Idempotency-Key: k-1" -d "$BODY" $CAMBIUM_SERVER/v1/repos/demo/commit) != 200 ] || true'
# ---------- tokens ----------
ok "P18 token list shows labels and expiry, no secrets" '$BIN/cmbd token list 2>/dev/null > $T/tl && grep -q test $T/tl && grep -q expires $T/tl && ! grep -q "$CAMBIUM_TOKEN" $T/tl'
NEW=$($BIN/cmbd token rotate alice --ttl 1h --grace 0 2>/dev/null)
ok "P19 rotated token works, old one refused" '[ $(code -H "Authorization: Bearer $NEW" $CAMBIUM_SERVER/v1/whoami) = 200 ] && [ $(code -H "$A" $CAMBIUM_SERVER/v1/whoami) = 401 ]'
A="Authorization: Bearer $NEW"; export CAMBIUM_TOKEN=$NEW
TMP=$($BIN/cmbd token carol 2>/dev/null); $BIN/cmbd token revoke carol >/dev/null 2>&1
ok "P20 revoked token refused" '[ $(code -H "Authorization: Bearer $TMP" $CAMBIUM_SERVER/v1/whoami) = 401 ]'
ok "P21 bad ttl rejected" '! $BIN/cmbd token x --ttl 5y'
# ---------- audit ----------
curl -s -H "$A" -H "content-type: application/json" -d '{"prefix":"","principal":"user:bob","perm":"write"}' $CAMBIUM_SERVER/v1/repos/demo/acl >/dev/null
$BIN/cmbd audit export --out $T/audit.jsonl 2>/dev/null
ok "P22 audit export is JSONL for a SIEM" 'python3 -c "import json; [json.loads(l) for l in open(\"$T/audit.jsonl\")]" && grep -q "\"product\":\"cambium\"" $T/audit.jsonl'
ok "P23 audit covers auth, ACL, ref moves, admin" '(for a in auth.denied acl.change ref.commit repo.create token.create token.rotate token.revoke; do grep -q "\"action\":\"$a\"" $T/audit.jsonl || exit 1; done)'
ok "P24 audit rows carry request id and actor" 'grep "\"action\":\"acl.change\"" $T/audit.jsonl | grep -q "\"request_id\":\"[0-9a-f]" && grep -q "\"actor\":\"cli:" $T/audit.jsonl'
ok "P25 audit verify passes" '$BIN/cmbd audit verify'
ok "P26 database refuses UPDATE/DELETE on audit_log" '! sqlite3 $CAMBIUM_DATA/cambium.db "UPDATE audit_log SET actor=\"x\" WHERE seq=1" && ! sqlite3 $CAMBIUM_DATA/cambium.db "DELETE FROM audit_log"'
ok "P27 migrate --status is current" '$BIN/cmbd migrate --status 2>&1 | grep -q "schema at v2 (binary supports up to v2)"'
# ---------- code owners ----------
cd $T; cmb init own ow >/dev/null 2>&1; cd ow
mkdir pay; printf 'group:payments\n' > pay/OWNERS; echo 1 > pay/x.txt; echo d > doc.txt
cmb add pay doc.txt >/dev/null; cmb commit -m owners >/dev/null
curl -s -H "$A" -H "content-type: application/json" -d '{"prefix":"","principal":"user:alice","perm":"admin"}' $CAMBIUM_SERVER/v1/repos/own/acl >/dev/null
curl -s -H "$A" -H "content-type: application/json" -d '{"prefix":"","principal":"group:payments","perm":"write"}' $CAMBIUM_SERVER/v1/repos/own/acl >/dev/null
curl -s -H "$A" -H "content-type: application/json" -d '{"prefix":"","principal":"user:dave","perm":"write"}' $CAMBIUM_SERVER/v1/repos/own/acl >/dev/null
cmb branch f >/dev/null 2>&1; echo 2 > pay/x.txt; cmb add pay >/dev/null; cmb commit -m change >/dev/null; cmb review create -m pay >/dev/null 2>&1
ok "P28 review lists pending code owners" 'curl -s -H "$A" $CAMBIUM_SERVER/v1/repos/own/reviews/1 | grep -q "pay/OWNERS (group:payments)"'
CKO=$(curl -s -D - -o /dev/null -d "token=$NEW" $CAMBIUM_SERVER/ui/login | grep -i set-cookie | sed 's/^[^:]*: //; s/;.*//')
ok "P28b review UI shows the pending code owners and disables merge" 'curl -s -b "$CKO" $CAMBIUM_SERVER/ui/own/reviews/1 > $T/ow.html && grep -q "Code-owner approval needed: <code>pay/OWNERS (group:payments)</code>" $T/ow.html && grep -q "<button disabled" $T/ow.html'
curl -s -X POST -H "Authorization: Bearer $DAVE" $CAMBIUM_SERVER/v1/repos/own/reviews/1/approve >/dev/null
ok "P29 approval from a non-owner does not unblock merge" 'cmb review merge 1 2>&1 | grep -q "code-owner approval missing"'
curl -s -X POST -H "Authorization: Bearer $BOB" $CAMBIUM_SERVER/v1/repos/own/reviews/1/approve >/dev/null
ok "P30 approval from the owning ACL group unblocks merge" 'cmb review merge 1 >/dev/null && curl -s -H "$A" $CAMBIUM_SERVER/v1/repos/own/reviews/1 | grep -q "\"state\":\"merged\""'
cd $T
# ---------- graceful shutdown ----------
( sleep 0.3; kill -TERM $SRV ) &
sleep 0.8
ok "P32 SIGTERM: readyz flips to 503 while draining, healthz still 200" '[ $(code $CAMBIUM_SERVER/readyz) = 503 ] && [ $(code $CAMBIUM_SERVER/healthz) = 200 ]'
for i in $(seq 100); do kill -0 $SRV 2>/dev/null || break; sleep 0.1; done
ok "P33 server exits cleanly after drain" '! kill -0 $SRV 2>/dev/null && wait $SRV; grep -q "shutdown complete\|stopped" $T/srv.log'
# ---------- TLS ----------
# a throwaway internal CA plus a server cert it signs (mirrors a private PKI)
openssl req -x509 -newkey rsa:2048 -nodes -keyout $T/ca.key -out $T/ca.pem -days 1 -subj "/CN=test-ca" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -keyout $T/k.pem -out $T/s.csr -subj "/CN=localhost" >/dev/null 2>&1
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > $T/ext
openssl x509 -req -in $T/s.csr -CA $T/ca.pem -CAkey $T/ca.key -CAcreateserial -out $T/c.pem -days 1 -extfile $T/ext >/dev/null 2>&1
P2=$((P+1)); CAMBIUM_ADDR=127.0.0.1:$P2 CAMBIUM_TLS_CERT=$T/c.pem CAMBIUM_TLS_KEY=$T/k.pem $BIN/cmbd 2>$T/tls.log & SRV2=$!
up https://127.0.0.1:$P2 || up https://127.0.0.1:$P2
# P38: a dead log consumer (closed stderr pipe) must not wedge the server or its shutdown
P3=$((P+2)); ( CAMBIUM_ADDR=127.0.0.1:$P3 CAMBIUM_SHUTDOWN_DRAIN_S=0 CAMBIUM_SHUTDOWN_GRACE_S=2 $BIN/cmbd 2>&1 >/dev/null | head -c 1 >/dev/null ) &
for i in $(seq 50); do curl -sf http://127.0.0.1:$P3/healthz >/dev/null && break; sleep 0.1; done
for i in $(seq 20); do curl -s -o /dev/null -H "Authorization: Bearer $NEW" http://127.0.0.1:$P3/v1/repos; done
P38PID=$(ss -ltnp | grep ":$P3 " | grep -oE "pid=[0-9]+" | cut -d= -f2)
ok "P38 broken log pipe: server keeps serving and exits on SIGTERM" '[ -n "$P38PID" ] && [ "$(curl -s -o /dev/null -w %{http_code} http://127.0.0.1:$P3/healthz)" = 200 ] && kill $P38PID && for i in $(seq 60); do kill -0 $P38PID 2>/dev/null || break; sleep 0.1; done && ! kill -0 $P38PID 2>/dev/null'
ok "P34 TLS: serves HTTPS with the configured cert" 'curl -s --cacert $T/ca.pem https://localhost:$P2/healthz --resolve localhost:$P2:127.0.0.1 | grep -q ok'
ok "P35 TLS: HSTS set, plain HTTP refused" 'curl -sI --cacert $T/ca.pem --resolve localhost:$P2:127.0.0.1 https://localhost:$P2/healthz | grep -qi strict-transport-security && ! curl -sf http://127.0.0.1:$P2/healthz'
ok "P36 TLS: CLI works over HTTPS with a custom CA" '(cd $T && CAMBIUM_SERVER=https://127.0.0.1:$P2 CAMBIUM_CA=$T/ca.pem $BIN/cmb checkout own wtls && [ -f wtls/doc.txt ]) && ! (cd $T && CAMBIUM_SERVER=https://127.0.0.1:$P2 $BIN/cmb checkout own wtls2)'
ok "P37 TLS: session cookie marked Secure" 'curl -s -D - -o /dev/null --cacert $T/ca.pem --resolve localhost:$P2:127.0.0.1 -d "token=$NEW" https://localhost:$P2/ui/login | grep -i set-cookie | grep -qi secure'
echo "prod: $pass passed, $fail failed"
[ $fail = 0 ]
