# Concepts

Five ideas explain almost everything Cambium does.

## 1. The server is the source of truth, and you are always online
Your machine does not hold the whole history. It holds a **working copy** of the files you
work on (all of them, or a **sparse** subset). History, merges, rebases, blame and search run
on the server and are answered in milliseconds, whatever the size of the repository.
There is no offline commit: if the server is unreachable, `cmb commit` says so and keeps
your change staged.

## 2. Work is changesets, not branches
A **changeset** is one recorded change: the files it adds, modifies or deletes, compared to
its parent, plus author, time and message. Changesets are immutable and have an id
(`3fa1c0…`).

There are **no feature branches**. Only two kinds of branch exist:
- `main`, where everyone's work lands;
- `release/NAME`, cut from `main` for releases and hotfixes.

Instead of a feature branch you have **drafts**: changesets you created but have not landed
yet. They live on the server (nobody else's view changes) and are tracked by your working copy.

| In git you would… | In Cambium you… |
|---|---|
| `git checkout -b feature`, commit, push, open PR | `cmb commit --draft`, `cmb review create` |
| `git pull --rebase` | `cmb sync` (the server rebases your drafts) |
| merge the PR | `cmb review merge` (or `cmb land` on an unprotected branch) |
| `git commit && git push` on main | `cmb commit` |
| `git revert`, `git bisect` | `cmb revert`, `cmb bisect` (computed on the server) |

## 3. Landing is lock-free: the server rebases for you
When you land (`cmb commit` or `cmb land`), the server takes your changeset, **rebases it
onto the current head** of the branch, and moves the branch with a single atomic
compare-and-swap. If someone landed a moment earlier, it simply rebases again. There is no
merge queue and no "your branch is out of date" error. If your change really conflicts with
what landed (same lines edited), it stays a **draft** and you resolve it with `cmb sync`.

Testing happens **after** landing (webhooks tell CI immediately) and before deploy, instead of
in a queue in front of `main`.

## 4. Fetching is by tree, and only what changed
Every folder is stored as an immutable **tree** identified by a hash of its content. A
checkout walks from the root tree down and fetches only folders and files it does not have;
an unchanged folder is skipped entirely. Optional **cache tiers** (`cmbd daemon` on a build
machine, `cmbd edge` in a region) serve these immutable objects close to you.

## 5. Permissions are per directory, and enforced everywhere
Rules say who may `read`, `write` or `admin` under a path prefix (`services/pay`, …). Hidden
files never appear: not in checkouts, history, search, blame, diffs, reviews, the web UI, or
error messages. **Protected branches** only change through reviews with enough approvals;
reviews that only touch docs or comments can need fewer.

## Vocabulary
| Term | Meaning |
|---|---|
| changeset | one recorded change; immutable, has an id |
| draft | a changeset not yet on a branch |
| land | put changesets on a branch (server rebase + atomic move) |
| head | the latest changeset of a branch |
| working copy | your local files plus `.cambium/` metadata |
| sparse checkout | a working copy of only some folders |
| review | a request to land a changeset on a protected branch |
| tree / blob | a stored folder / file content, identified by hash |
