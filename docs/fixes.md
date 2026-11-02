<!-- name-bearing URLs live only in recon.md citations -->
# Entrepreneur phase: complaints → fixes

Evidence: 17 verbatim items from 4 sources (verbatim text in recon.md Appendix B; reviews.py was run
on them — its default consumer-app themes fit poorly, so ranking below is by hand and is a
**small sample: direction, not ranking**). Never invented; every item links to its source.

## What they hate
1. **Online-only = one outage stops everyone** ("back to Subversion"). 6 items, 2 sources.
   - https://news.ycombinator.com/item?id=50082437 (server down stopped all work under SVN; git still lets you work)
   - https://news.ycombinator.com/item?id=50077008 (worse than a forge outage when the client itself is online-only)
   - https://news.ycombinator.com/item?id=50092235, https://news.ycombinator.com/item?id=50110649, https://news.ycombinator.com/item?id=50082905
   - [recon.md source 3] (reads as a reinvention of centralized source control like Subversion)
   - The builders' engineer confirmed offline commit buffering was not built: https://news.ycombinator.com/item?id=50079495
2. **Unusable by anyone else: closed, no public version.** 3 items, 3 sources.
   - https://news.ycombinator.com/item?id=50079215, [recon.md Appendix B, The New Stack], [recon.md source 2]
3. **Prior art not credited** (the server-side design of an existing open-source VCS server). 2 sources (thin).
   - [recon.md source 2], [recon.md source 5]
4. **Overkill / build-vs-buy: Perforce or git would do for most; only fits giant monorepos.** 3 items, 1 source (thin).
   - https://news.ycombinator.com/item?id=50087526, https://news.ycombinator.com/item?id=50087026, https://news.ycombinator.com/item?id=50100931
5. **Trust in a fast-built VCS: strange bugs in the system of record.** 1 item (thin).
   - https://news.ycombinator.com/item?id=50085434

## What is missing
- Offline commits / branching (item 1 above; https://news.ycombinator.com/item?id=50078055).
- A version others can run (item 2).

## What is unsolved
- Mid-size teams with agent swarms who outgrew a single git server but will never run a fleet.
- Teams that need source control to keep working through a server outage.

## Fix plan (evidence × cheapness)
| # | fix | status |
|---|---|---|
| 1 | Offline outbox: queue commits when the server is unreachable; `cmb flush` lands them via server rebase + CAS | built, then **removed** (always-online design; data-loss bugs) |
| 2 | Open source, single container, SQLite default, runs on a laptop | built (Dockerfile + compose) |
| 3 | Credit prior art plainly in the README (centralized VCS lineage, open-source monorepo servers) | built (README "Prior art") |
| 4 | Scale down: one process, no fleet; scale-out path documented, not required | built / documented |
| 5 | Trust: every blob integrity-checked on the client, 48 automated tests, WAL auditable | built |
| 6 | Per-directory ACLs (security ask) | next |
