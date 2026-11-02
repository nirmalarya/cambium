#!/usr/bin/env bash
# Container smoke test: the built image serves the API, review UI, search,
# protection and import-git. Local only (host network, loopback port).
set -u
IMG=${IMG:-cambium:dev}; TOOLS_IMG=${TOOLS_IMG:-${IMG%:*}:tools}; PORT=${PORT:-7880}; B=${BIN:-${CARGO_TARGET_DIR:-$PWD/target}/debug}
S=$(mktemp -d); chmod 755 $S; mkdir $S/g
(cd $S/g && git init -q -b main . && git -c user.name=S -c user.email=s@example.invalid commit -q --allow-empty -m e \
  && echo hi > h.txt && git add h.txt && git -c user.name=S -c user.email=s@example.invalid commit -qm h); chmod -R a+rX $S/g
sudo docker rm -f cambium-smoke >/dev/null 2>&1
sudo docker run -d --name cambium-smoke --network host --read-only --cap-drop ALL --security-opt no-new-privileges -e CAMBIUM_ADDR=127.0.0.1:$PORT -v $S/g:/src:ro $IMG >/dev/null
trap 'sudo docker rm -f cambium-smoke >/dev/null; rm -rf $S' EXIT
export CAMBIUM_SERVER=http://127.0.0.1:$PORT CAMBIUM_CACHE=$S/c
for i in $(seq 50); do curl -sf $CAMBIUM_SERVER/v1/health >/dev/null && break; sleep 0.1; done
export CAMBIUM_TOKEN=$(sudo docker exec cambium-smoke cmbd token smoke)
pass=0; fail=0
r(){ if (eval "$2") >/dev/null 2>&1; then pass=$((pass+1)); echo "PASS $1"; else fail=$((fail+1)); echo "FAIL $1"; fi; }
cd $S
r "S1 health" 'curl -sf $CAMBIUM_SERVER/v1/health'
r "S2 init + commit" '$B/cmb init smoke w && cd w && echo "hello cambium" > a.txt && $B/cmb add a.txt && $B/cmb commit -m one'
r "S3 fresh checkout" '$B/cmb checkout smoke w2 && grep -q cambium w2/a.txt'
r "S4 search" 'cd w && $B/cmb search cambium | grep -q "a.txt:1:"'
r "S5 review UI requires sign-in" '[ "$(curl -s -o /dev/null -w %{http_code} $CAMBIUM_SERVER/ui/smoke)" = 303 ]'
r "S6 protect + direct commit refused" 'cd w && $B/cmb protect main 1 && echo x >> a.txt && $B/cmb add a.txt && ! $B/cmb commit -m direct'
r "S7 import-git via the tools image (one-time job, same volume)" 'sudo docker run --rm --network host --volumes-from cambium-smoke -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0=/src $TOOLS_IMG import-git /src imported smoke | grep -q "imported 2 commits"'
r "S8 imported repo checks out" '$B/cmb checkout imported w3 && grep -q hi w3/h.txt'
r "S9 runs as non-root (65532)" '[ "$(sudo docker inspect -f {{.Config.User}} $IMG)" = 65532:65532 ] && sudo docker top cambium-smoke | tail -1 | grep -q "^65532 "'
r "S10 read-only root filesystem, no shell in image" '[ "$(sudo docker inspect -f {{.HostConfig.ReadonlyRootfs}} cambium-smoke)" = true ] && ! sudo docker exec cambium-smoke /bin/sh -c true'
r "S11 probes and metrics" 'curl -sf $CAMBIUM_SERVER/healthz && curl -sf $CAMBIUM_SERVER/readyz && curl -sf $CAMBIUM_SERVER/metrics | grep -q cambium_http_requests_total'
r "S12 SBOM shipped in image" 'sudo docker cp cambium-smoke:/usr/share/cambium/sbom/cambium-server.cdx.json $S/sb.json && python3 -c "import json; assert json.load(open(\"$S/sb.json\"))[\"bomFormat\"]==\"CycloneDX\""'
r "S13 graceful stop (SIGTERM) exits 0" 'sudo docker stop -t 40 cambium-smoke && [ "$(sudo docker inspect -f {{.State.ExitCode}} cambium-smoke)" = 0 ]'
echo "docker smoke: $pass passed, $fail failed"; [ $fail = 0 ]
