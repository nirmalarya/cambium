#!/usr/bin/env bash
# End-to-end: real cmbd server + cmb client, every command in the vertical slice.
set -u
BIN=${BIN:-${CARGO_TARGET_DIR:-$PWD/target}/debug}
T=$(mktemp -d); export CAMBIUM_DATA=$T/data CAMBIUM_ADDR=127.0.0.1:${PORT:-7979} CAMBIUM_CACHE=$T/cache
export CAMBIUM_SERVER=http://$CAMBIUM_ADDR
pass=0; fail=0
# portable helpers (GNU/Linux and macOS/BSD)
fmode(){ stat -c %a "$1" 2>/dev/null || python3 -c 'import os,sys; print(format(os.stat(sys.argv[1]).st_mode & 0o7777, "o"))' "$1"; }
fsize(){ wc -c < "$1" | tr -d ' '; }
ok(){ if eval "$2" >/dev/null 2>&1; then pass=$((pass+1)); echo "PASS $1"; else fail=$((fail+1)); echo "FAIL $1"; fi; }
export CAMBIUM_TOKEN=$($BIN/cmbd token alice)
$BIN/cmbd & SRV=$!; trap 'kill $SRV; rm -rf $T' EXIT
for i in $(seq 50); do curl -sf $CAMBIUM_SERVER/v1/health >/dev/null && break; sleep 0.1; done
cmb(){ $BIN/cmb "$@"; }
cd $T
ok "F01 unauthenticated request rejected" '[ "$(curl -s -o /dev/null -w %{http_code} $CAMBIUM_SERVER/v1/repos/x/refs)" = 401 ]'
ok "F01 bad token rejected" '! CAMBIUM_TOKEN=cmbt_bad cmb checkout demo w0'
ok "F01 init" 'cmb init demo wc'
cd wc
mkdir -p src docs; printf 'fn a() {}\nfn b() {}\n' > src/lib.rs; echo hello > docs/readme.md
ok "F02 status shows new files" 'cmb status | grep -q "new      src/lib.rs"'
ok "F02 add" 'cmb add src docs'
ok "F02 status shows staged" 'cmb status | grep -q "staged new"'
ok "F02 commit" 'cmb commit -m "first files"'
ok "F02 status clean after commit" 'cmb status | grep -q clean'
ok "F02 commit with nothing staged fails" '! cmb commit -m empty'
printf 'fn a() {}\nfn inserted() {}\nfn b() {}\n' > src/lib.rs
ok "F05 diff worktree" 'cmb diff | grep -q "^+fn inserted"'
cmb add src/lib.rs >/dev/null; cmb commit -m "insert fn" >/dev/null
ok "F05 log" '[ $(cmb log | wc -l) = 3 ]'
ok "F05 log path filter" '[ $(cmb log docs | wc -l) = 1 ]'
ok "F05 blame" 'cmb blame src/lib.rs | sed -n 2p | grep -q "$(cmb log -n 1 | cut -c1-12)"'
ok "F05 diff between revisions" 'cmb diff --from $(cmb log | sed -n 2p | cut -c1-12) --to main | grep -q inserted'
cmb checkout demo ../wc2 >/dev/null 2>&1
ok "F03 draft commit leaves main alone" 'echo feature > docs/feature.md && cmb add docs >/dev/null && cmb commit --draft -m "feature doc" | grep -q "^\[draft" && ! cmb cat demo main docs/feature.md >/dev/null 2>&1 && cmb status | grep -q "drafts not landed"'
REFBODY=$(printf '{"expected":null,"new":"%s"}' "$(cmb log -n 1 | cut -c1-12)")
ok "F03 only main and release refs can be created" '[ "$(curl -s -o /dev/null -w %{http_code} -H "Authorization: Bearer $CAMBIUM_TOKEN" -H content-type:application/json -d "$REFBODY" $CAMBIUM_SERVER/v1/repos/demo/refs/feat)" = 400 ]'
(cd ../wc2 && echo "main change" > docs/main.md && cmb add docs >/dev/null && cmb commit -m "main doc" >/dev/null)
ok "F04 sync rebases drafts onto the new main" 'cmb sync && [ -e docs/main.md ] && [ -e docs/feature.md ] && cmb status | grep -q "drafts not landed"'
ok "F04 land puts the drafts on main" 'cmb land | grep -q "^\[main" && cmb cat demo main docs/feature.md | grep -q feature && ! cmb status | grep -q drafts'
ok "F04 commit lands even after main moved (server rebases)" '(cd ../wc2 && cmb sync >/dev/null 2>&1 && echo q > docs/q.md && cmb add docs >/dev/null && cmb commit -m qq >/dev/null) && echo r > docs/r.md && cmb add docs >/dev/null && cmb commit -m rr | grep -q "^\[main" && [ -e docs/q.md ]'
ok "F03 release branch, fix, merge back into main" 'cmb branch 1 | grep -q "created release/1" && (cd ../wc2 && cmb sync >/dev/null 2>&1 && cmb switch release/1 >/dev/null 2>&1 && echo fix > docs/fix.md && cmb add docs >/dev/null && cmb commit -m fix >/dev/null && cmb switch main >/dev/null 2>&1) && echo m > docs/m.md && cmb add docs >/dev/null && cmb commit -m m >/dev/null && cmb merge release/1 && [ -e docs/fix.md ] && cmb log -n 1 | grep -q "(merge)"'
ok "F04 conflict keeps the change as a draft" '(cd ../wc2 && cmb sync >/dev/null 2>&1 && echo A > docs/readme.md && cmb add docs >/dev/null && cmb commit -m A >/dev/null) && echo B > docs/readme.md && cmb add docs >/dev/null && ! cmb commit -m B 2>$T/c.err && grep -q "kept as draft" $T/c.err && grep -q "conflicts with main" $T/c.err'
cd $T
ok "F06 sparse checkout fetches only prefix" 'cmb checkout demo sp --sparse src && [ -e sp/src/lib.rs ] && [ ! -e sp/docs ]'
ok "F06 sparse list" '(cd sp && cmb sparse | grep -q src)'
ok "F06 sparse widen" '(cd sp && cmb sparse add docs && [ -e docs/readme.md ])'
ok "F07 API-only read (no working copy)" 'cmb cat demo main src/lib.rs | grep -q inserted'
# Scion-style fetch: root tree -> changed trees -> blobs; unchanged directories are skipped
(cd $T && cmb init trees tr >/dev/null 2>&1 && cd tr && mkdir -p a b c && echo 1 > a/x && echo 2 > b/y && echo 3 > c/z && cmb add a b c >/dev/null && cmb commit -m dirs >/dev/null)
(cd $T && cmb checkout trees tr2 >/dev/null 2>&1)
(cd $T/tr && echo changed > a/x && cmb add a >/dev/null && cmb commit -m "only a" >/dev/null)
ok "T01 sync fetches only the changed trees (root + a), reuses b and c" '(cd $T/tr2 && CAMBIUM_STATS=1 cmb sync 2>&1 | grep -q "trees 2 fetched, 2 unchanged" && grep -q changed a/x)'
# ---------- pipelines: bisect and revert ----------
(cd $T && cmb init bis bs >/dev/null 2>&1 && cd bs && for n in $(seq 1 20); do if [ $n -ge 13 ]; then echo broken > status; else echo fine > status; fi; mkdir -p steps; echo $n > steps/$n; cmb add status steps >/dev/null 2>&1; cmb commit -m "step $n" >/dev/null; done)
GOOD=$(cd $T/bs && cmb log -n 20 | tail -1 | cut -c1-12)
ok "B01 cmb bisect --run finds the first bad changeset" '(cd $T/bs && cmb bisect $GOOD main --run "grep -qx fine status" 2>/dev/null | grep -q "first bad: .* step 13$" && [ -e steps/20 ])'
BADC=$(cd $T/bs && cmb log -n 8 | tail -1 | cut -c1-12)
ok "B02 cmb revert undoes it on top of later work" '(cd $T/bs && cmb log -n 8 | tail -1 | grep -q "step 13" && cmb revert $BADC | grep -q reverted && grep -qx fine status && [ ! -e steps/13 ] && [ -e steps/20 ])'
ok "F02 commit after main moved is rebased, not refused" '(cd sp && echo z > src/z.rs && cmb add src >/dev/null && (cd ../wc2 && cmb sync >/dev/null 2>&1; echo q2 > docs/q2.md && cmb add docs && cmb commit -m q2) >/dev/null 2>&1 && cmb commit -m z | grep -q "^\[main" && [ -e docs/q2.md ])'
ok "F08 cache bounded" 'CAMBIUM_CACHE_MAX=1 cmb checkout demo tiny >/dev/null 2>&1; [ $(ls $CAMBIUM_CACHE | wc -l) -le 1 ]'
HEAD_ID=$(curl -s -H "Authorization: Bearer $CAMBIUM_TOKEN" $CAMBIUM_SERVER/v1/repos/demo/resolve/main | python3 -c 'import sys,json; print(json.load(sys.stdin)["cs"])')
TRAV=$(printf '{"ref":"main","expected":"%s","message":"m","files":[{"path":"../evil","blob":null}]}' "$HEAD_ID")
ok "path traversal rejected by server" '[ "$(curl -s -o /dev/null -w %{http_code} -H "Authorization: Bearer $CAMBIUM_TOKEN" -H content-type:application/json -d "$TRAV" $CAMBIUM_SERVER/v1/repos/demo/commit)" = 400 ]'
cd $T
ok "F09 commit without a server fails clearly and keeps the change" '(cd sp && echo off > src/off.rs && cmb add src >/dev/null && out=$(CAMBIUM_SERVER=http://127.0.0.1:1 CAMBIUM_MAX_RETRIES=0 $BIN/cmb commit -m offline 2>&1); [ $? -ne 0 ] && echo "$out" | grep -q "cannot reach server" && [ -e src/off.rs ] && cmb status | grep -q off.rs)'
(cd wc2 && cmb sync >/dev/null 2>&1 && echo online > docs/on.md && cmb add docs >/dev/null && cmb commit -m online >/dev/null)
ok "F09 the same change commits once the server is back" '(cd sp && cmb sync >/dev/null 2>&1 && cmb commit -m offline >/dev/null && cmb log -n 2 | grep -q offline && cmb log -n 2 | grep -q online && cmb status | grep -q clean)'

# ---------- file modes ----------
cd $T
(cd wc2 && cmb sync >/dev/null 2>&1)
ok "M01 new executable shown in status" '(cd wc2 && printf "#!/bin/sh\necho hi\n" > run.sh && chmod +x run.sh && cmb status | grep -q "new      run.sh (executable)")'
ok "M01 commit executable, fresh checkout keeps +x" '(cd wc2 && cmb add run.sh && cmb commit -m "script") && cmb checkout demo mx >/dev/null 2>&1 && [ -x mx/run.sh ] && [ ! -x mx/src/lib.rs ]'
ok "M02 chmod -x shows as mode change in status" '(cd mx && chmod -x run.sh && cmb status | grep -q "mode     run.sh (100755 -> 100644)")'
ok "M02 chmod shows in worktree diff without content hunk" '(cd mx && cmb diff | grep -q "^mode run.sh: 100755 -> 100644" && ! cmb diff | grep -q "^@@")'
ok "M02 mode-only commit and revision diff" '(cd mx && cmb add run.sh && cmb commit -m "not exec" && cmb diff --from $(cmb log | sed -n 2p | cut -c1-12) --to main | grep -q "100755 -> 100644")'
ok "M03 sync applies mode change to other copy" '(cd wc2 && cmb sync && [ ! -x run.sh ])'
ok "M04 merge keeps exec bit from a release branch" '(cd mx && cmb branch xbit >/dev/null && cmb switch release/xbit >/dev/null 2>&1 && chmod +x run.sh && cmb add run.sh && cmb commit -m "exec again" && cmb switch main && echo extra > docs/extra.md && cmb add docs && cmb commit -m extra && cmb merge release/xbit && [ -x run.sh ])'
ok "M05 symlink committed and restored as a link" '(cd mx && ln -s docs/readme.md latest && cmb status | grep -q "latest (symlink)" && cmb add latest && cmb commit -m link) && cmb checkout demo ml >/dev/null 2>&1 && [ -L ml/latest ] && [ "$(readlink ml/latest)" = docs/readme.md ]'
ok "M05 retargeted symlink shows in diff" '(cd ml && ln -sfn src/lib.rs latest && cmb diff | grep -q "^+src/lib.rs")'
# ---------- resumable fetch ----------
head -c 300000 /dev/urandom | base64 > $T/big.txt
(cd wc2 && cmb sync >/dev/null 2>&1 && cp $T/big.txt big.txt && cmb add big.txt >/dev/null && cmb commit -m big >/dev/null)
ok "R01 server answers ranges (206 + Content-Range)" 'id=$(cmb log -n 1 >/dev/null; curl -s -H "Authorization: Bearer $CAMBIUM_TOKEN" -H content-type:application/json -d "{\"rev\":\"main\",\"prefixes\":[\"big.txt\"]}" $CAMBIUM_SERVER/v1/repos/demo/tree | python3 -c "import sys,json;print(json.load(sys.stdin)[\"entries\"][\"big.txt\"].split(\":\")[0])"); curl -s -D - -o /dev/null -H "Authorization: Bearer $CAMBIUM_TOKEN" -H "Range: bytes=100-" $CAMBIUM_SERVER/v1/blobs/$id | grep -qi "content-range: bytes 100-"'
export RC=$T/rcache
ok "R02 interrupted fetch leaves a partial and exits" '! CAMBIUM_CACHE=$RC CAMBIUM_FETCH_CHUNK=65536 CAMBIUM_FETCH_ABORT_AFTER=2 cmb cat demo main big.txt >/dev/null 2>$T/r2.err && grep -q "interrupted at byte 131072" $T/r2.err && [ $(fsize $RC-partial/*) = 131072 ]'
ok "R03 rerun resumes at the saved byte and content matches" 'CAMBIUM_CACHE=$RC CAMBIUM_FETCH_CHUNK=65536 cmb cat demo main big.txt 2>$T/r3.err | cmp - $T/big.txt && grep -q "resuming blob .* at byte 131072" $T/r3.err && [ -z "$(ls $RC-partial)" ]'
ok "R04 checkout resumes an interrupted large file" '! CAMBIUM_CACHE=$T/rc2 CAMBIUM_FETCH_CHUNK=50000 CAMBIUM_FETCH_ABORT_AFTER=3 cmb checkout demo rco >/dev/null 2>&1; rm -rf rco; CAMBIUM_CACHE=$T/rc2 CAMBIUM_FETCH_CHUNK=50000 cmb checkout demo rco 2>&1 | grep -q resuming && cmp rco/big.txt $T/big.txt'
# ---------- rate limiting (second server with a tight limit) ----------
RL=127.0.0.1:$(( ${PORT:-7979} + 1 ))
CAMBIUM_ADDR=$RL CAMBIUM_RATE=1 CAMBIUM_BURST=2 $BIN/cmbd 2>/dev/null & SRV2=$!; trap 'kill $SRV $SRV2; rm -rf $T' EXIT
for i in $(seq 50); do curl -sf http://$RL/v1/health >/dev/null && break; sleep 0.1; done
H="Authorization: Bearer $CAMBIUM_TOKEN"
ok "L01 over the limit returns 429 with Retry-After" 'for i in 1 2 3 4; do curl -s -o /dev/null -H "$H" http://$RL/v1/repos/demo/refs; done; curl -s -D - -o /dev/null -H "$H" http://$RL/v1/repos/demo/refs | grep -qi "^retry-after: [0-9]" && [ "$(curl -s -o /dev/null -w %{http_code} -H "$H" http://$RL/v1/repos/demo/refs)" = 429 ]'
ok "L02 limit is per token" 'T2=$(CAMBIUM_DATA=$CAMBIUM_DATA $BIN/cmbd token bob); [ "$(curl -s -o /dev/null -w %{http_code} -H "Authorization: Bearer $T2" http://$RL/v1/repos/demo/refs)" = 200 ]'
ok "L03 client backs off and succeeds" '(cd wc2 && CAMBIUM_SERVER=http://$RL bash -c "for i in 1 2 3 4; do $BIN/cmb log -n 1 || exit 1; done" 2>$T/l3.err >/dev/null) && grep -q "rate limited by server; retrying in" $T/l3.err'
ok "L04 client gives up after CAMBIUM_MAX_RETRIES" 'for i in 1 2 3; do curl -s -o /dev/null -H "$H" http://$RL/v1/repos/demo/refs; done; ! (cd wc2 && CAMBIUM_SERVER=http://$RL CAMBIUM_MAX_RETRIES=0 $BIN/cmb log -n 1)'

# ---------- identity + per-directory permissions ----------
cd $T
BOB=$($BIN/cmbd token bob --groups eng); CAROL=$($BIN/cmbd token carol --groups eng,payments)
ok "A01 whoami shows user and groups" '[ "$(CAMBIUM_TOKEN=$CAROL cmb whoami)" = "carol (groups: eng, payments)" ]'
ok "A02 token helper command (CAMBIUM_TOKEN_CMD)" '[ "$(CAMBIUM_TOKEN= CAMBIUM_TOKEN_CMD="echo $BOB" cmb whoami)" = "bob (groups: eng)" ]'
ok "A03 open repo by default" '(cmb init sec sec >/dev/null 2>&1 && cd sec && mkdir -p pub payments && echo p > pub/p.txt && echo s > payments/s.txt && cmb add pub payments && cmb commit -m seed && cmb acl | grep -q "no rules")'
ok "A04 non-owner cannot lock an open repo" '(cd sec && ! CAMBIUM_TOKEN=$BOB cmb acl set / user:bob admin)'
ok "A05 owner sets rules" '(cd sec && cmb acl set / user:alice admin && cmb acl set / group:eng read && cmb acl set payments group:eng none && cmb acl set payments group:payments write && cmb acl | grep -q "group:payments")'
ok "A06 reader checkout hides denied directory" 'CAMBIUM_TOKEN=$BOB cmb checkout sec bobwc >/dev/null 2>&1 && [ -e bobwc/pub/p.txt ] && [ ! -e bobwc/payments ]'
ok "A07 reader commit refused with 403 hint" '(cd bobwc && echo x >> pub/p.txt && CAMBIUM_TOKEN=$BOB cmb add pub && CAMBIUM_TOKEN=$BOB cmb commit -m nope 2>&1 | grep -q "forbidden: write on pub/p.txt" )'
ok "A08 group grant allows writes in its directory only" 'CAMBIUM_TOKEN=$CAROL cmb checkout sec carolwc >/dev/null 2>&1 && (cd carolwc && echo t >> payments/s.txt && CAMBIUM_TOKEN=$CAROL cmb add payments && CAMBIUM_TOKEN=$CAROL cmb commit -m pay && echo t >> pub/p.txt && CAMBIUM_TOKEN=$CAROL cmb add pub && ! CAMBIUM_TOKEN=$CAROL cmb commit -m pub)'
ok "A09 hidden file unreadable through API" '! CAMBIUM_TOKEN=$BOB cmb cat sec main payments/s.txt 2>/dev/null'
ok "A10 non-admin cannot read or change rules" '(cd bobwc && ! CAMBIUM_TOKEN=$BOB cmb acl && ! CAMBIUM_TOKEN=$BOB cmb acl set pub user:bob write)'

# ---------- code review + protected branches (CLI and HTML UI) ----------
cd $T
cmb init rev rv >/dev/null 2>&1; cd rv; echo base > a.txt; cmb add a.txt >/dev/null; cmb commit -m base >/dev/null
ok "W01 owner protects main (1 approval)" 'cmb protect main 1 && cmb protect | grep -q "main .*1 approval"'
echo direct >> a.txt; cmb add a.txt >/dev/null
ok "W02 commit to protected main is kept as a draft" '! cmb commit -m direct 2>$T/w2 && grep -q "only changes through an approved review" $T/w2 && cmb status | grep -q "drafts not landed"'
ok "W03 non-admin cannot change protection" '! CAMBIUM_TOKEN=$BOB cmb protect main 0'
ok "W04 review opened from the drafts" 'cmb review create -m "<script>alert(1)</script> add line" | grep -q "review #1 opened: [0-9a-f]* -> main"'
ok "W05 author cannot approve own review" 'cmb review approve 1 2>&1 | grep -q "cannot approve their own"'
ok "W06 merge blocked until approved" 'cmb review merge 1 2>&1 | grep -q "needs 1 approval"'
U=$CAMBIUM_SERVER/ui
ok "W07 UI requires sign-in" '[ "$(curl -s -o /dev/null -w %{http_code} $U/rev)" = 303 ] && curl -s -D - -o /dev/null $U/rev | grep -qi "^location: /ui/login"'
ok "W08 UI rejects a bad token" '[ "$(curl -s -o /dev/null -w %{http_code} -d token=cmbt_nope $U/login)" = 401 ]'
curl -s -D $T/h -o /dev/null -d "token=$BOB" $U/login
CK="cambium_session=$(grep -i '^set-cookie: cambium_session=' $T/h | sed 's/.*cambium_session=\([^;]*\).*/\1/' | tr -d '\r')"
ok "W09 UI sign-in sets an HttpOnly SameSite=Strict cookie" 'grep -i "^set-cookie: cambium_session=" $T/h | grep -qi "HttpOnly" && grep -i "^set-cookie" $T/h | grep -qi "SameSite=Strict"'
curl -s -b "$CK" $U/rev/reviews/1 > $T/rv.html
ok "W10 UI review page: diff, escaped title, approvals 0/1" 'grep -q "class=\"add\">+direct" $T/rv.html && grep -q "&lt;script&gt;alert(1)&lt;/script&gt;" $T/rv.html && ! grep -q "<script>alert" $T/rv.html && grep -q "<b>0/1</b>" $T/rv.html'
ok "W11 UI lists reviews for the repo" 'curl -s -b "$CK" $U/rev | grep -q "/ui/rev/reviews/1"'
CSRF=$(grep -o 'name=csrf value="[0-9a-f]*"' $T/rv.html | head -1 | grep -o '[0-9a-f]\{32\}')
ok "W12 UI action without CSRF value refused" '[ "$(curl -s -o /dev/null -w %{http_code} -b "$CK" -d csrf=bad $U/rev/reviews/1/approve)" = 403 ]'
ok "W13 UI comment" '[ "$(curl -s -o /dev/null -w %{http_code} -b "$CK" --data-urlencode "csrf=$CSRF" --data-urlencode "body=nice <i>work</i>" $U/rev/reviews/1/comments)" = 303 ] && curl -s -b "$CK" $U/rev/reviews/1 | grep -q "nice &lt;i&gt;work&lt;/i&gt;"'
ok "W14 UI approve" '[ "$(curl -s -o /dev/null -w %{http_code} -b "$CK" -d csrf=$CSRF $U/rev/reviews/1/approve)" = 303 ] && curl -s -b "$CK" $U/rev/reviews/1 | grep -q "<b>1/1</b>"'
ok "W15 UI merge lands the review on main" '[ "$(curl -s -o /dev/null -w %{http_code} -b "$CK" -d csrf=$CSRF $U/rev/reviews/1/merge)" = 303 ] && cmb review list | grep -q "^#1 *merged" && cmb sync >/dev/null 2>&1 && grep -q direct a.txt && ! cmb status | grep -q drafts'
ok "W16 review show (CLI) includes the comment" 'cmb review show 1 | grep -q "bob: nice"'
ok "W17 UI cookie holds a server-side session id, never the API token" 'echo "$CK" | grep -q "^cambium_session=cmbs_" && ! grep -qi "$BOB" $T/h'
# semantic review rules: docs/comment-only changes may need fewer approvals
(cd $T && cmb init sem sm >/dev/null 2>&1 && cd sm && printf "fn f() -> u8 { 1 }\n" > lib.rs && echo hi > README.md && cmb add lib.rs README.md >/dev/null && cmb commit -m base >/dev/null && cmb protect main 2 --trivial 1 >/dev/null)
ok "W30 protection lists the docs/comment-only rule" '(cd $T/sm && cmb protect | grep -q "main .*2 approval(s) (1 for docs/comment-only changes)")'
ok "W31 comment-only review needs 1 approval, logic needs 2" '(cd $T/sm && printf "fn f() -> u8 {\n    1 // one\n}\n" > lib.rs && cmb add lib.rs >/dev/null && cmb commit --draft -m cmt >/dev/null && N=$(cmb review create -m comments | sed -n "s/^review #\([0-9]*\).*/\1/p") && cmb review list | grep -q "^#$N .*comments *0/1" && printf "fn f() -> u8 { 2 }\n" > lib.rs && cmb add lib.rs >/dev/null && cmb commit --draft -m logic >/dev/null && cmb review update $N >/dev/null && cmb review list | grep -q "^#$N .*logic *0/2")'
ok "W18 UI sends security headers" 'curl -s -D - -o /dev/null -b "$CK" $U/rev | tr -d "\r" | grep -qi "^content-security-policy: default-src .none." && curl -s -D - -o /dev/null $U/login | grep -qi "^x-frame-options: DENY"'
ok "W19 sign-out ends the session server-side" 'curl -s -o /dev/null -b "$CK" -X POST $U/logout && [ "$(curl -s -o /dev/null -w %{http_code} -b "$CK" $U/rev)" = 303 ]'

# ---------- code search ----------
(cd $T/sec && cmb switch main >/dev/null 2>&1; echo "quarterly Ledger totals" > payments/ledger.txt && echo "see ledger notes" > pub/notes.txt && cmb add payments pub >/dev/null && cmb commit -m ledger >/dev/null)
ok "X01 search finds path and content matches" '(cd $T/sec && cmb search ledger | sort | tr "\n" "|" | grep -q "^payments/ledger.txt (path)|payments/ledger.txt:1: quarterly Ledger totals|pub/notes.txt:1: see ledger notes|$")'
ok "X02 search respects directory permissions" '(cd $T/bobwc && CAMBIUM_TOKEN=$BOB cmb search ledger --rev main | grep -q "pub/notes.txt" && ! CAMBIUM_TOKEN=$BOB cmb search ledger --rev main | grep -q payments)'
ok "X03 search: no hits is empty, not an error" '(cd $T/sec && [ -z "$(cmb search zzqqxx)" ])'
ok "X04 diff reports renames by content" '(cd $T/sec && P=$(cmb log -n 1 | cut -d" " -f1) && mv pub/notes.txt pub/renamed.txt && cmb add pub >/dev/null && cmb commit -m mv >/dev/null && cmb diff --from $P --to main | grep -qx "rename pub/notes.txt -> pub/renamed.txt")'

# ---------- shared per-machine object cache ----------
SC=$T/machine-cache
ok "Y01 first checkout fills the shared cache from the server" '(cd $T && CAMBIUM_SHARED_CACHE=$SC CAMBIUM_STATS=1 cmb checkout rev sc1 2>&1 | grep -q "cambium-stats: 0 from cache, [1-9][0-9]* from server")'
ok "Y02 another user on the machine reuses it (no server fetches)" '(cd $T && CAMBIUM_TOKEN=$BOB CAMBIUM_CACHE=$T/bobcache CAMBIUM_SHARED_CACHE=$SC CAMBIUM_STATS=1 cmb checkout rev sc2 2>&1 | grep -q "cambium-stats: [1-9][0-9]* from cache, 0 from server") && diff -r --exclude=.cambium $T/sc1 $T/sc2'
ok "Y03 shared cache dir is group-shared (setgid, no world access)" '[ "$(fmode $SC)" = 2770 ]'
F=$(ls $SC | head -1); echo tampered > $SC/$F
ok "Y04 tampered entry is detected and refetched" '(cd $T && CAMBIUM_SHARED_CACHE=$SC CAMBIUM_STATS=1 cmb checkout rev sc3 2>&1 | grep -q "failed integrity check; refetching") && diff -r --exclude=.cambium $T/sc1 $T/sc3'

# ---------- filesystem monitor (cmb watch) ----------
cd $T; cmb init wat wat >/dev/null 2>&1; cd wat; echo a > a.txt; mkdir -p deep/er; echo x > deep/er/x.txt; cmb add a.txt deep >/dev/null; cmb commit -m deep >/dev/null
$BIN/cmb watch >/dev/null 2>&1 & WP=$!; trap 'kill $SRV $SRV2 $WP 2>/dev/null; rm -rf $T' EXIT
for i in $(seq 50); do [ -s .cambium/watch.pid ] && break; sleep 0.1; done
ok "U01 first status with a live watcher does one full scan" 'CAMBIUM_STATS=1 cmb status 2>&1 | grep -q "full scan"'
echo changed >> a.txt; echo n > fresh.txt; rm deep/er/x.txt; sleep 0.5
ok "U02 later status checks only watched paths" 'CAMBIUM_STATS=1 cmb status 2>&1 | grep -q "watch: [0-9]* candidate"'
ok "U03 watcher sees modify, create and delete" 'cmb status > $T/ws && grep -q "modified a.txt" $T/ws && grep -q "new      fresh.txt" $T/ws && grep -q "deleted  deep/er/x.txt" $T/ws'
ok "U04 commit then clean, still via the watcher" 'cmb add a.txt fresh.txt deep >/dev/null 2>&1; cmb commit -m watched >/dev/null && CAMBIUM_STATS=1 cmb status 2>&1 | grep -q "watch:" && cmb status | grep -q clean'
kill $WP; wait $WP 2>/dev/null
ok "U05 dead watcher is ignored (back to full scans)" 'echo again >> a.txt && CAMBIUM_STATS=1 cmb status 2>&1 | grep -q "full scan" && cmb status | grep -q "modified a.txt"'
cd $T

# ---------- Git import with full history ----------
G=$T/gitsrc; mkdir -p $G && cd $G && git init -q -b main . && git config user.name "Ada Example" && git config user.email ada@example.invalid
mkdir -p lib && echo one > lib/a.txt && printf '#!/bin/sh\n' > tool.sh && chmod +x tool.sh && git add -A && git commit -qm one
git checkout -qb topic && echo two >> lib/a.txt && git commit -qam two && git checkout -q main && echo b > b.txt && git add b.txt && git commit -qm three && git merge -q --no-ff -m merged topic
cd $T
ok "I01 cmbd import-git imports every commit; refs only main and release/*" '$BIN/cmbd import-git $G gitimp alice 2>/dev/null > $T/imp.out; grep -q "imported 4 commits .* refs: main" $T/imp.out && ! grep -q "refs: main, topic" $T/imp.out'
ok "I02 imported tree matches the Git checkout" 'cmb checkout gitimp gi >/dev/null 2>&1 && diff -r --exclude=.cambium --exclude=.git $G $T/gi && [ -x $T/gi/tool.sh ]'
ok "I03 imported history and merge parents" '(cd $T/gi && cmb log | head -1 | grep -q "merged (merge)" && [ $(cmb log | wc -l) = 5 ])'
ok "I04 blame works on imported history" '(cd $T/gi && [ $(cmb blame lib/a.txt | cut -d" " -f1 | sort -u | wc -l) = 2 ])'

# ---------- regional read-through cache (cmbd edge) ----------
EP=${EDGE_PORT:-7994}; E=http://127.0.0.1:$EP
CAMBIUM_UPSTREAM=$CAMBIUM_SERVER CAMBIUM_ADDR=127.0.0.1:$EP CAMBIUM_DATA=$T/edge $BIN/cmbd edge 2>/dev/null & EDGE=$!; trap 'kill $SRV $SRV2 $EDGE 2>/dev/null; rm -rf $T' EXIT
for i in $(seq 50); do curl -sf $E/v1/health >/dev/null && break; sleep 0.1; done
LID=$(sha256sum $T/sec/payments/ledger.txt | cut -d" " -f1)
ok "Z01 checkout through the edge matches the primary" '(cd $T && CAMBIUM_SERVER=$E CAMBIUM_CACHE=$T/ec1 CAMBIUM_TOKEN=$CAROL cmb checkout sec e1 >/dev/null 2>&1 && diff -r --exclude=.cambium $T/sec $T/e1)'
ok "Z02 blob fetched once from the primary, then served locally" 'curl -s -D - -o /dev/null -H "Authorization: Bearer $CAROL" $E/v1/blobs/$LID | grep -qi "^x-cambium-edge: hit"'
ok "Z03 cached blob still checked against the primary ACL" '[ "$(curl -s -o /dev/null -w %{http_code} -H "Authorization: Bearer $BOB" $E/v1/blobs/$LID)" = 404 ] && [ "$(curl -s -o /dev/null -w %{http_code} $E/v1/blobs/$LID)" = 401 ]'
ok "Z04 ranges served from the edge copy" '[ "$(curl -s -H "Authorization: Bearer $CAROL" -H "Range: bytes=0-8" $E/v1/blobs/$LID)" = quarterly ]'
ok "Z05 writes pass through to the primary" '(cd $T/e1 && echo edge >> payments/ledger.txt && CAMBIUM_SERVER=$E CAMBIUM_TOKEN=$CAROL cmb add payments >/dev/null && CAMBIUM_SERVER=$E CAMBIUM_TOKEN=$CAROL cmb commit -m via-edge >/dev/null) && (cd $T && CAMBIUM_TOKEN=$CAROL cmb checkout sec e2 >/dev/null 2>&1 && grep -qx "edge" e2/payments/ledger.txt)'
ok "Z06 redirects are passed back, not followed" '[ "$(curl -s -o /dev/null -w %{http_code} $E/ui/sec)" = 303 ]'
# ---------- cache tiers: client -> cmbd daemon -> cmbd edge -> cmbd ----------
DP=$((EP+1)); D=http://127.0.0.1:$DP
CAMBIUM_UPSTREAM=$E CAMBIUM_ADDR=127.0.0.1:$DP CAMBIUM_DATA=$T/daemon $BIN/cmbd daemon 2>/dev/null & DAEMON=$!; trap 'kill $SRV $SRV2 $EDGE $DAEMON 2>/dev/null; rm -rf $T' EXIT
for i in $(seq 50); do curl -sf $D/v1/health >/dev/null && break; sleep 0.1; done
ok "Z07 checkout through daemon -> edge -> server matches" '(cd $T && CAMBIUM_SERVER=$D CAMBIUM_CACHE=$T/dc1 CAMBIUM_TOKEN=$CAROL cmb checkout sec d1 >/dev/null 2>&1 && diff -r --exclude=.cambium $T/e1 $T/d1)'
ROOTID=$(curl -s -H "Authorization: Bearer $CAROL" $D/v1/repos/sec/root/main | python3 -c 'import sys,json; print(json.load(sys.stdin)["root"])')
ok "Z08 trees and blobs are then served by the daemon tier" 'curl -s -D - -o /dev/null -H "Authorization: Bearer $CAROL" "$D/v1/repos/sec/trees/$ROOTID?path=" | grep -qi "^x-cambium-edge: hit" && curl -s -D - -o /dev/null -H "Authorization: Bearer $CAROL" $D/v1/blobs/$LID | tr -d "\r" | grep -qi "^x-cambium-tier: daemon" && curl -s -D - -o /dev/null -H "Authorization: Bearer $CAROL" $D/v1/blobs/$LID | grep -qi "^x-cambium-edge: hit"'
ok "Z09 permissions hold through every tier" '[ "$(curl -s -o /dev/null -w %{http_code} -H "Authorization: Bearer $BOB" $D/v1/blobs/$LID)" = 404 ] && [ "$(curl -s -o /dev/null -w %{http_code} $D/v1/blobs/$LID)" = 401 ] && ! curl -s -H "Authorization: Bearer $BOB" "$D/v1/repos/sec/trees/$ROOTID?path=" | grep -q payments'

# ---------- virtual checkout (FUSE lazy mount) ----------
if [ -w /dev/fuse ] && command -v fusermount3 >/dev/null; then
  cd $T; MC=$T/mcache
  CAMBIUM_CACHE=$MC cmb mount sec vmnt >$T/mount.log 2>&1 & MP=$!
  for i in $(seq 50); do [ -e vmnt/pub/p.txt ] && break; sleep 0.1; done
  ok "V01 mount lists the tree without downloading files" '[ -e vmnt/pub/p.txt ] && [ "$(ls -A $MC 2>/dev/null | wc -l)" = 0 ] && [ "$(fsize vmnt/pub/p.txt)" = "$(CAMBIUM_TOKEN=$CAMBIUM_TOKEN cmb cat sec main pub/p.txt | wc -c)" ]'
  ok "V02 first read fetches just that file" 'grep -q "^p" vmnt/pub/p.txt && [ "$(ls -A $MC | wc -l)" = 1 ]'
  ok "V03 mount is read-only" '! touch vmnt/new 2>/dev/null && ! sh -c "echo x > vmnt/pub/p.txt" 2>/dev/null'
  fusermount3 -u vmnt 2>/dev/null || { kill $MP; fusermount3 -uz vmnt; }; wait $MP 2>/dev/null
  CAMBIUM_TOKEN=$BOB CAMBIUM_CACHE=$T/mcache2 cmb mount sec bmnt >/dev/null 2>&1 & MP=$!
  for i in $(seq 50); do [ -e bmnt/pub ] && break; sleep 0.1; done
  ok "V04 mount honours per-directory permissions" '[ -e bmnt/pub/p.txt ] && [ ! -e bmnt/payments ]'
  fusermount3 -u bmnt 2>/dev/null || { kill $MP; fusermount3 -uz bmnt; }; wait $MP 2>/dev/null
  # writable copy-on-write mount on repo "wat" (a.txt, deep/er/x.txt)
  export CAMBIUM_CACHE=$T/wcache
  A0=$(CAMBIUM_CACHE=$T/c0 cmb cat wat main a.txt)
  cmb mount wat wmnt --write >$T/wmount.log 2>&1 & MP=$!
  for i in $(seq 50); do [ -e wmnt/a.txt ] && break; sleep 0.1; done
  ok "V05 writable mount: edit, create, delete, chmod, mkdir, rename stay local" '(cd wmnt && echo more >> a.txt && echo n > new.txt && mkdir -p sub && echo s > sub/s.txt && printf "#!/bin/sh\n" > run.sh && chmod +x run.sh && mv new.txt renamed.txt && rm fresh.txt) && [ "$(CAMBIUM_CACHE=$T/c0 cmb cat wat main a.txt)" = "$A0" ] && [ ! -e wmnt/new.txt ] && [ "$(tail -1 wmnt/a.txt)" = more ]'
  ok "V06 status inside the mount sees exactly the overlay changes" '(cd wmnt && cmb status | tail -n +2 | awk "{print \$1, \$2}" | sort | tr "\n" "|" | grep -qx "deleted fresh.txt|modified a.txt|new renamed.txt|new run.sh|new sub/s.txt|")'
  ok "V07 unchanged files were never downloaded" '[ "$(ls -A $T/wcache 2>/dev/null | wc -l)" -le 1 ]'
  ok "V08 commit inside the mount goes through the normal API" '(cd wmnt && cmb add . >/dev/null && cmb commit -m "from mount" >/dev/null && [ -z "$(cmb status)" ]) && [ "$(CAMBIUM_CACHE=$T/c0 cmb cat wat main a.txt | tail -1)" = more ] && [ "$(CAMBIUM_CACHE=$T/c0 cmb cat wat main sub/s.txt)" = s ] && ! CAMBIUM_CACHE=$T/c0 cmb cat wat main fresh.txt 2>/dev/null && CAMBIUM_CACHE=$T/c0 cmb log wat 2>/dev/null | head -1 | grep -q "from mount" || (cd wmnt && cmb log -n 1 | grep -q "from mount")'
  ok "V09 executable bit and rename committed" '(cd $T && CAMBIUM_CACHE=$T/c0 cmb checkout wat wchk >/dev/null 2>&1 && [ -x wchk/run.sh ] && [ -f wchk/renamed.txt ] && [ ! -e wchk/new.txt ])'
  echo pending > wmnt/unsaved.txt
  fusermount3 -u wmnt 2>/dev/null || { kill $MP; fusermount3 -uz wmnt; }; wait $MP 2>/dev/null
  ok "V10 unmount keeps uncommitted work as a salvage copy" 'grep -q "uncommitted changes" $T/wmount.log && find $T/wcache-mounts -path "*salvage/unsaved.txt" | grep -q . && ! find $T/wcache-mounts -path "*salvage/a.txt" | grep -q .'
  unset CAMBIUM_CACHE; export CAMBIUM_CACHE=$T/cache
else
  echo "SKIP V01-V10: /dev/fuse or fusermount3 not available"
fi
echo "e2e: $pass passed, $fail failed"
[ $fail = 0 ]
