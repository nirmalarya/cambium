# Cambium — CLI surface (`cmb`) and server (`cmbd`)

Binary names: **`cmb`** (client) and **`cmbd`** (server). Written fresh; no command names or help text borrowed.

| command | what happens | runs where |
|---|---|---|
| `cmb init <repo> [dir]` | creates repo + working copy | server creates root changeset and `main` |
| `cmb checkout <repo> <dir> [--branch B] [--sparse a/,b/]` | lays out only the listed prefixes | client fetches tree, then only missing blobs |
| `cmb status` | new / modified / deleted / staged | client hashes files vs last-known manifest |
| `cmb add <path>…` | stages (deletions too) | client |
| `cmb commit -m <msg>` | uploads blobs, asks server to commit on `expected` head | server: WAL append + CAS |
| `cmb branch [name]`, `cmb switch <b>` | list / create (CAS from absent) / move working copy | server |
| `cmb merge <rev>`, `cmb rebase <onto>` | server-side; working copy refreshed after | server |
| `cmb log [path] [-n N]`, `cmb blame <path>`, `cmb diff [--from R] [--to R] [path]` | queries | server |
| `cmb sparse [list|add <prefix>]` | widen the sparse set | client + server tree |
| `cmb cat <repo> <rev> <path>` | read a file with **no** working copy | server |

Output rules (voice: plain, short, specific): one line per fact; ids shortened to 12 hex chars; every error says what to do next (e.g. `hint: … run cmb sync`). Exit code 1 on any failure. No colour needed to read output.

Env: `CAMBIUM_SERVER`, `CAMBIUM_TOKEN`, `CAMBIUM_CACHE`, `CAMBIUM_CACHE_MAX`; server: `CAMBIUM_DATA`, `CAMBIUM_ADDR`.
Contrast check for the review-UI tokens: `python3 contrast.py design/tokens.json` → 11 pairs, 0 failing AA.
