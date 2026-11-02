# Everyday workflows

## Start of the day: stay current
```sh
cmb sync          # update to the latest main; your drafts (if any) are rebased onto it
cmb status
```
Big repository? Check out only what you need, and widen later:
```sh
cmb checkout mono work --sparse services/pay/,libs/common/
cmb sparse add docs/
```

## Make a change on an unprotected branch
```sh
cmb add <files>
cmb commit -m "what and why"
```
`commit` creates a changeset and lands it. If others landed meanwhile, the server rebases
yours onto theirs and your working copy is updated to the result. If the same lines changed,
it says **kept as draft** — see *Resolve a conflict*.

## Make a change that needs review (protected branch)
```sh
cmb add <files>
cmb commit --draft -m "step 1"        # drafts stack: add more if you like
cmb commit --draft -m "step 2"
cmb review create -m "Short title"    # reviewers see the whole stack
```
Address feedback: edit, `cmb commit --draft -m "review fixes"`, then
`cmb review update <n>` (earlier approvals lapse, because the content changed).
Land it: `cmb review merge <n>` once it has enough approvals. Reviewers can approve in the
web UI or with `cmb review approve <n>`; authors cannot approve their own review.

Plain `cmb commit` on a protected branch keeps the change as a draft and tells you to open a
review.

Reviews that only change documentation, or only comments/formatting in code, can need fewer
approvals if an admin set it up (`cmb protect main 2 --trivial 1`). The review shows its kind
(`docs`, `comments` or `logic`); anything uncertain counts as `logic`.

## Resolve a conflict
```sh
cmb sync          # rebases your drafts; reports the conflicting files
# edit the files, then
cmb add <files> && cmb commit --draft -m "resolve conflict"
cmb land          # or update your review
```

## Undo something that landed
```sh
cmb log -n 20
cmb revert <changeset>    # server computes the inverse on top of the latest main and lands it
```
Later changes to the same files are kept; only the reverted change is undone.

## Find which change broke something
```sh
cmb bisect <good> <bad> --run "cargo test -q"    # exit 0 = good
```
Cambium checks out candidates in your working copy (about log2 of the range), reports the
first bad changeset, then restores your working copy. Without `--run` it prints the next
changeset to test.

## Releases and hotfixes
```sh
cmb branch release/2026.11            # cut from main's history
cmb switch release/2026.11
# fix, then
cmb add <files> && cmb commit -m "fix for 2026.11"
cmb switch main && cmb merge release/2026.11      # bring the fix back to main
```

## Browse and search
- Web UI: **Code** (files, README), **History**, blame, changesets with diffs, search.
- CLI: `cmb log [path]`, `cmb blame <path>`, `cmb search <text>`, `cmb cat <repo> <rev> <path>`.

## CI and automation
- Webhooks announce every move of `main` and `release/*` (see [administration](admin.md)).
- API-only jobs need no working copy: `GET /v1/repos/<repo>/root/main`, then trees and blobs;
  or `cmb cat`. Bisect and revert are API calls too (`/bisect`, `/revert`).
