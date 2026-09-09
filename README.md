# forkstack

Stacked pull requests inside your own fork.

`ghstack` and `sl ghstack` refuse to submit to a repository GitHub marks as a
fork, and `sl pr submit` resolves its target through the fork's parent, so it
always opens pull requests against upstream. GitHub itself has no such
restriction: a pull request whose base repo is your fork, with both base and
head branches in that fork, shows exactly one commit's diff. `forkstack` drives
that directly with `git push` and `gh`.

Useful when you don't have write access to the upstream repository, or when you
want a stack reviewable somewhere before anything goes upstream.

## Requirements

- `git`
- [`gh`](https://cli.github.com/), authenticated
- Python 3 (standard library only)

## Usage

Check out the top of the stack, then:

```
forkstack.py                                  # dry run: print the plan
forkstack.py --execute                        # push branches, open the PRs
forkstack.py --base main --prefix feat --execute
```

For each commit in `<remote>/<base>..HEAD` it preserves an existing stable
identity or assigns a new `<prefix>/<n>` identity, then opens a pull request
containing exactly that commit. A `fs-branch` commit trailer keeps the change
attached to the same pull request when commits are reordered. Existing
identities are opaque and may be mixed in one stack; `--prefix` only controls
the names assigned to commits without a trailer and is required when any such
commits exist. Remote refs are named `fs-head/<identity>` and
`fs-base/<identity>`.

Each pull request targets a private `fs-base/<prefix>/<n>` branch rather
than the preceding PR branch directly. The base ref points at the exact local
parent commit and the head ref points at the exact local change commit.
Forkstack updates every base and head ref together in one atomic,
force-with-lease push, so GitHub cannot observe a half-restacked branch set.

Execute mode also creates or updates a matching local `fs-head/<identity>`
branch for every change and configures it to track the fork's remote branch.
This makes individual stack layers available to branch-based tooling while the
`fs-branch` trailer keeps each local branch attached to the same logical change
after amendments or reordering. Forkstack refuses to overwrite a local head
branch that has diverged from its tracked remote branch.

The first execute pass records stable identities in the local commit messages,
rewrites the local stack without changing its trees, and creates PRs using the
new head/base ref namespace. PRs created by older Forkstack versions are left
untouched and can be closed manually.

After that, amend or reorder normally (for example with `git rebase -i`) and run
Forkstack again. Keep each commit's `fs-branch` trailer with that change.

| option | meaning |
| --- | --- |
| `--repo DIR` | repository to work in (default: cwd) |
| `--remote NAME` | remote for your fork (default: `origin`) |
| `--base BRANCH` | branch in the fork the stack sits on (default: `main`) |
| `--prefix NAME` | identity prefix; required only when the stack has untagged commits |
| `--no-draft` | open pull requests ready for review instead of as drafts |
| `--execute` | actually push and create; without it, nothing happens |

### Viewing the stack

```
forkstack.py log                          # local branches, HEAD and origin's refs
forkstack.py log --remote origin,upstream # other remotes are opt-in (repeatable)
forkstack.py log --no-remotes -n 20
forkstack.py log --tags                   # bring tags back
```

In the pager, `R` re-runs the walk in place and `q` quits, so watching a stack
change is one keystroke rather than quit-and-retype. (Needs less 582 or newer;
on older ones only `q` is bound.)

Only `origin` — the fork the stack is pushed to — is shown by default, and tags
are hidden. Both defaults exist because the graph is worth nothing once the
stack is buried: an upstream remote carries every contributor's ghstack refs,
and a repository that tags for CI has thousands of tags, each dragging its
commit into the graph.

`--remote` and the tag default filter both what is walked and what is decorated,
so a commit shared with an excluded remote doesn't still show that remote's
name, and a tagged commit off to the side doesn't take up a row in the graph.

## Notes

- Forkstack rewrites commits only when assigning a new stable identity. It
  preserves commit trees, authorship, timestamps, and local branch markers.
- Re-running after an amend or reorder atomically updates the existing pull
  requests with force-with-lease protection.
- Existing identities from different prefixes can be combined and reordered in
  one stack. Their branch and pull-request identities remain unchanged.
- Bodies are regenerated on every run from the commit message plus a table of
  the stack, so edits made in the GitHub UI are overwritten. Titles are updated
  from commit subjects.
- Execute mode fetches the selected remote before calculating updates. Dry-run
  mode uses the existing remote-tracking refs.
- If a stack gets shorter, branches and pull requests from earlier runs are left
  behind.
