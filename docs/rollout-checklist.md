# Cambium rollout checklist

## Before the pilot (owner: platform team)
- [ ] Internal DNS name + certificate from the internal CA
- [ ] IdP app registration (audience, groups claim) — IAM team
- [ ] Postgres instance with TLS, backups and PITR; `CAMBIUM_DB_TLS=require`
- [ ] Object-store bucket + credentials in the secret store; run S3 backend against it
- [ ] Image pushed to your registry and signed; digest pinned in values
- [ ] `helm install` in a staging namespace; NetworkPolicy enforced by the CNI verified
- [ ] Prometheus scraping via ServiceMonitor; alerts from slos.md
- [ ] Log shipping and audit export into the SIEM
- [ ] Restore drill in staging
- [ ] Security review / pen test
- [ ] Load test on production-grade hardware
- [ ] CI pipeline (.github/workflows/ci.yml or internal equivalent) green

## Pilot
- [ ] One team, git import with `cmbd import-git` (tools image job), mirror kept read-only
- [ ] Tune `CAMBIUM_RATE`/`CAMBIUM_BURST`, `CAMBIUM_PG_POOL`, replica count
- [ ] Weekly review of errors, latency and support tickets

## GA
- [ ] SLOs met for 30 days in pilot
- [ ] Runbook rehearsed by on-call
- [ ] Token rotation schedule in place
