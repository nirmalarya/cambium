# Review UI concept ("Cambium Review") — concept only, not built

This slice documents a minimal concept; parity rows for it are `no`.

Screens
- R1 Change list: table (title, author/agent, base, status, risk badge), filters "needs me", "agent-only", "touches protected paths". Empty: "Nothing waiting on you."
- R2 Change view: header (changeset id, branch, merge-base), file tree (left), unified diff (center, `diff-add-bg` / `diff-del-bg` tokens), comment threads, approve/request-changes.
- R3 Rules: approval policies per path prefix, e.g. "comments/docs-only → auto", "auth/ → two humans".

Components (spec style from design SKILL): Button (primary/secondary/ghost/danger; focus ring 2px accent), Table (sortable headers, keyboard rows), DiffView (line numbers, `aria-label` per hunk), Badge (risk low/med/high, text + colour), CommentThread.
Data: reads `/v1/repos/:r/diff`, `/log`, `/blame`, `/merge-base` — the same APIs the CLI uses.
Tokens: `design/tokens.json` (AA verified). Screenshots: none (no GUI built; the recon had no screenshots to compare — adaptation noted).
