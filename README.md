# forkstack

Stacked pull requests inside your own GitHub fork, with a terminal UI for
viewing and rearranging stacks.

## Install

Install Git, Rust/Cargo, the GitHub CLI (`gh`), and the `github/gh-stack`
extension, then authenticate with `gh auth login`. The paged `log` command also
uses `less`.

```sh
gh extension install github/gh-stack
cargo install --path .
```

During development, replace `forkstack` below with `cargo run --`.

## Run

```sh
# Preview or publish origin/main..HEAD
forkstack submit --prefix feat
forkstack submit --prefix feat --execute

# Open the terminal UI
forkstack ui --repo . --prefix feat

# Browse repositories in a parent folder (root plus immediate children)
forkstack ui --repo ..

# View the Git graph
forkstack log
forkstack log --remote origin,upstream
forkstack log --no-remotes -n 20
```

### UI keys

| Key | Action |
| --- | --- |
| `Tab` / `Shift-Tab` | Focus the next / previous pane |
| `j/k`, `Up` / `Down` | Navigate the focused pane |
| `Enter` | Activate a repository; in the graph, check out or confirm a preview |
| `m` / `M` | Move exactly one commit / a substack onto a destination |
| `z` / `Z` | Reorder one commit / a substack while preserving destination descendants |
| `l` | Preview resetting local `fs-head/*` branches to fetched `origin/*` refs |
| `o` | Preview publishing and linking the stack on `origin` |
| `u` | Preview publishing and linking the stack on `upstream` |
| `Esc` | Exit search, clear a repository filter, or cancel a graph preview |
| `/` | Filter repositories by name/path or search commits in the focused pane |
| `n`, `N` | Next / previous graph search match |
| `r` | Rescan repositories; in the graph, also refresh graph and PR links |
| `?` | Show the complete key map |
| `q` | Quit |

`l`, followed by `Enter`, uses the configured fork remote (`origin` by default)
and the already-fetched remote-tracking refs. It discards local-only commits on
matching `fs-head/*` branches, leaves branches missing from the remote unchanged,
and refuses to run when tracked files have uncommitted changes.

Opening a parent folder starts in the repository sidebar. Highlighting or filtering
repositories does not load history: press `Enter` to activate one, then `Tab` to
focus its graph. Opening a repository directly (including from a subdirectory)
activates it immediately. A direct single-repository view hides the sidebar;
narrow terminals show only the focused pane.

Each visited repository retains its graph, selected commit, scroll position, and
search for the session. Graphs load on activation and PR links refresh only on
request. While typing a search, `j/k` enter text, arrows navigate matches, and
`Enter` finishes the search; in the sidebar, press `Enter` again to activate the
highlighted result. Switching repositories cancels unconfirmed previews and is
blocked while checkout, rebase, or publishing is running.

Discovery includes Git worktrees (`.git` files), skips invalid repositories and
child directory symlinks, and does not scan grandchildren. Press `r` in the sidebar
to discover newly added or removed repositories without loading their graphs.

### Fixture

```sh
python3 scripts/create_fixture_repo.py --force
cargo run -- ui --repo fixture/repo
```

Reordering the `beta/2` substack after `alpha/2` with `Z` intentionally conflicts in
`shared.txt`, allowing the conflict UI and `git rebase --continue` workflow to
be tested.

## Design

Each commit has a stable `fs-branch` identity. Publishing creates paired
`fs-base/*` and `fs-head/*` refs, updates the complete ref set atomically, and
links the resulting pull requests into a GitHub stack.

The CLI and UI share the same Rust planner and executor. Previews are in-memory;
completed operations are always reloaded from Git.

## Tests

Hermetic Rust unit tests do not invoke Git or create repositories:

```sh
cargo test --lib
```

Real-system tests are opt-in and run separately:

```sh
cargo test --features integration-tests --test submit_integration
cargo test --features integration-tests --test workspace_integration
python3 -m unittest tests/test_fixture_integration.py
```

Run the lightweight real-repository benchmark separately with `cargo run --release --example benchmark`.
