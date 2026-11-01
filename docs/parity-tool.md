Parity: 82.7 / 100

features 82.7  (43 counted, must-haves 21 of 21 done)

By area, weakest first
- scale                          0.0  (3 features)
- review                         0.0  (2 features)
- derived data                  40.0  (6 features)
- storage                       75.0  (4 features)
- checkout                      83.3  (5 features)
- client                        92.3  (5 features)
- repo                         100.0  (1 features)
- auth                         100.0  (3 features)
- commit                       100.0  (1 features)
- data model                   100.0  (3 features)
- refs                         100.0  (3 features)
- merge                        100.0  (2 features)
- history                      100.0  (3 features)
- api                          100.0  (2 features)

Missing, in build order
- [should] checkout: Virtualized checkout materialising files on read, no  (no FUSE layer; files fetched at checkout/widen time only)
- [should] derived data: Commit graph skip lists for fast ancestry, no  (merge-base is BFS)
- [should] derived data: Path history index, no  (log path filter walks first-parent chain; no index)
- [should] scale: Horizontally scalable stateless servers, no  (single node SQLite; content addressing makes it possible later)
- [should] storage: Object-store (S3-compatible) blob backend, no  (BlobStore trait in place (Fs + Mem impls); no S3 client shipped)
- [could] client: Filesystem change monitoring in client, no  (status rescans and hashes files)
- [could] derived data: Copy detection index, no
- [could] derived data: Path-based file search index, no
- [could] review: Code review web UI, no  (concept in design/review-ui.md only)
- [could] review: Semantic approval rules, no
- [could] scale: Node-level shared object daemon, no  (per-user cache dir only)
- [could] scale: Regional cache tier with identical RPCs, no

Left out on purpose (not scored)
- Git ingestion pipeline: one-way migration tooling tied to their history and repos
- Planet-scale multi-region replicated database: managed cloud infrastructure not a product feature
- Internal monitoring agents per component: their internal operations

Yours, not in the original (not scored)
- Offline commit outbox with server-side rebase on flush
