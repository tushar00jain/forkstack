#!/usr/bin/env python3
"""Create a stack of pull requests inside your own fork.

Stacking tools (ghstack, `sl ghstack`) refuse to submit to a repository GitHub
marks as a fork, and `sl pr submit` always targets the fork's upstream parent.
GitHub itself has no such restriction: a pull request whose base repo is your
fork, with both base and head branches in that fork, shows exactly one commit's
diff. This drives that directly with `git push` and `gh`.

For each commit in <remote>/<base>..HEAD it pushes a branch to your fork and opens
a pull request based on the branch below it, so every PR is a single commit.
Re-running after an amend force-pushes the branches; existing PRs are reused.

Commits are read straight from the repository, so a Sapling stack works as is
(its commits are git commits) and nothing rewrites them.

Subcommands:
    submit  push a branch per commit and open the pull requests (the default)
    log     graph of the stacks and where the branches point
"""

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile


def run(cmd, cwd=None, capture=True):
    result = subprocess.run(
        cmd, cwd=cwd, check=True, text=True, capture_output=capture
    )
    return (result.stdout or "").strip()


def git(args, repo):
    return run(["git", *args], cwd=repo)


def parse_owner_repo(url):
    m = re.search(r"[:/]([^/:]+)/([^/]+?)(?:\.git)?$", url.strip())
    return f"{m.group(1)}/{m.group(2)}" if m else None


def commit_field(repo, rev, fmt):
    return git(["log", "-1", f"--format={fmt}", rev], repo)


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
            "--json", "number",
        ],
        cwd=repo,
    )
    prs = json.loads(out or "[]")
    return prs[0]["number"] if prs else None


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

    base_ref = f"{args.remote}/{args.base}"
    revs = git(["rev-list", "--reverse", f"{base_ref}..HEAD"], repo).split()
    if not revs:
        sys.exit(f"no commits in {base_ref}..HEAD")

    plan = []
    for i, rev in enumerate(revs, start=1):
        plan.append(
            {
                "rev": rev,
                "branch": f"{args.prefix}/{i}",
                "base": args.base if i == 1 else f"{args.prefix}/{i - 1}",
                "subject": commit_field(repo, rev, "%s"),
                "body": commit_field(repo, rev, "%b"),
            }
        )

    print(f"fork:   {fork}")
    print(f"stack:  {base_ref}..HEAD  ({len(plan)} commits)\n")
    for step in plan:
        print(f"  {step['rev'][:12]}  {step['branch']} <- base {step['base']}")
        print(f"                {step['subject']}")

    if not args.execute:
        print("\nDry run. Commands that would run:\n")
        for step in plan:
            print(
                f"  git push --force {args.remote} "
                f"{step['rev']}:refs/heads/{step['branch']}"
            )
        for step in plan:
            draft = " --draft" if args.draft else ""
            print(
                f"  gh pr create --repo {fork}{draft} --base {step['base']} "
                f"--head {step['branch']} --title {step['subject']!r} --body-file -"
            )
        print("\nRe-run with --execute to do it.")
        return

    for step in plan:
        print(f"pushing {step['branch']}")
        run(
            [
                "git", "push", "--force", args.remote,
                f"{step['rev']}:refs/heads/{step['branch']}",
            ],
            cwd=repo,
            capture=False,
        )

    for step in plan:
        number = existing_pr(fork, step["branch"], repo)
        if number:
            print(f"{step['branch']}: reusing #{number}")
        else:
            cmd = [
                "gh", "pr", "create",
                "--repo", fork,
                "--base", step["base"],
                "--head", step["branch"],
                "--title", step["subject"],
                "--body", step["body"],
            ]
            if args.draft:
                cmd.append("--draft")
            print(f"{step['branch']}: created {run(cmd, cwd=repo)}")
            number = existing_pr(fork, step["branch"], repo)
        step["number"] = number

    entries = [(s["branch"], s.get("number"), s["subject"]) for s in plan]
    for step in plan:
        if step.get("number"):
            run(
                [
                    "gh", "pr", "edit", str(step["number"]),
                    "--repo", fork,
                    "--body", stack_table(entries, step["branch"]) + step["body"],
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
            "Branches are named <prefix>/<n> from the bottom of the stack up. If the\n"
            "stack gets shorter, branches and PRs from the previous run are left\n"
            "behind; close them yourself. --base is read through its remote-tracking\n"
            "ref, so fetch first if it may be stale.\n"
        ),
    )
    p.add_argument("--repo", default=".", help="repository directory (default: cwd)")
    p.add_argument("--remote", default="origin", help="remote for your fork")
    p.add_argument(
        "--base",
        default="main",
        help="branch in the fork the stack sits on; the bottom PR targets it "
        "(default: main)",
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
