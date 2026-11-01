#!/usr/bin/env bash
# Static validation of the Helm chart: lint, render, schema-validate with
# kubeconform for several Kubernetes versions, then policy assertions on the
# rendered objects. (No cluster is contacted.)
set -u
C=deploy/helm/cambium; T=$(mktemp -d); pass=0; fail=0
ok(){ if eval "$2" >/dev/null 2>&1; then pass=$((pass+1)); echo "PASS $1"; else fail=$((fail+1)); echo "FAIL $1"; fi; }
ok "K01 helm lint --strict (default + all options)" 'helm lint --strict $C && helm lint --strict $C --set ingress.enabled=true,ingress.tlsSecretName=t,podTLS.enabled=true,podTLS.secretName=p,serviceMonitor.enabled=true'
helm template cambium $C -n cambium > $T/d.yaml
helm template cambium $C -n cambium --set ingress.enabled=true,ingress.tlsSecretName=t,podTLS.enabled=true,podTLS.secretName=p > $T/f.yaml
for v in 1.28.0 1.29.0 1.30.0 1.31.0; do
  ok "K02 kubeconform -strict k8s $v" 'kubeconform -strict -summary -kubernetes-version '$v' $T/d.yaml $T/f.yaml | grep -q "Invalid: 0, Errors: 0"'
done
py(){ python3 - "$T/d.yaml" <<PY
import sys, yaml
docs=[d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
k={d['kind']:d for d in docs}
dep=k['Deployment']; pod=dep['spec']['template']['spec']; c=pod['containers'][0]
job=k['Job']['spec']['template']['spec']
$1
PY
}
ok "K03 non-root pod + container (Deployment and migration Job)" 'py "assert pod[\"securityContext\"][\"runAsNonRoot\"] and pod[\"securityContext\"][\"runAsUser\"]==65532 and job[\"securityContext\"][\"runAsNonRoot\"]"'
ok "K04 read-only rootfs, no privilege escalation, all caps dropped" 'py "
for x in (c, job[\"containers\"][0]):
  s=x[\"securityContext\"]; assert s[\"readOnlyRootFilesystem\"] and not s[\"allowPrivilegeEscalation\"] and s[\"capabilities\"][\"drop\"]==[\"ALL\"]"'
ok "K05 seccomp RuntimeDefault, SA token not mounted" 'py "assert pod[\"securityContext\"][\"seccompProfile\"][\"type\"]==\"RuntimeDefault\" and pod[\"automountServiceAccountToken\"] is False"'
ok "K06 resource requests and limits" 'py "r=c[\"resources\"]; assert r[\"requests\"][\"cpu\"] and r[\"limits\"][\"memory\"] and job[\"containers\"][0][\"resources\"][\"limits\"]"'
ok "K07 startup, liveness (/healthz) and readiness (/readyz) probes" 'py "assert c[\"livenessProbe\"][\"httpGet\"][\"path\"]==\"/healthz\" and c[\"readinessProbe\"][\"httpGet\"][\"path\"]==\"/readyz\" and c[\"startupProbe\"]"'
ok "K08 PodDisruptionBudget selects the pods" 'py "p=k[\"PodDisruptionBudget\"][\"spec\"]; assert p[\"minAvailable\"]>=1 and p[\"selector\"][\"matchLabels\"]==dep[\"spec\"][\"selector\"][\"matchLabels\"]"'
ok "K09 NetworkPolicy restricts ingress and egress" 'py "n=k[\"NetworkPolicy\"][\"spec\"]; assert set(n[\"policyTypes\"])=={\"Ingress\",\"Egress\"} and n[\"ingress\"][0][\"from\"] and all(\"ipBlock\" not in f or f[\"ipBlock\"][\"cidr\"]!=\"0.0.0.0/0\" for e in n[\"egress\"] for f in e[\"to\"])"'
ok "K10 zero-downtime rollout + grace > drain+shutdown" 'py "s=dep[\"spec\"][\"strategy\"][\"rollingUpdate\"]; assert s[\"maxUnavailable\"]==0 and dep[\"spec\"][\"replicas\"]>=2 and pod[\"terminationGracePeriodSeconds\"]>30"'
ok "K11 migrations run as a pre-upgrade hook; pods do not auto-migrate" 'py "j=k[\"Job\"]; assert \"pre-upgrade\" in j[\"metadata\"][\"annotations\"][\"helm.sh/hook\"] and k[\"ConfigMap\"][\"data\"][\"CAMBIUM_AUTO_MIGRATE\"]==\"0\""'
ok "K12 secrets only via existing Secret ref (none in ConfigMap or chart)" 'py "d=k[\"ConfigMap\"][\"data\"]; assert not any(x in d for x in (\"CAMBIUM_DB\",\"CAMBIUM_S3_SECRET_KEY\",\"CAMBIUM_S3_ACCESS_KEY\",\"CAMBIUM_METRICS_TOKEN\")) and \"Secret\" not in k"'
ok "K13 Service is ClusterIP (not exposed outside the cluster)" 'py "assert k[\"Service\"][\"spec\"][\"type\"]==\"ClusterIP\""'
ok "K14 ingress (when enabled) requires TLS" '! helm template cambium $C --set ingress.enabled=true && grep -q "tls:" $T/f.yaml'
echo "k8s: $pass passed, $fail failed"; rm -rf $T; [ $fail = 0 ]
