## Parity: 96.5 / 100

features 96.5  (44 counted, must-haves 21 of 21 done)

## By area, weakest first
- review                        75.0  (2 features)
- derived data                  85.0  (6 features)
- scale                         87.5  (3 features)
- checkout                      91.7  (5 features)
- repo                         100.0  (1 features)
- auth                         100.0  (3 features)
- commit                       100.0  (1 features)
- data model                   100.0  (3 features)
- storage                      100.0  (4 features)
- refs                         100.0  (3 features)
- client                       100.0  (5 features)
- merge                        100.0  (2 features)
- history                      100.0  (3 features)
- api                          100.0  (2 features)
- migration                    100.0  (1 features)

## Missing, in build order
- [should] checkout: Virtualized checkout materialising files on read, partial  (cmb mount: FUSE lazy mount; listing from tree+sizes, blob fetched on first read into the cache; ACL-filtered; READ-ONLY (writes EROFS) - no writable overlay; Linux only; e2e V01-V04)
- [should] derived data: Commit graph skip lists for fast ancestry, partial  (generation numbers per changeset + max-gen heap walk with pruning (is_ancestor stops below gen(a); merge-base two-colour); not skip lists so worst case still walks the gap)
- [could] derived data: Copy detection index, partial  (diff reports rename/copy by identical content (blob id); no similarity (edited-copy) detection and no persisted index; engine test + e2e X04)
- [could] review: Semantic approval rules, partial  (protected branches with N approvals (0-20); author cannot self-approve; approvals stale after a new push; merge lands the exact approved head; direct commit/merge/rebase/CAS refused 403; no path-owner or content-based rules; engine tests + e2e W01-W06)
- [could] scale: Node-level shared object daemon, partial  (CAMBIUM_SHARED_CACHE: one group-shared (2770) content-addressed cache per machine, atomic writes, verified on every read; a shared directory, not a daemon; e2e Y01-Y04)

## Left out on purpose (not scored)
- Planet-scale multi-region replicated database: managed cloud infrastructure not a product feature
- Internal monitoring agents per component: their internal operations

## Yours, not in the original (not scored)
- Offline commit outbox with server-side rebase on flush
