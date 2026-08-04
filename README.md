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

For each commit in `<remote>/<base>..HEAD` it pushes a branch `<prefix>/<n>` to
your fork and opens a pull request based on the branch below it, so every pull
request contains exactly one commit. The bottom one targets `<base>`, the branch
in the fork your stack sits on.

| option | meaning |
| --- | --- |
| `--repo DIR` | repository to work in (default: cwd) |
| `--remote NAME` | remote for your fork (default: `origin`) |
| `--base BRANCH` | branch in the fork the stack sits on (default: `main`) |
| `--prefix NAME` | branch name prefix, one per stack (default: `stack`) |
| `--no-draft` | open pull requests ready for review instead of as drafts |
| `--execute` | actually push and create; without it, nothing happens |

### Viewing the stack

```
forkstack.py log                    # graph of every ref except tags
forkstack.py log --remote origin    # only this remote's refs (repeatable)
forkstack.py log --no-remotes -n 20
forkstack.py log --tags             # bring tags back
```

Tags are hidden by default. A repository that tags for CI has thousands of them,
and each one drags its commit into the graph, so the stack you came to look at
gets buried; pass `--tags` when you actually want them.

`--remote` and the tag default filter both what is walked and what is decorated,
so a commit shared with an excluded remote doesn't still show that remote's
name, and a tagged commit off to the side doesn't take up a row in the graph.

## Notes

- Commits are read straight from the repository, so a Sapling stack works as is
  (its commits are git commits) and nothing rewrites them.
- Re-running after an amend force-pushes the branches and updates the existing
  pull requests rather than opening duplicates.
- Bodies are regenerated on every run from the commit message plus a table of
  the stack, so edits made in the GitHub UI are overwritten. Titles are only set
  when a pull request is created.
- `--base` is read through its remote-tracking ref; fetch first if it may be
  stale.
- If a stack gets shorter, branches and pull requests from earlier runs are left
  behind.
