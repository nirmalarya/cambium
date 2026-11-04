#!/usr/bin/env bash
# Install the Helm chart on a throwaway kind cluster and exercise it for real:
# migrations, a commit/checkout round trip from inside the cluster, the
# NetworkPolicy, losing a pod under traffic, a rolling upgrade under traffic.
#
# The cluster gets its own name and its own kubeconfig file, so other clusters
# and your current kubectl context are never touched.
#   CLUSTER=cambium-test   cluster name
#   SKIP_BUILD=1           reuse the images already built
#   KEEP=1                 leave the cluster running afterwards (default: delete it)
set -u
cd "$(dirname "$0")/.."
CLUSTER=${CLUSTER:-cambium-test}
export KUBECONFIG=/tmp/$CLUSTER.kubeconfig
pass=0; fail=0
ok(){ if eval "$2" >/dev/null 2>&1; then pass=$((pass+1)); echo "PASS $1"; else fail=$((fail+1)); echo "FAIL $1"; fi; }
k(){ kubectl "$@"; }

kind get clusters 2>/dev/null | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" --kubeconfig "$KUBECONFIG" --wait 180s
kind export kubeconfig --name "$CLUSTER" --kubeconfig "$KUBECONFIG" >/dev/null 2>&1
if [ -z "${SKIP_BUILD:-}" ]; then
  docker build -q -t cambium-cmbd:kindtest --target runtime . >/dev/null
  docker build -q -t cambium-tools:kindtest --target tools . >/dev/null
fi
kind load docker-image --name "$CLUSTER" cambium-cmbd:kindtest cambium-tools:kindtest >/dev/null

k apply -f tests/k8s/deps.yaml >/dev/null
k -n cambium rollout status deploy/postgres deploy/rustfs --timeout=300s >/dev/null
ok "K20 helm install (migration Job first, then 2 ready pods)" \
  'helm upgrade --install cambium deploy/helm/cambium -n cambium -f tests/k8s/kind-values.yaml --wait --timeout 5m && [ "$(k -n cambium get deploy cambium -o jsonpath={.status.readyReplicas})" = 2 ]'

# a client inside the cluster, in the namespace the NetworkPolicy admits
k -n cambium-client delete pod client --ignore-not-found --wait >/dev/null
DB=$(k -n cambium get secret cambium-secrets -o jsonpath='{.data.CAMBIUM_DB}' | base64 -d)
k -n cambium-client run client --image=cambium-tools:kindtest --image-pull-policy=Never --restart=Never \
  --env=CAMBIUM_SERVER=http://cambium.cambium.svc:7878 --env=CAMBIUM_ALLOW_HTTP=1 --env=HOME=/tmp \
  --env=CAMBIUM_CACHE=/tmp/cache --env=CAMBIUM_DATA=/tmp/d --env="CAMBIUM_DB=$DB" \
  --command -- sleep 3600 >/dev/null
k -n cambium-client wait --for=condition=Ready pod/client --timeout=120s >/dev/null
TOKEN=$(k -n cambium-client exec client -- cmbd token alice 2>/dev/null | tail -1)
c(){ k -n cambium-client exec client -- env CAMBIUM_TOKEN="$TOKEN" bash -c "$1"; }

ok "K21 commit from inside the cluster, fresh checkout matches" \
  'c "cd /tmp && cmb init demo wc && cd wc && mkdir -p src && echo hello > src/a.txt && cmb add src && cmb commit -m first && cd /tmp && cmb checkout demo wc2 && diff -r --exclude=.cambium wc wc2"'
ok "K22 NetworkPolicy admits the client namespace" 'c "timeout 5 bash -c \"</dev/tcp/cambium.cambium.svc/7878\""'
k -n default delete pod probe --ignore-not-found --wait >/dev/null
# from another namespace: Postgres (no policy) must be reachable, cmbd must not be
PROBE=$(k -n default run probe --image=cambium-tools:kindtest --image-pull-policy=Never --restart=Never --rm -i --command -- bash -c \
  'sleep 3; t(){ timeout 5 bash -c "</dev/tcp/$1.cambium.svc.cluster.local/$2"; echo $1=$?; }; t postgres 5432; t cambium 7878; t cambium-edge 7879; t cambium-daemon 7880' 2>/dev/null)
ok "K22 NetworkPolicy blocks other namespaces (Postgres reachable from there; cmbd, edge, daemon not)" \
  'echo "$PROBE" | grep -q "^postgres=0" && echo "$PROBE" | grep -q "^cambium=124" && echo "$PROBE" | grep -q "^cambium-edge=124" && echo "$PROBE" | grep -q "^cambium-daemon=124"'

# cache tiers: checkout through daemon (this node) -> edge -> cmbd, then a repeat
# fetch of the same blob is answered by the daemon from its cache
ok "K27 checkout through daemon -> edge -> cmbd matches" \
  'c "cd /tmp && CAMBIUM_SERVER=http://cambium-daemon.cambium.svc:7880 CAMBIUM_CACHE=/tmp/cache-tiers cmb checkout demo wc3 && diff -r --exclude=.cambium wc wc3"'
BLOB=$(printf 'hello\n' | shasum -a 256 | cut -c1-64)
raw(){ c "exec 3<>/dev/tcp/cambium-daemon.cambium.svc/7880; printf 'GET /v1/blobs/$BLOB HTTP/1.1\r\nHost: d\r\nAuthorization: Bearer %s\r\nConnection: close\r\n\r\n' \"\$CAMBIUM_TOKEN\" >&3; tr -d '\r' <&3 | sed -n '1,/^\$/p'"; }
raw >/dev/null
HDRS=$(raw)
ok "K28 repeat blob fetch is a cache hit on the node's daemon" 'echo "$HDRS" | grep -q "^HTTP/1.1 200" && echo "$HDRS" | grep -qi "^x-cambium-tier: daemon" && echo "$HDRS" | grep -qi "^x-cambium-edge: hit"'
ok "K28 tiers are ready and report their own health" \
  '[ "$(k -n cambium get ds cambium-daemon -o jsonpath={.status.numberReady})" -ge 1 ] && [ "$(k -n cambium get deploy cambium-edge -o jsonpath={.status.readyReplicas})" = 2 ]'

# traffic: commit continuously (a commit takes ~50 ms) until told to stop, so the
# disruption is guaranteed to happen mid-traffic; then every attempt must have landed
start_load(){ # name
  c "rm -f /tmp/$1.stop /tmp/$1.out /tmp/$1.err; cd /tmp/wc && n=0 ok=0; while [ ! -e /tmp/$1.stop ]; do n=\$((n+1)); echo $1-\$n > src/$1-\$n.txt && cmb add src >/dev/null && cmb commit -m $1-\$n >/dev/null 2>>/tmp/$1.err && ok=\$((ok+1)); done; echo \$n \$ok > /tmp/$1.out" &
  sleep 3
}
stop_load(){ # name -> "attempts landed"
  sleep 3; c "touch /tmp/$1.stop"; wait; c "cat /tmp/$1.out"
}
landed_all(){ set -- $1; [ "$1" -gt 20 ] && [ "$1" = "$2" ]; }

start_load kill
k -n cambium delete pod "$(k -n cambium get pod -l app.kubernetes.io/name=cambium -o jsonpath='{.items[0].metadata.name}')" --wait=true >/dev/null
k -n cambium rollout status deploy/cambium --timeout=180s >/dev/null
R=$(stop_load kill)
ok "K23 every commit lands while a server pod is deleted and replaced ($R: attempts landed)" 'landed_all "$R"'

start_load upgrade
ok "K24 helm upgrade under traffic (new config, migration Job revision 2)" \
  'helm upgrade cambium deploy/helm/cambium -n cambium -f tests/k8s/kind-values.yaml --set config.CAMBIUM_LOG=debug --wait --timeout 5m'
R=$(stop_load upgrade)
ok "K24 every commit lands during the rolling upgrade ($R: attempts landed)" 'landed_all "$R"'
TOTAL=$(c "cat /tmp/kill.out /tmp/upgrade.out" | awk '{s+=$2} END {print s}')

k -n cambium rollout restart deploy/cambium >/dev/null && k -n cambium rollout status deploy/cambium --timeout=180s >/dev/null
ok "K25 history intact after restarting every pod (root + 1 + $TOTAL commits)" 'c "cd /tmp/wc && cmb sync && [ \$(cmb log -n 100000 | wc -l) = $((TOTAL + 2)) ]"'
ok "K26 pods run as the non-root user with a read-only root filesystem" \
  '[ "$(k -n cambium get pod -l app.kubernetes.io/name=cambium -o jsonpath="{.items[0].spec.securityContext.runAsUser}{.items[0].spec.containers[0].securityContext.readOnlyRootFilesystem}")" = 65532true ]'

if [ "$fail" -gt 0 ]; then
  echo "--- client errors"; c "cat /tmp/*.err 2>/dev/null | sort | uniq -c | head -20" || true
  echo "--- server logs (last lines)"; k -n cambium logs -l app.kubernetes.io/name=cambium --tail=15 --prefix 2>/dev/null | tail -30
fi
[ -n "${KEEP:-}" ] || kind delete cluster --name "$CLUSTER" --kubeconfig "$KUBECONFIG" >/dev/null 2>&1
echo "k8s-install: $pass passed, $fail failed"
[ "$fail" = 0 ]
