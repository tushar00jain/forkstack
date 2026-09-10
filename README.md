# forkstack

Stacked pull requests inside your own GitHub fork, with a terminal UI for
viewing and rearranging stacks.

## Install

Install Git, Rust/Cargo, and the GitHub CLI (`gh`), then authenticate with
`gh auth login`. The paged `log` command also uses `less`.

```sh
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

# View the Git graph
forkstack log
forkstack log --remote origin,upstream
forkstack log --no-remotes -n 20
```

### UI keys

| Key | Action |
| --- | --- |
| `Up` / `Down` | Select |
| `Enter` | Check out or preview a move |
| `m` / `M` | Move one commit / a substack |
| `a` | Apply a move preview |
| `f` / `F` | Preview / execute a publish |
| `Esc` | Cancel a preview |
| `/`, `n`, `N` | Search / next / previous |
| `R` | Refresh |
| `q` | Quit |

### Fixture

```sh
python3 scripts/create_fixture_repo.py --force
cargo run -- ui --repo fixture/repo
```

Moving the `beta/2` substack after `alpha/2` intentionally conflicts in
`shared.txt`, allowing the conflict UI and `git rebase --continue` workflow to
be tested.

## Design

Each commit has a stable `fs-branch` identity. Publishing creates paired
`fs-base/*` and `fs-head/*` refs so every pull request contains one commit, and
updates the complete ref set atomically.

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
python3 -m unittest tests/test_fixture_integration.py
python3 -m unittest tests/test_python_submit_integration.py
```

Run the lightweight real-repository benchmark separately with `cargo run --release --example benchmark`.
