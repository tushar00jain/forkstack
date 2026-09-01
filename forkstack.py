#!/usr/bin/env python3
"""Create a stack of pull requests inside your own fork.

Stacking tools (ghstack, `sl ghstack`) refuse to submit to a repository GitHub
marks as a fork, and `sl pr submit` always targets the fork's upstream parent.
GitHub itself has no such restriction: a pull request whose base repo is your
fork, with both base and head branches in that fork, shows exactly one commit's
diff. This drives that directly with `git push` and `gh`.

For each commit in <remote>/<base>..HEAD it maintains private, stable head and
base branches, so every PR is a single commit. Commit trailers keep the same PR
attached to a change when the stack is amended or reordered. All base and head
refs are updated together in one atomic, force-with-lease push.

Subcommands:
    submit  push a branch per commit and open the pull requests (the default)
    log     graph of the stacks and where the branches point
"""

import argparse
import dataclasses
import json
import os
import re
import signal
import subprocess
import sys
import tempfile


def run(cmd, cwd=None, capture=True, input=None, env=None):
    result = subprocess.run(
        cmd,
        cwd=cwd,
        check=True,
        text=True,
        capture_output=capture,
        input=input,
        env=env,
    )
    return (result.stdout or "").strip()


def git(args, repo, **kwargs):
    return run(["git", *args], cwd=repo, **kwargs)


def parse_owner_repo(url):
    m = re.search(r"[:/]([^/:]+)/([^/]+?)(?:\.git)?$", url.strip())
    return f"{m.group(1)}/{m.group(2)}" if m else None


def commit_field(repo, rev, fmt):
    return git(["log", "-1", f"--format={fmt}", rev], repo)


IDENTITY_TRAILER = "fs-branch"
IDENTITY_RE = re.compile(rf"^{IDENTITY_TRAILER}:\s*(\S+)\s*$", re.MULTILINE)


@dataclasses.dataclass
class StackCommit:
    rev: str
    branch: str
    subject: str
    body: str
    message: str
    identity_added: bool = False
    number: int | None = None
    pr: dict | None = None

    @property
    def head_branch(self):
        return f"fs-head/{self.branch}"

    @property
    def base_branch(self):
        return f"fs-base/{self.branch}"


def identity_from_message(message):
    matches = IDENTITY_RE.findall(message)
    if len(matches) > 1:
        raise RuntimeError(f"multiple {IDENTITY_TRAILER} trailers in one commit")
    return matches[0] if matches else None


def add_identity(message, branch):
    if identity_from_message(message):
        return message
    return f"{message.rstrip()}\n\n{IDENTITY_TRAILER}: {branch}\n"


def body_without_identity(body):
    return IDENTITY_RE.sub("", body).rstrip()


def remote_identities(repo, remote, prefix):
    out = git(
        [
            "for-each-ref",
            "--format=%(refname:strip=4)",
            f"refs/remotes/{remote}/fs-head/{prefix}/",
        ],
        repo,
    )
    return set(out.splitlines()) if out else set()


def assign_branches(repo, revs, remote, prefix):
    records = []
    claimed = set()
    previous = None
    for rev in revs:
        parents = commit_field(repo, rev, "%P").split()
        if len(parents) != 1:
            raise RuntimeError(
                f"commit {rev[:12]} has {len(parents)} parents; "
                "forkstack requires a linear stack"
            )
        if previous is not None and parents[0] != previous:
            raise RuntimeError("forkstack requires a linear stack")
        previous = rev
        message = commit_field(repo, rev, "%B")
        branch = identity_from_message(message)
        if branch:
            if not branch.startswith(f"{prefix}/"):
                raise RuntimeError(
                    f"commit {rev[:12]} belongs to {branch!r}, not prefix {prefix!r}"
                )
            if branch in claimed:
                raise RuntimeError(f"duplicate {IDENTITY_TRAILER}: {branch}")
            try:
                git(["check-ref-format", "--branch", branch], repo)
            except subprocess.CalledProcessError as error:
                raise RuntimeError(f"invalid {IDENTITY_TRAILER}: {branch}") from error
            claimed.add(branch)
        records.append((rev, message, branch))

    known = remote_identities(repo, remote, prefix) | claimed
    numbered = []
    branch_re = re.compile(rf"^{re.escape(prefix)}/([0-9]+)$")
    for branch in known:
        if match := branch_re.match(branch):
            numbered.append(int(match.group(1)))
    next_number = max(numbered, default=0) + 1

    plan = []
    for position, (rev, message, branch) in enumerate(records, start=1):
        identity_added = branch is None
        if branch is None:
            positional = f"{prefix}/{position}"
            if positional in known and positional not in claimed:
                branch = positional
            else:
                while f"{prefix}/{next_number}" in known:
                    next_number += 1
                branch = f"{prefix}/{next_number}"
                next_number += 1
            claimed.add(branch)
            known.add(branch)
        plan.append(
            StackCommit(
                rev=rev,
                branch=branch,
                subject=commit_field(repo, rev, "%s"),
                body=body_without_identity(commit_field(repo, rev, "%b")),
                message=message,
                identity_added=identity_added,
            )
        )
    return plan


def rewrite_with_identities(repo, plan):
    old_head = git(["rev-parse", "HEAD"], repo)
    new_parent = None
    rewritten = {}
    for step in plan:
        old_rev = step.rev
        old_parent = git(["rev-parse", f"{old_rev}^"], repo)
        parent = new_parent or old_parent
        message = add_identity(step.message, step.branch)
        if parent == old_parent and message == step.message:
            new_rev = old_rev
        else:
            fields = commit_field(
                repo,
                old_rev,
                "%T%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI",
            ).split("\0")
            (
                tree,
                author_name,
                author_email,
                author_date,
                committer_name,
                committer_email,
                committer_date,
            ) = fields
            env = {
                **os.environ,
                "GIT_AUTHOR_NAME": author_name,
                "GIT_AUTHOR_EMAIL": author_email,
                "GIT_AUTHOR_DATE": author_date,
                "GIT_COMMITTER_NAME": committer_name,
                "GIT_COMMITTER_EMAIL": committer_email,
                "GIT_COMMITTER_DATE": committer_date,
            }
            new_rev = git(
                ["commit-tree", tree, "-p", parent],
                repo,
                input=f"{message.rstrip()}\n",
                env=env,
            )
        step.rev = new_rev
        step.message = message
        step.body = body_without_identity("\n".join(message.splitlines()[1:]))
        new_parent = new_rev
        rewritten[old_rev] = new_rev

    # Preserve local branch markers inside the stack, not just the checked-out
    # top branch. This is useful when a stack has names such as `draft` and
    # `draft4` pointing at intermediate and top commits.
    for old_rev, new_rev in rewritten.items():
        if old_rev == new_rev:
            continue
        refs = git(
            [
                "for-each-ref",
                "--format=%(refname)",
                "--points-at",
                old_rev,
                "refs/heads/",
            ],
            repo,
        ).splitlines()
        for ref in refs:
            git(["update-ref", ref, new_rev, old_rev], repo)

    if git(["rev-parse", "HEAD"], repo) == old_head and new_parent != old_head:
        git(["update-ref", "HEAD", new_parent, old_head], repo)
    return plan


def push_refs(repo, remote, updates, label):
    if not updates:
        return
    print(f"pushing {label}")
    run(
        [
            "git",
            "push",
            "--atomic",
            "--force-with-lease",
            remote,
            *[f"{rev}:refs/heads/{branch}" for branch, rev in updates.items()],
        ],
        cwd=repo,
        capture=False,
    )


def stack_table(entries, current):
    lines = ["", "---", "Stack (top to bottom):"]
    for branch, number, subject in reversed(entries):
        mark = "->" if branch == current else ""
        ref = f"#{number}" if number else branch
        lines.append(f"- {mark} {ref} {subject}")
    lines += ["---", ""]
    return "\n".join(lines)


def existing_pr(fork, branch, repo):
    out = run(
        [
            "gh", "pr", "list",
            "--repo", fork,
            "--head", branch,
            "--state", "open",
            "--json", "number,baseRefName,title,body",
        ],
        cwd=repo,
    )
    prs = json.loads(out or "[]")
    return prs[0] if prs else None


# less quits with the first character of a bound quit action's extra string as
# its exit status, which is how R gets back here to re-run the walk. Needs less
# 582 or newer to read the key file as source; on anything older LESSKEYIN is
# ignored, R keeps its usual repaint meaning and only q works.
REFRESH_KEY = "R"
REFRESH_STATUS = ord(REFRESH_KEY)
LESSKEY = f"#command\n{REFRESH_KEY} quit {REFRESH_KEY}\n"

# -S truncates rather than wraps: a graph row is the graph columns plus the
# decorations plus the subject, and a wrapped row leaves its continuation
# without graph columns, so the bars stop lining up. -+F cancels a -F in the
# caller's $LESS, which would otherwise quit before R could be pressed
# whenever the graph happened to fit on one screen. -X keeps the graph on
# screen after the last quit, the way git's pager leaves it.
LESS = [
    "less", "-R", "-S", "-+F", "-X",
    f"-Ps?e(END)  .[{REFRESH_KEY}] refresh   [q] quit",
]

# -X is also why a refresh has to clear: without the alternate screen less
# leaves the graph behind when it exits, and the next one paints underneath it.
CLEAR = "\033[H\033[2J"


def page(cmd, repo, env):
    """Run cmd under less. True if the user asked for another walk."""
    # ^C is the pager's while it is up, which is what git does for its own
    # pager too: less takes it as "interrupt the read", and neither we nor the
    # walk may die on it and leave less drawing over a returned shell prompt.
    walk = subprocess.Popen(
        cmd, cwd=repo, stdout=subprocess.PIPE, start_new_session=True
    )
    previous = signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
        status = subprocess.run(LESS, stdin=walk.stdout, env=env).returncode
    finally:
        signal.signal(signal.SIGINT, previous)
        # Closing our end lets a walk that outlives the pager die of SIGPIPE
        # instead of blocking on a pipe nobody is reading.
        walk.stdout.close()
        walk.wait()
    return status == REFRESH_STATUS


def cmd_log(args):
    cmd = ["git", "log", "--graph", "--oneline", "--decorate"]
    remotes = [r for spec in args.remote or [] for r in spec.split(",") if r]
    if args.no_remotes:
        remotes = []
    elif not remotes:
        remotes = ["origin"]
    # --glob picks what to walk; --decorate-refs keeps the labels in step,
    # otherwise a commit shared with an excluded remote still shows its name.
    refs = ["refs/heads/*", "HEAD"]
    refs += [f"refs/remotes/{r}/*" for r in remotes]
    cmd += ["--branches", "HEAD"]
    cmd += [f"--glob=refs/remotes/{r}/*" for r in remotes]
    if args.tags:
        refs.append("refs/tags/*")
        cmd.append("--tags")
    cmd += [f"--decorate-refs={ref}" for ref in refs]
    if args.max_count:
        cmd.append(f"-{args.max_count}")
    cmd += args.git_args
    if not sys.stdout.isatty():
        subprocess.run(cmd, cwd=args.repo, check=False)
        return

    # Paging by hand rather than through git's own pager: the walk has to be
    # re-run to refresh, and git cannot hand its pager's exit status back.
    cmd.insert(2, "--color=always")
    with tempfile.NamedTemporaryFile("w", suffix=".lesskey") as keys:
        keys.write(LESSKEY)
        keys.flush()
        env = {**os.environ, "LESSKEYIN": keys.name}
        try:
            while page(cmd, args.repo, env):
                print(CLEAR, end="", flush=True)
        except KeyboardInterrupt:
            pass


def cmd_submit(args):
    repo = args.repo
    fork = parse_owner_repo(git(["remote", "get-url", args.remote], repo))
    if not fork:
        sys.exit(f"could not read owner/name from the {args.remote!r} remote URL")

    if args.execute:
        print(f"fetching {args.remote}")
        run(["git", "fetch", args.remote], cwd=repo, capture=False)

    base_ref = f"{args.remote}/{args.base}"
    revs = git(["rev-list", "--reverse", f"{base_ref}..HEAD"], repo).split()
    if not revs:
        sys.exit(f"no commits in {base_ref}..HEAD")

    plan = assign_branches(repo, revs, args.remote, args.prefix)

    print(f"fork:   {fork}")
    print(f"stack:  {base_ref}..HEAD  ({len(plan)} commits)\n")
    for step in plan:
        marker = "  (new identity)" if step.identity_added else ""
        print(
            f"  {step.rev[:12]}  {step.branch} ({step.head_branch}) "
            f"<- base {step.base_branch}{marker}"
        )
        print(f"                {step.subject}")

    if not args.execute:
        print("\nDry run. The execute pass would:\n")
        for step in plan:
            if step.identity_added:
                print(
                    f"  add {IDENTITY_TRAILER}: {step.branch} to "
                    f"{step.rev[:12]}"
                )
            print(f"  atomically update {step.base_branch} and {step.head_branch}")
        print("\nRe-run with --execute to do it.")
        return

    if any(step.identity_added for step in plan):
        print(f"recording stable {IDENTITY_TRAILER} trailers")
        rewrite_with_identities(repo, plan)

    for step in plan:
        step.pr = existing_pr(fork, step.head_branch, repo)
        if step.pr:
            step.number = step.pr["number"]
            if step.pr["baseRefName"] != step.base_branch:
                raise RuntimeError(
                    f"PR #{step.number} targets {step.pr['baseRefName']!r}; "
                    f"expected {step.base_branch!r}"
                )

    # The PR base and head names are stable, while their targets are the exact
    # clean commits from the local stack. Updating every ref in one atomic push
    # prevents GitHub from observing a half-restacked branch set.
    updates = {}
    root = git(["rev-parse", base_ref], repo)
    for index, step in enumerate(plan):
        updates[step.base_branch] = root if index == 0 else plan[index - 1].rev
        updates[step.head_branch] = step.rev

    push_refs(repo, args.remote, updates, "PR base and head refs")

    for step in plan:
        if step.number:
            print(f"{step.branch}: reusing #{step.number}")
        else:
            cmd = [
                "gh", "pr", "create",
                "--repo", fork,
                "--base", step.base_branch,
                "--head", step.head_branch,
                "--title", step.subject,
                "--body", step.body,
            ]
            if args.draft:
                cmd.append("--draft")
            url = run(cmd, cwd=repo)
            print(f"{step.branch}: created {url}")
            match = re.search(r"/pull/([0-9]+)/?$", url)
            if not match:
                raise RuntimeError(f"could not read pull request number from {url!r}")
            step.number = int(match.group(1))

    entries = [(s.branch, s.number, s.subject) for s in plan]
    for step in plan:
        if step.number:
            run(
                [
                    "gh", "pr", "edit", str(step.number),
                    "--repo", fork,
                    "--title", step.subject,
                    "--body", stack_table(entries, step.branch) + step.body,
                ],
                cwd=repo,
            )
    print(f"\n{len(plan)} pull requests in {fork}.")


def build_parser():
    top = argparse.ArgumentParser(
        description="Create a stack of one-commit pull requests inside your own fork.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="`submit` is the default, so its arguments work with no subcommand.",
    )
    sub = top.add_subparsers(dest="command")

    lg = sub.add_parser(
        "log",
        help="git log --graph of the stacks and branches",
        description="Graph of the repository's commits and where the branches point. "
        "In the pager, R re-runs the walk and q quits.",
    )
    lg.add_argument("--repo", default=".", help="repository directory (default: cwd)")
    lg.add_argument(
        "--remote",
        action="append",
        metavar="NAME",
        help="show only this remote's refs (repeatable, or comma-separated, "
        "default: origin); local branches and HEAD are always shown",
    )
    lg.add_argument(
        "--no-remotes", action="store_true", help="show no remote refs at all"
    )
    lg.add_argument(
        "--tags",
        action="store_true",
        help="also walk and decorate tags (hidden by default: a repository that "
        "tags for CI buries the stack otherwise)",
    )
    lg.add_argument(
        "-n", "--max-count", type=int, metavar="N", help="limit to N commits"
    )
    lg.add_argument(
        "git_args", nargs="*", metavar="GIT_ARG", help="extra arguments for git log"
    )
    lg.set_defaults(func=cmd_log)

    p = sub.add_parser(
        "submit",
        help="push branches and open the pull requests",
        description="Create a stack of one-commit pull requests inside your own fork.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "New changes are assigned <prefix>/<n> identities. Those identities stay\n"
            "with the changes when the stack is reordered. If the stack gets\n"
            "shorter, old branches and PRs are left behind; close them yourself.\n"
        ),
    )
    p.add_argument("--repo", default=".", help="repository directory (default: cwd)")
    p.add_argument("--remote", default="origin", help="remote for your fork")
    p.add_argument(
        "--base",
        default="main",
        help="branch in the fork the stack sits on (default: main)",
    )
    p.add_argument(
        "--prefix",
        default="stack",
        help="branch name prefix, one per stack (default: stack)",
    )
    p.add_argument("--draft", action="store_true", default=True)
    p.add_argument(
        "--no-draft",
        dest="draft",
        action="store_false",
        help="open PRs ready for review",
    )
    p.add_argument(
        "--execute",
        action="store_true",
        help="actually push branches and create PRs (default: print the plan)",
    )
    p.set_defaults(func=cmd_submit)
    return top


def main():
    parser = build_parser()
    argv = sys.argv[1:]
    if not argv or argv[0] not in ("submit", "log", "-h", "--help"):
        argv.insert(0, "submit")
    args = parser.parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    main()
