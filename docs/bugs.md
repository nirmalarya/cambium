# Bugs
Found and fixed during build/test:
| ID | sev | steps | expected | actual | fix |
|---|---|---|---|---|---|
| B1 | S3 | offline commit with CAMBIUM_SERVER set to unreachable host while state.json held the real server | env override used | state's server always won, so outbox path never triggered | env CAMBIUM_SERVER now takes priority (crates/cambium-cli/src/main.rs `from_env`); covered by e2e F09 |
| B2 | S4 | e2e "diff between revisions" used unsupported `main~` syntax | — | passed only via fallback | test rewritten to use explicit id |
Open: none S1/S2. Known gaps (not bugs): no rate limits, no ACLs, merge conflicts don't write markers.
