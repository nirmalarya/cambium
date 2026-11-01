# Recon map: server-centric monorepo version control (clean-room study)

Studied system: an in-house, closed-source version control system described publicly by
its builders in October 2026. Its proper names appear **only** in the Sources table below
as citations; the clone is called Cambium everywhere else.

## Step 1: scope
- **What / platform:** a CLI client + network server (no GUI in the original's core; its
  companion review web app is studied only as a concept).
- **Slice:** the core loop — lay out a sparse working copy, change files, have the
  *server* create the changeset, move the branch with compare-and-swap, and answer
  history / blame / diff / merge-base queries over an API.
- **For whom:** teams whose main "developers" are always-online agents and CI jobs.

## Step 2: sources
| # | source | URL | read | what it gave |
|---|---|---|---|---|
| 1 | **Primary:** builders' post ("Redesigning source control at Anthropic", 2026-10-13) | https://www.anthropic.com/research/redesigning-source-control | fetched OK 2026-10-31 (same text is also served at https://www.anthropic.com/engineering/redesigning-source-control, fetched first) | principles, changeset data model, derived data, CAS refs, checkout protocol, cache tiers, results |
| 2 | Pragmatic Engineer, "The Pulse: Anthropic builds its own source control system" (2026-10-22) | https://blog.pragmaticengineer.com/the-pulse-anthropic-builds-its-own-source-control-system/ | fetched OK 2026-10-31 | Rust server+client, Go+React review tool, Spanner + cloud object storage, replication/backups, prior-art controversy |
| 3 | Simon Willison link post | https://simonwillison.net/2026/Oct/13/redesigning-source-control-at-anthropic/ | via search 2026-10-31 | commentary (used in entrepreneur phase) |
| 4 | Hacker News discussion (1014 comments) | https://news.ycombinator.com/item?id=50073633 | Algolia API 2026-10-31 | complaints (entrepreneur phase) |
| 5 | sunshowers.io posts on prior art | https://sunshowers.io/posts/anthropic-mononoke/ | HN index | prior-art discussion |
| 6 | Third-party independent clean-room implementation (README only, read via raw URL; not cloned, no code viewed) | https://github.com/CarterT27/scion | 2026-10-31 | how others read the design; scope comparison only |
| 7 | select 1 newsletter write-up of a third-party implementation | https://select1.dev/2026-10-16 | via search | "land" = rebase + CAS without a merge queue |
No source code of the original exists publicly; none was sought. The third-party repo's code was not read.

Ideas taken from sources 6–7 (as scope/comparison only, nothing copied): (a) a "land" step =
server rebase then CAS with retry, no merge queue — our `cmb flush` uses the same idea; (b) file
modes (executable bit, symlinks) are part of parity — **gap in Cambium**; (c) importing
existing Git history is commonly expected — kept as `skip`; (d) a production backing of a
distributed SQL store + S3-compatible object store, TLS transport, per-node daemon — recorded as
gaps; (e) benchmarking against Git on synthetic workloads — listed as next step in deploy.md. No private APIs, no accounts used.
Note: the user's brief said "changelog entry"; the primary source calls the unit a
*changeset*. This map uses "changeset".

Additions after re-reading the primary /research/ URL: object requests are per immutable
object and therefore resumable (no single large bundle response); the builders mention rate
limiting, fuzzing and hermetic simulations as quality measures; refs are discouraged except
for main/release branches. Two feature rows added (resumable per-object fetch, rate limiting).

## Step 3: surface inventory (screens → CLI commands / API surfaces)
| ID | surface | how reached | purpose | key components | states seen / required |
|---|---|---|---|---|---|
| S01 | create repo | client `init` / `POST /repos` | new repository with a root changeset and `main` ref | auth, ref CAS | ok, exists, bad name, unauthorized |
| S02 | checkout (sparse/virtual) | client `checkout` | lay out only requested directories; fetch objects on demand | tree request, blob fetch, local cache | full, sparse, empty repo, dirty-overwrite refused |
| S03 | status | client | show local edits vs checked-out changeset | fs scan, hash compare | clean, new, modified, deleted, staged |
| S04 | add/stage | client | choose what goes into the next changeset | stage list | nothing changed, path outside wc |
| S05 | commit | client → server | server creates changeset, moves ref by CAS | changeset writer, WAL, CAS | ok, nothing staged, ref moved (stale), invalid path |
| S06 | branch / switch | client → server | create ref (CAS from absent), list, move wc | refs table | exists, unknown |
| S07 | merge | server | three-way merge done server-side | merge base, merge3 | clean, conflict, up to date |
| S08 | rebase | server | replay branch onto target server-side | replay, CAS | clean, conflict, across-merge refused |
| S09 | log | server | first-parent history, path filtered | log walk / path history index | empty, filtered |
| S10 | blame | server | per-line origin | derived blame index | file missing |
| S11 | diff | server | diff between changesets, or wc overlay vs head | manifests, unified diff | no diff, prefix-limited |
| S12 | API-only access | HTTP | jobs ask questions without a working copy (cat, merge-base, changeset) | REST | not found |
| S13 | node daemon + regional cache tiers | infra | share objects across jobs on a node; tiered caches | daemon, cache servers | **not built** (see skip) |
| S14 | review UI: change list | web (companion tool) | list open changes | table | concept only |
| S15 | review UI: change view | web | diff, comments, approvals with semantic rules | diff viewer, comment thread | concept only |

## Step 4: flows
```
F01 Agent bootstraps a repo        S01 -> S02            (1 command: init)
    edge: bad token, repo exists, bad name
F02 Agent makes a change           S03 -> S04 -> S05     (3 commands)
    edge: nothing staged, branch moved meanwhile (CAS loses), bad path
F03 Feature branch and merge       S06 -> S05 -> S06 -> S07
    edge: conflicting edits, merge of already-merged
F04 Keep a branch current          S08
    edge: conflict during replay, branch contains merges
F05 Inspect history                S09 / S10 / S11
    edge: path filter, unknown rev
F06 Sparse job                     S02(--sparse) -> read files -> widen
    edge: file outside sparse set
F07 Checkout-free CI job           S12 (cat / diff / merge-base by API)
F08 Bounded local cache            S02 with cache cap; eviction
```
Happy path to land a change: 3 commands (status optional: add, commit). Number to beat: 3.

## Step 5: components
| component | variants | used by |
|---|---|---|
| Changeset writer (WAL append) | plain, merge (2 parents), replayed | S01 S05 S07 S08 |
| Ref CAS | create, move, delete | S01 S05 S06 S07 S08 |
| Blob store | local FS, object store (S3-compatible), memory | all |
| Derived data engine | manifest, blame, (path history, commit-graph skip lists in original) | S02 S09 S10 S11 |
| Merge engine | file-level, line-level 3-way | S07 S08 |
| Tree/blob protocol | client asks only for objects it wants | S02 |
| Client object cache | bounded, LRU | S02 |
| Auth / IAM | tokens; original: repo + per-directory ACLs | all |
| Review UI primitives | change table, diff viewer, comment, approval badge | S14 S15 |

## Step 6: inferred data model
```
Changeset  id (content address), parents[1..2], author, message, timestamp,
           files: [(path, blob_ptr | deleted)] relative to FIRST parent
           evidence: source 1 "Data structures" section + figure; footnote 1. confidence: high
WAL entry  seq, kind, payload — changesets are entries in a write-ahead log
           evidence: source 1. confidence: high (exact fields: guess)
Blob       content-addressed compressed file contents in object storage
           evidence: sources 1, 2. confidence: high (compression: not built)
Ref        name -> changeset, moved by compare-and-swap on one database row
           evidence: source 1. confidence: high
Derived    (kind, changeset) -> value, defined recursively from parents;
           kinds: blame, path history, commit-graph skip lists, copy detection, path search
           eager or lazy per kind. evidence: source 1. confidence: high
Tree       directory objects requested recursively from a root tree by the client
           evidence: source 1 "Scaling" section. confidence: medium (format unknown)
Principal/ACL  repo-level and per-directory permissions. evidence: source 1. confidence: medium
Review change, approval rule (semantic constraints). evidence: source 1. confidence: low
```

## Step 7: feature matrix — see features.csv

## Step 8: what cannot be cloned
- Planet-scale storage (Spanner multi-region replication, PITR) — we use SQLite/Postgres.
- Their fleet: node daemons and regional cache tiers with >99% hit rate (infra, not features).
- Their git-ingestion pipeline and migration, internal review tool, code search, merge-queue
  replacements, IAM integrations, monitoring agents.
- Reported results (200x footprint, seconds-not-minutes checkouts, 25% CI CPU, 34 → 3.7 min
  per change) are properties of their workload; they are not claims this clone can make.

## Step 9: size
13 buildable surfaces + 2 concept screens, 8 flows, 6 entities. Hard parts: correct
three-way merge/rebase server-side, derived blame that stays incremental, virtualized
(FUSE) checkouts. Size: **M** for a working slice, **L/XL** for the full fleet design.

## Summary
Core loop: sparse checkout → edit → add → server-side commit with CAS ref.
15 surfaces, 8 flows. Hardest: server merge/rebase, incremental derived data, virtual FS.
Out of scope: fleet cache tiers, global replication, review tool, git ingestion.
Next: replica-architect.

## Appendix B: research quotes (verbatim, linked) used by the entrepreneur phase
Kept here because they name the original; fixes.md paraphrases them. 17 items, 4 sources (Hacker News Algolia API, Simon Willison, Pragmatic Engineer, The New Stack), read 2026-10-31. No ratings exist for these sources.
- Hacker News, 2026-10-14, https://news.ycombinator.com/item?id=50082437: "Until the server goes down. Subversion going down at work stopped everything. At least with git you can still find work to continue on with even if you can't push."
- Hacker News, 2026-10-13, https://news.ycombinator.com/item?id=50077008: "That my thought when I saw online only, we're back to Subversion. You think you have it bad now when GitHub goes down, wait until you're stuck with an online only client."
- Hacker News, 2026-10-14, https://news.ycombinator.com/item?id=50082905: "This is why you are not an investor. (Cue McMahon meme), A marketing campaign to get eye balls across all the major tech outlets and take up headspace in everyone's trending feed? Oh yes. A potential, closed source alternative to github you can slowly wrangle your users to move to? Hell yeah. An always online centralized design requiring users to stay connected with the server for every operation? Let's go!"
- Hacker News, 2026-10-13, https://news.ycombinator.com/item?id=50078055: "It is a terrible thing. Yes, one generally has a place which is the source of truth. But distributed still gives you offline commits and branching and all the good things, for free."
- Hacker News, 2026-10-14, https://news.ycombinator.com/item?id=50092235: "Somehow, those principles remind me of SVN (the version control that was popular before git). The central server and clients can't run independently... Does not feel like progress."
- Hacker News, 2026-10-15, https://news.ycombinator.com/item?id=50110649: "Mistake #1: using a monorepo for the whole company. Why this obsession with monorepos since recent times? Mistake #2: Anthropic essentially reinvented Subversion."
- Hacker News, 2026-10-13, https://news.ycombinator.com/item?id=50079215: "It's not open sourced in case you are wondering."
- Hacker News, 2026-10-14, https://news.ycombinator.com/item?id=50087526: "Many of the large enterprises don't use a monorepo. Amazon doesn't have one. For the rest that do, the vast majority of them are going to buy some Perforce licenses instead of vibe coding their own solution. Unless you are Google/Meta where your company is large enough and its revenue supports paying a team of engineers (a lot of money) to do that, or you are Anthropic who is just vibe coding everything which can turn out to be good PR, you simply don't have the resources or can afford the risk to develop your own VCS solution. This may be inspiring but out of question for most companies."
- Hacker News, 2026-10-14, https://news.ycombinator.com/item?id=50087026: "I am about 80% certain that using well configured Perforce could work just as well while saving Anthropic some money, considering the salary of Anthropic engineers. But then Anthropic doesn't get the PR, so..."
- Hacker News, 2026-10-15, https://news.ycombinator.com/item?id=50100931: "Right, let me try and articulate my unease with this decision better. It's not really technical. I'm sure the Anthropic devs did a fantastic job. But... there's a lot of decisions that make sense for a company like Google or Meta that don't make sense for other companies. Google, Meta, Apple and a few others like them are nearly unique. They have extremely profitable quasi-monopolies that have proven impregnable to competitors, and the second price auction system they use automatically sets prices to whatever the market will bear. Thanks to that they they can pay enormous salaries, fund many high risk/low ROI projects and still have lots left over. In such an organization projects like creating your own build system, VCS, IDEs, email system and so on can be rational, even though they can't actually measure the productivity impact of a developer tool to one percentage point of precision. All this is nothing like normal business. Normal businesses face peer competitors, meaning pricing is under continuous downwards pressure. Across all industries the profit margin is only 10%, but that's an average that includes the tech sector. Industries with a lot of competition stabilize at margins around 3% e.g. supermarkets or airlines. When we look at AI labs, what do we see? They spend like they are Google, but the market they're in looks nothing like ads or SaaS. Instead they are in vicious competition selling largely undifferentiated products to price sensitive customers who display almost no brand loyalty. Quite surprisingly to those of us who got used to the FAANG and SaaS era, AI firms look more like airlines than tech firms. In normal companies the rules and culture are totally different. Decisions are driven by people with MBAs who use terms like "core competency", "outsourcing" and "margin compression". In-sourcing is rare because it's expensive and risky. If your margin is only 3% then you don't compete with your own supply chain unless it's absolutely required. Doing that without a shit ton of spreadsheets showing it's necessary is the sort of decision that will get you fired, pronto. You use things off the shelf. If nothing is available for free then you're expected to spend months in vendor negotiations trying to squeeze another 5% off the contract, and this is the kind of work valued in such a culture which is why MBAs rise to the top. Historically the software industry has avoided this because of ads, the ZIRP era, lock-in, the explosive growth of VC up to 2017, and low growth in other sectors like pharma. But that era could never last forever and seems to be ending now. A strong signal is the growth of Bending Spoons, whose business model can be summed up as buying SaaS companies run by people who can't psychologically handle a high interest rate environment and then just running them like a normal business. I realize acutely that the reason I don't run a top tier AI lab is because of this mindset, but if I were running Anthropic I'd be trying to find ways to cut costs dramatically. For a decade+ you never heard the words "tech startup" and "bankruptcy" together in a sentence, partly because VC firms forced firms to acquihire startups that weren't working out, making failure look like success. But this week Character.AI went into Chapter 11. None of us know the true figures but it looks like AI firms don't have the margins to justify acting like a Google or a successful SaaS. Corporate cultures imported from those companies risk blowing everything up."
- Hacker News, 2026-10-14, https://news.ycombinator.com/item?id=50085434: "They (allegedly) had a problem or copying their mono-repo around their network so they solved it by making the server do all the work. Sounds like a out of the frying pan and into the fire kind of solution. With the added bonus that you now have a vibe-coded VCS, so good luck when that has strange bugs."
- Hacker News, 2026-10-13, https://news.ycombinator.com/item?id=50079495: "Practically speaking, we could build local commit buffering for when the server is offline, but then the client needs to also learn how to do all of the things the server currently knows how to do (and would still never be able to answer more complex "full-graph" questions, like history, blame, or merge base ancestry). Since Claude is by far the heaviest user of Scion and is necessarily online, this just wasn't important to do."
- Hacker News, 2026-10-13, https://news.ycombinator.com/item?id=50077142: "I think offline work is increasingly impossible these days, or at least so hindered that there's no reason to optimize for it (especially with how reliable internet access is). Imagine working at Anthropic and not having access to Claude. Google has been online-only for a long time now."
- Hacker News, 2026-10-15, https://news.ycombinator.com/item?id=50099265: "Sounds like converting technical debt to monetary debt that will still ultimately need to be paid. Pay engineers and/or anthropic to fix the problems caused by anthropic. What do you do when anthropic servers are down?"
- Simon Willison blog, 2026-10-13, https://simonwillison.net/2026/Oct/13/redesigning-source-control-at-anthropic/: "The source control system is called Scion, and sounds to me like a reinvention of centralized source control along the lines of Subversion."
- Pragmatic Engineer, 2026-10-22, https://blog.pragmaticengineer.com/the-pulse-anthropic-builds-its-own-source-control-system/: "Anthropic did not give credit to Monoke in their original post, and only added a small attribution after the complaint."
- The New Stack, 2026-10-15, https://thenewstack.io/anthropic-scion-git-replacement/: "Even though Anthropic expects other companies to hit similar bottlenecks, it has no plans to release Scion or create a public version any time soon."
